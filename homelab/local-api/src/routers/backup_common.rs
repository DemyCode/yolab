// Shared primitives used by backup_run.rs (BackupRun reconciler), restore_run.rs
// (RestoreRun reconciler), backups.rs (S3 enable/status/snapshot browsing), and
// apps.rs (setup_namespace_backup at install time). Pulled out of backups.rs so
// the run-level orchestration (backup_run.rs/restore_run.rs) doesn't have to
// depend on the HTTP-handler module, and vice versa.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "yolab");

pub(crate) async fn kubectl_apply(manifest: &str) -> anyhow::Result<()> {
    crate::kubectl::apply(manifest).await
}

pub(crate) async fn kubectl_get_secret(name: &str, ns: &str) -> Option<HashMap<String, String>> {
    crate::kubectl::get_secret(name, ns).await
}

pub(crate) async fn kubectl_apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
) -> anyhow::Result<()> {
    crate::kubectl::apply_secret(name, ns, data, &[MANAGED_BY]).await
}

pub(crate) fn random_hex(bytes: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct S3StorageInfo {
    pub bucket_name: String,
    pub endpoint: String,
    #[allow(dead_code)]
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[allow(dead_code)]
    pub created_at: String,
}

/// Collapses a (possibly restore-mangled) PVC name back to its original identity.
///
/// Restores rename the live PVC to `volsync-emergency-restore-{id}-dest`. Without this,
/// every subsequent restore of an already-restored PVC mints a longer, uglier name and a
/// brand new ReplicationSource/restic-secret/S3-path — fragmenting backup history and
/// leaving the previous RS behind as an orphaned duplicate. Names derived from this id
/// (RS name, restic secret name, S3 repo path) stay stable across any number of restores.
pub(crate) fn canonical_pvc_id(pvc_name: &str) -> String {
    let mut id = pvc_name;
    while let Some(stripped) = id
        .strip_prefix("volsync-emergency-restore-")
        .and_then(|s| s.strip_suffix("-dest"))
    {
        id = stripped;
    }
    id.to_string()
}

// ── Master backup config ──────────────────────────────────────────────────────

pub(crate) const MASTER_SECRET: &str = "yolab-backup-config";
pub(crate) const MASTER_NS: &str = "kube-system";
// Secret name per PVC: "<pvc-name>-restic" in the PVC's namespace.
pub(crate) const RESTIC_SECRET_SUFFIX: &str = "-restic";

pub(crate) const EXCLUDED_NS: &[&str] = &[
    "kube-system",
    "rook-ceph",
    "velero",
    "volsync-system",
    "cattle-system",
    "local-path-storage",
    "default",
];

#[derive(Clone)]
pub(crate) struct BackupConfig {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket: String,
    /// Full S3 endpoint URL e.g. https://s3.eu-central-003.backblazeb2.com
    pub endpoint: String,
    /// restic encryption password — generated once, never sent to yolab-external.
    /// Recoverable only via GET /api/backups/recovery-key (see backups.rs).
    pub restic_password: String,
}

impl BackupConfig {
    pub fn restic_repo(&self, path: &str) -> String {
        format!(
            "s3:{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            path
        )
    }

    pub async fn unlock(&self, path: &str) {
        restic_unlock(&self.restic_repo(path), &self.restic_password, &self.access_key_id, &self.secret_access_key).await;
    }
}

/// Removes any restic lock that `restic` itself determines is stale (its owning
/// process/host is no longer alive) from the given repo. Without this, a mover pod
/// killed mid-sync or a local-api restart mid-backup leaves a lock that blocks every
/// future backup/restore against that repo forever — restic's own staleness check
/// (not `--remove-all`) is what keeps this safe to call unconditionally before any
/// operation, since it never touches a lock that's still actively held.
pub(crate) async fn restic_unlock(repo: &str, password: &str, key_id: &str, secret_key: &str) {
    let out = Command::new("restic")
        .args(["unlock"])
        .env("RESTIC_REPOSITORY", repo)
        .env("RESTIC_PASSWORD", password)
        .env("AWS_ACCESS_KEY_ID", key_id)
        .env("AWS_SECRET_ACCESS_KEY", secret_key)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let msg = String::from_utf8_lossy(&o.stdout);
            if !msg.trim().is_empty() {
                tracing::info!("restic unlock ({repo}): {}", msg.trim());
            }
        }
        Ok(o) => tracing::debug!("restic unlock ({repo}): {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => tracing::debug!("restic unlock ({repo}): {e}"),
    }
}

/// Reads the existing master backup config secret. Returns `None` if backups
/// have never been enabled — unlike `ensure_master_config`, never provisions
/// anything or calls yolab-external; safe to call from the reconcile loop on
/// every tick.
pub(crate) async fn read_master_config() -> Option<BackupConfig> {
    let data = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await?;
    let restic_password = data.get("restic_password").cloned().unwrap_or_default();
    if restic_password.is_empty() {
        return None;
    }
    Some(BackupConfig {
        access_key_id: data.get("access_key_id").cloned().unwrap_or_default(),
        secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
        bucket: data.get("bucket").cloned().unwrap_or_default(),
        endpoint: data.get("endpoint").cloned().unwrap_or_default(),
        restic_password,
    })
}

/// Reads the master config, provisioning B2 storage via yolab-external and generating
/// a fresh restic password on first use. Used only by the explicit enable-backups flow;
/// the reconcile loop uses `read_master_config` so a not-yet-configured cluster never
/// triggers provisioning as a side effect of a scheduling tick.
pub(crate) async fn ensure_master_config(url: &str, token: &str) -> anyhow::Result<BackupConfig> {
    if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
        let restic_password = data.get("restic_password").cloned().unwrap_or_default();
        if !restic_password.is_empty() {
            return Ok(BackupConfig {
                access_key_id: data.get("access_key_id").cloned().unwrap_or_default(),
                secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
                bucket: data.get("bucket").cloned().unwrap_or_default(),
                endpoint: data.get("endpoint").cloned().unwrap_or_default(),
                restic_password,
            });
        }
        // Old secret exists (rclone era) but lacks restic_password — add it.
        let restic_password = random_hex(32);
        kubectl_apply_secret(
            MASTER_SECRET,
            MASTER_NS,
            &[
                ("access_key_id", data.get("access_key_id").map(|s| s.as_str()).unwrap_or("")),
                ("secret_access_key", data.get("secret_access_key").map(|s| s.as_str()).unwrap_or("")),
                ("bucket", data.get("bucket").map(|s| s.as_str()).unwrap_or("")),
                ("endpoint", data.get("endpoint").map(|s| s.as_str()).unwrap_or("")),
                ("restic_password", &restic_password),
            ],
        ).await?;
        return Ok(BackupConfig {
            access_key_id: data.get("access_key_id").cloned().unwrap_or_default(),
            secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
            bucket: data.get("bucket").cloned().unwrap_or_default(),
            endpoint: data.get("endpoint").cloned().unwrap_or_default(),
            restic_password,
        });
    }

    let resp = http_client()
        .post(format!("{url}/storage/s3"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?;
    let s3: S3StorageInfo = resp.json().await.map_err(|e| anyhow::anyhow!(e))?;

    // Encryption password generated locally — never sent to yolab-external.
    let restic_password = random_hex(32);

    kubectl_apply_secret(
        MASTER_SECRET,
        MASTER_NS,
        &[
            ("access_key_id", &s3.access_key_id),
            ("secret_access_key", &s3.secret_access_key),
            ("bucket", &s3.bucket_name),
            ("endpoint", &s3.endpoint),
            ("restic_password", &restic_password),
        ],
    )
    .await?;

    Ok(BackupConfig {
        access_key_id: s3.access_key_id,
        secret_access_key: s3.secret_access_key,
        bucket: s3.bucket_name,
        endpoint: s3.endpoint,
        restic_password,
    })
}

/// Re-fetches S3 credentials from yolab-external and overwrites the cached copy,
/// keeping the same restic_password (it's local-only and never came from yolab-external
/// in the first place). `ensure_master_config` caches indefinitely once a secret exists,
/// so a B2 key rotation on yolab-external's side would otherwise 403 every backup/restore
/// forever with no path back — this is that path, exposed as POST /api/backups/credentials/refresh.
pub(crate) async fn refresh_master_config(url: &str, token: &str) -> anyhow::Result<BackupConfig> {
    let resp = http_client()
        .post(format!("{url}/storage/s3"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?;
    let s3: S3StorageInfo = resp.json().await.map_err(|e| anyhow::anyhow!(e))?;

    // Preserve the existing restic_password — only the S3-side credentials rotate.
    let restic_password = match kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
        Some(data) if !data.get("restic_password").cloned().unwrap_or_default().is_empty() =>
            data["restic_password"].clone(),
        _ => random_hex(32),
    };

    kubectl_apply_secret(
        MASTER_SECRET,
        MASTER_NS,
        &[
            ("access_key_id", &s3.access_key_id),
            ("secret_access_key", &s3.secret_access_key),
            ("bucket", &s3.bucket_name),
            ("endpoint", &s3.endpoint),
            ("restic_password", &restic_password),
        ],
    ).await?;

    Ok(BackupConfig {
        access_key_id: s3.access_key_id,
        secret_access_key: s3.secret_access_key,
        bucket: s3.bucket_name,
        endpoint: s3.endpoint,
        restic_password,
    })
}

/// Annotate a namespace to allow VolSync movers to run with elevated privileges.
/// Required so the restic mover can call lchown to restore original file ownership.
pub(crate) async fn annotate_ns_privileged_movers(ns: &str) {
    let _ = Command::new("kubectl")
        .args(["annotate", "namespace", ns,
               "volsync.backube/privileged-movers=true", "--overwrite"])
        .output().await;
}

/// Create (or update) the per-PVC restic secret in its namespace.
/// Contains the full repo URL so VolSync knows where to read/write.
/// Keyed by the canonical PVC id so the repo path (and thus backup history) survives restores.
pub(crate) async fn ensure_restic_secret(ns: &str, pvc: &str, cfg: &BackupConfig) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(pvc);
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");
    let repo = cfg.restic_repo(&format!("volsync/{ns}/{cid}"));
    kubectl_apply_secret(
        &secret_name,
        ns,
        &[
            ("RESTIC_REPOSITORY", &repo),
            ("RESTIC_PASSWORD", &cfg.restic_password),
            ("AWS_ACCESS_KEY_ID", &cfg.access_key_id),
            ("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key),
        ],
    )
    .await
}

// ── PVC discovery ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct PvcInfo {
    pub namespace: String,
    pub name: String,
}

/// Every PVC in a `yolab.io/managed=true` namespace — deliberately the SAME set the
/// cluster-metadata export (backup_run.rs's `snapshot_cluster`) walks for its k8s/
/// catalog export, by construction rather than by convention. These two inventories
/// used to be computed independently (this one: "every non-excluded namespace"; the
/// export: "every `yolab.io/managed=true` namespace") and could disagree — a PVC in
/// an unlabeled namespace would have its data pushed to B2 but never appear in
/// catalog.json, making it unrestorable even though it was faithfully backed up.
/// Filtering on the same label here closes that gap structurally: nothing this
/// function returns can ever be outside what the export captures.
pub(crate) async fn list_user_pvcs() -> anyhow::Result<Vec<PvcInfo>> {
    let managed: std::collections::HashSet<String> = list_managed_namespaces().await.into_iter().collect();

    let out = Command::new("kubectl")
        .args(["get", "pvc", "-A", "-o", "json"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "kubectl get pvc: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let items = v["items"].as_array().cloned().unwrap_or_default();

    Ok(items
        .into_iter()
        .filter_map(|item| {
            let ns = item["metadata"]["namespace"].as_str()?.to_string();
            let name = item["metadata"]["name"].as_str()?.to_string();
            if EXCLUDED_NS.contains(&ns.as_str()) || !managed.contains(&ns) {
                return None;
            }
            // VolSync creates its own PVCs (restic cache, restore destinations) inside the
            // same user namespace as the real app data. Without this, each backup run would
            // pick up the previous run's cache PVC and back *that* up too, spawning a new
            // ReplicationSource and cache PVC every time — an unbounded, self-amplifying
            // chain. App PVCs are always named after the app (e.g. "filebrowser-data"),
            // never with this prefix.
            if name.starts_with("volsync-") {
                return None;
            }
            Some(PvcInfo { namespace: ns, name })
        })
        .collect())
}

/// Every namespace labeled `yolab.io/managed=true` — the set the cluster-metadata
/// export (k8s objects + catalog.json) walks. Exposed so callers that need to check
/// whether a PVC's namespace will actually be captured (see the doc comment on
/// `list_user_pvcs`) can compare the two sets instead of assuming they match.
pub(crate) async fn list_managed_namespaces() -> Vec<String> {
    let out = Command::new("kubectl")
        .args(["get", "namespaces", "-l", "yolab.io/managed=true",
               "-o", "jsonpath={.items[*].metadata.name}"])
        .output().await;
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect(),
        Err(_) => Vec::new(),
    }
}

// ── VolSync ReplicationSource ─────────────────────────────────────────────────

/// Ensures a ReplicationSource exists for `pvc`. When `trigger_now` is true, stamps a fresh
/// manual trigger so VolSync starts a sync immediately.
///
/// VolSync RSes have NO independent schedule — they only run when the backup explicitly
/// triggers them. This makes every backup a single coherent point-in-time: VolSync runs,
/// completes, then the cluster snapshot is taken.
///
/// When `trigger_now` is false and the RS already exists, this is a no-op — the existing RS
/// (and any in-progress manual trigger set by the backup) is left untouched.
pub(crate) async fn ensure_replication_source(pvc: &PvcInfo, trigger_now: bool) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(&pvc.name);
    let rs_name = format!("volsync-{cid}");
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");

    // Self-healing path: only create if missing — never overwrite a live manual trigger.
    if !trigger_now {
        let exists = crate::kubectl::run(&["get", "replicationsource", &rs_name,
                                          "-n", &pvc.namespace]).await.is_ok();
        if exists {
            return Ok(());
        }
    }

    let trigger = if trigger_now {
        serde_json::json!({
            "manual": chrono::Utc::now().format("backup-%Y%m%d%H%M%S").to_string()
        })
    } else {
        serde_json::json!({})
    };
    // copyMethod Direct: read from the live PVC without snapshotting first.
    // More reliable than Snapshot which requires a working VolumeSnapshotClass —
    // if the CSI plugin is unhealthy the snapshot never completes and the backup
    // silently stalls forever. Direct is safe for most apps; the window of
    // inconsistency (files written mid-backup) is the same as tar-over-a-live-fs.
    let manifest = serde_json::json!({
        "apiVersion": "volsync.backube/v1alpha1",
        "kind": "ReplicationSource",
        "metadata": {
            "name": rs_name,
            "namespace": pvc.namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "spec": {
            "sourcePVC": pvc.name,
            "trigger": trigger,
            "restic": {
                "repository": secret_name,
                "pruneIntervalDays": 7,
                "retain": { "daily": 7, "weekly": 4, "monthly": 12 },
                "copyMethod": "Direct",
                "cacheStorageClassName": "yolab-cephfs",
                // Run as root so the mover can read files regardless of the app's uid.
                // App containers vary: some write as uid 0, some as uid 1000, some as
                // arbitrary UIDs. Running as 1000 silently skips root-owned files; running
                // as root reads everything and restores correct ownership on the way back.
                "moverSecurityContext": {
                    "runAsUser": 0,
                    "runAsGroup": 0,
                    "fsGroup": 0
                }
            }
        }
    });
    kubectl_apply(&manifest.to_string()).await
}

/// Raw `kubectl get replicationsource -A -o json` — callers match against this
/// themselves (see backup_run.rs's polling loop) rather than this module
/// prescribing a single "is it synced" answer.
pub(crate) async fn get_replication_sources() -> serde_json::Value {
    let out = Command::new("kubectl")
        .args(["get", "replicationsource", "-A", "-o", "json"])
        .output()
        .await;
    match out {
        Ok(o) => serde_json::from_slice(&o.stdout).unwrap_or(serde_json::json!({"items": []})),
        Err(_) => serde_json::json!({"items": []}),
    }
}

pub(crate) fn hours_since(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_hours())
}

// ── Cluster-metadata export helpers ───────────────────────────────────────────

/// Strips K8s-assigned runtime fields from exported objects so they can be
/// cleanly re-applied to a new or existing cluster without conflicts.
///
/// Fields removed:
///   - metadata: resourceVersion, uid, creationTimestamp, generation,
///     managedFields, selfLink, ownerReferences, finalizers
///   - annotations: last-applied, deployment revision (set by K8s controllers)
///   - status: entirely (always rebuilt by controllers after apply)
///   - Service spec.clusterIP / clusterIPs: cluster-specific; a pinned value
///     blocks re-apply if the IP is already taken or out of range
///
/// Service-account-token Secrets are dropped entirely — they are cluster-local
/// credentials that cannot be re-used across clusters.
pub(crate) fn sanitize_k8s_items_for_backup(items: &[serde_json::Value]) -> Vec<serde_json::Value> {
    const META_DROP: &[&str] = &[
        "resourceVersion", "uid", "creationTimestamp", "generation",
        "managedFields", "selfLink", "ownerReferences", "finalizers",
    ];
    const ANN_DROP: &[&str] = &[
        "kubectl.kubernetes.io/last-applied-configuration",
        "deployment.kubernetes.io/revision",
        "control-plane.alpha.kubernetes.io/leader",
    ];
    items.iter().filter_map(|item| {
        let kind = item["kind"].as_str().unwrap_or("");
        if kind == "Secret"
            && item["type"].as_str() == Some("kubernetes.io/service-account-token")
        {
            return None;
        }
        let mut obj = item.clone();
        if let Some(meta) = obj["metadata"].as_object_mut() {
            for &f in META_DROP { meta.remove(f); }
            if let Some(anns) = meta.get_mut("annotations").and_then(|a| a.as_object_mut()) {
                for &f in ANN_DROP { anns.remove(f); }
                if anns.is_empty() { meta.remove("annotations"); }
            }
        }
        if let Some(m) = obj.as_object_mut() { m.remove("status"); }
        if kind == "Service" {
            if let Some(spec) = obj["spec"].as_object_mut() {
                spec.remove("clusterIP");
                spec.remove("clusterIPs");
            }
        }
        Some(obj)
    }).collect()
}

pub(crate) fn parse_capacity_bytes(s: &str) -> u64 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("Ti") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Gi") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Mi") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Ki") { return n.trim().parse::<u64>().unwrap_or(0) * 1024; }
    s.parse::<u64>().unwrap_or(0)
}

/// Sums the requested capacity of existing app PVCs in the given namespaces — the
/// space an in-place restore will free (each PVC is deleted before being recreated)
/// and can therefore reuse. VolSync's own `volsync-*` cache PVCs are excluded, same
/// as everywhere else, since they aren't part of the restored app data.
pub(crate) async fn reclaimable_pvc_bytes(namespaces: &[String]) -> u64 {
    let mut total = 0u64;
    for ns in namespaces {
        let out = Command::new("kubectl")
            .args(["get", "pvc", "-n", ns, "-o", "json"])
            .output().await;
        let pvcs = out.ok()
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default();
        for item in pvcs {
            let name = item["metadata"]["name"].as_str().unwrap_or("");
            if name.starts_with("volsync-") { continue; }
            let cap = item["spec"]["resources"]["requests"]["storage"].as_str().unwrap_or("0");
            total = total.saturating_add(parse_capacity_bytes(cap));
        }
    }
    total
}

// ── Restore primitives ────────────────────────────────────────────────────────

/// Deletes a ReplicationDestination once its data has been applied, without letting
/// VolSync's own controller tear down the destination PVC in the process.
///
/// The destination PVC is now the live, actively-mounted data volume for the app — by this
/// point we've already patched deployments to use it. Kubectl's `--cascade=orphan` only
/// controls the API server's ownerReference-based garbage collection; it does NOT stop
/// VolSync's own finalizer-driven reconcile-on-delete cleanup, which (observed live) can
/// still delete the PVC it created for this RD regardless of that flag. Stripping the RD's
/// finalizers first means it's removed immediately, before VolSync's controller ever gets a
/// chance to react to the deletion at all — leftover VolSync-internal staging objects (temp
/// PVC/snapshot) are an acceptable trade-off; destroying the live app's data is not.
pub(crate) async fn delete_replication_destination_without_touching_pvc(name: &str, namespace: &str) {
    let _ = Command::new("kubectl")
        .args([
            "patch", "replicationdestination", name, "-n", namespace,
            "--type=merge", "-p", r#"{"metadata":{"finalizers":[]}}"#,
        ])
        .output()
        .await;
    let _ = Command::new("kubectl")
        .args(["delete", "replicationdestination", name, "-n", namespace, "--ignore-not-found"])
        .output()
        .await;
}

pub(crate) async fn scale_deployment(namespace: &str, name: &str, replicas: u32) -> anyhow::Result<()> {
    let out = Command::new("kubectl")
        .args([
            "scale",
            "deployment",
            name,
            "-n",
            namespace,
            &format!("--replicas={replicas}"),
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "scale deployment {} failed: {}",
            name,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Creates the PVC a restore will write into, owned by us rather than VolSync.
///
/// Every RD manifest uses copyMethod "Direct" with this PVC passed as `destinationPVC`,
/// instead of copyMethod "Snapshot" (which was the original design). Per VolSync's own
/// docs, a Snapshot-copyMethod destination PVC is explicitly internal to VolSync's own
/// bookkeeping — it can be recreated/replaced on subsequent reconciles — which is
/// fundamentally incompatible with what this code does with it (repoint a Deployment to
/// use it as a permanent, ongoing data volume). That mismatch, not any particular delete
/// ordering, is what caused the restored PVC to vanish out from under running pods, observed
/// live and reproduced twice even after two different delete-ordering fixes. With Direct +
/// destinationPVC, the PVC is ours from creation onward; VolSync only ever writes into it.
pub(crate) async fn ensure_destination_pvc(
    name: &str,
    namespace: &str,
    capacity: &str,
    storage_class: &str,
    access_mode: &str,
) -> anyhow::Result<()> {
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "spec": {
            "accessModes": [access_mode],
            "storageClassName": storage_class,
            "resources": { "requests": { "storage": capacity } }
        }
    });
    kubectl_apply(&manifest.to_string()).await
}

/// Deletes `pvc_name` if present and waits (bounded) for it to actually finish deleting.
///
/// Restoring in place — recreating a PVC under the exact same name the app already uses,
/// instead of a differently-named "-dest" PVC that then needs deployments repointed at it —
/// means the caller can never have two PVC objects share a name even momentarily. The caller
/// must have already scaled down whatever was mounting the old PVC; this just waits out the
/// window between issuing the delete and the pvc-protection finalizer actually clearing.
pub(crate) async fn delete_pvc_and_wait(namespace: &str, pvc_name: &str) -> anyhow::Result<()> {
    let _ = Command::new("kubectl")
        .args(["delete", "pvc", pvc_name, "-n", namespace, "--wait=false", "--ignore-not-found"])
        .output()
        .await;
    for _ in 0..40 {
        let exists = Command::new("kubectl")
            .args(["get", "pvc", pvc_name, "-n", namespace])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !exists {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    anyhow::bail!(
        "timed out waiting for PVC {namespace}/{pvc_name} to finish deleting — \
         a pod may still be mounting it"
    );
}

/// Polls until CephFilesystem yolab-fs reports phase=Ready (filesystem is mountable).
/// Returns immediately if already ready. Creating a PVC on a down CephFS hangs the
/// CSI provisioner indefinitely, so DR restore gates on this before any PVC work.
/// Bounded by `timeout_secs`, which the caller should keep well inside the owning
/// RestoreRun phase's deadline.
pub(crate) async fn wait_for_cephfs_ready(timeout_secs: u64) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let out = Command::new("kubectl")
            .args(["get", "cephfilesystem", "yolab-fs", "-n", "rook-ceph",
                   "-o", "jsonpath={.status.phase}"])
            .output().await;
        let phase = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => String::new(),
        };
        if phase == "Ready" {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "timed out after {timeout_secs}s waiting for CephFilesystem to be Ready \
                 (current: {phase:?}) — surviving OSDs may need more time to recover"
            );
        }
        tracing::info!("dr: CephFilesystem phase={phase:?} — waiting for Ready");
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_pvc_id_passes_through_plain_names() {
        assert_eq!(canonical_pvc_id("gitea-data"), "gitea-data");
    }

    #[test]
    fn canonical_pvc_id_strips_one_restore_layer() {
        assert_eq!(
            canonical_pvc_id("volsync-emergency-restore-gitea-data-dest"),
            "gitea-data"
        );
    }

    #[test]
    fn canonical_pvc_id_strips_nested_restore_layers() {
        // Re-restoring an already-restored PVC must collapse back to the same id
        // so RS/secret/S3-path names stay stable instead of growing each time.
        let mangled =
            "volsync-emergency-restore-volsync-emergency-restore-gitea-data-dest-dest";
        assert_eq!(canonical_pvc_id(mangled), "gitea-data");
    }

    #[test]
    fn random_hex_length_and_charset() {
        let h = random_hex(16);
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
