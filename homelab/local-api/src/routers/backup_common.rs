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
        restic_unlock(
            &self.restic_repo(path),
            &self.restic_password,
            &self.access_key_id,
            &self.secret_access_key,
        )
        .await;
    }
}

/// Every restic invocation in the crate goes through this (or [`restic_timeout`] for
/// the one call — the cluster-backup upload in backup_run.rs — that legitimately needs
/// longer than administrative commands do) and gets `kill_on_drop(true)`. Mirrors
/// `kubectl.rs`'s `KUBECTL_TIMEOUT`/`run_bounded` for the identical reason: every one of
/// these calls used to run unbounded inside a reconcile loop or an HTTP handler, so a
/// restic process wedged on a stalled B2 connection hung its caller forever — for
/// backup_run.rs/restore_run.rs that meant the entire backup+restore reconciler, since
/// both are driven from the one serial `reconcile_tick` chain.
const RESTIC_TIMEOUT: Duration = Duration::from_secs(180);

/// Run a restic subcommand against `cfg`'s repo, bounded by [`RESTIC_TIMEOUT`].
pub(crate) async fn restic(
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    restic_timeout(repo, cfg, args, RESTIC_TIMEOUT).await
}

/// Same as [`restic`] with an explicit timeout — for the cluster-backup upload, which
/// runs under its own much longer phase-deadline budget (see backup_run.rs's
/// `step_snapshotting`) rather than this module's default.
pub(crate) async fn restic_timeout(
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    let work = Command::new("restic")
        .args(args)
        .kill_on_drop(true)
        .env("RESTIC_REPOSITORY", repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output();
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "restic {}: timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            )
        })?
        .map_err(|e| anyhow::anyhow!("restic {}: {e}", args.join(" ")))
}

/// Removes any restic lock that `restic` itself determines is stale (its owning
/// process/host is no longer alive) from the given repo. Without this, a mover pod
/// killed mid-sync or a local-api restart mid-backup leaves a lock that blocks every
/// future backup/restore against that repo forever — restic's own staleness check
/// (not `--remove-all`) is what keeps this safe to call unconditionally before any
/// operation, since it never touches a lock that's still actively held.
pub(crate) async fn restic_unlock(repo: &str, password: &str, key_id: &str, secret_key: &str) {
    let work = Command::new("restic")
        .args(["unlock"])
        .kill_on_drop(true)
        .env("RESTIC_REPOSITORY", repo)
        .env("RESTIC_PASSWORD", password)
        .env("AWS_ACCESS_KEY_ID", key_id)
        .env("AWS_SECRET_ACCESS_KEY", secret_key)
        .output();
    let out = tokio::time::timeout(RESTIC_TIMEOUT, work).await;
    match out {
        Ok(Ok(o)) if o.status.success() => {
            let msg = String::from_utf8_lossy(&o.stdout);
            if !msg.trim().is_empty() {
                tracing::info!("restic unlock ({repo}): {}", msg.trim());
            }
        }
        Ok(Ok(o)) => tracing::debug!(
            "restic unlock ({repo}): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Ok(Err(e)) => tracing::debug!("restic unlock ({repo}): {e}"),
        Err(_) => tracing::debug!(
            "restic unlock ({repo}): timed out after {}s",
            RESTIC_TIMEOUT.as_secs()
        ),
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
                (
                    "access_key_id",
                    data.get("access_key_id").map(|s| s.as_str()).unwrap_or(""),
                ),
                (
                    "secret_access_key",
                    data.get("secret_access_key")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                ),
                (
                    "bucket",
                    data.get("bucket").map(|s| s.as_str()).unwrap_or(""),
                ),
                (
                    "endpoint",
                    data.get("endpoint").map(|s| s.as_str()).unwrap_or(""),
                ),
                ("restic_password", &restic_password),
            ],
        )
        .await?;
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
        Some(data)
            if !data
                .get("restic_password")
                .cloned()
                .unwrap_or_default()
                .is_empty() =>
        {
            data["restic_password"].clone()
        }
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

/// Annotate a namespace to allow VolSync movers to run with elevated privileges.
/// Required so the restic mover can call lchown to restore original file ownership.
pub(crate) async fn annotate_ns_privileged_movers(ns: &str) {
    let _ = crate::kubectl::run(&[
        "annotate",
        "namespace",
        ns,
        "volsync.backube/privileged-movers=true",
        "--overwrite",
    ])
    .await;
}

/// Create (or update) the per-PVC restic secret in its namespace.
/// Contains the full repo URL so VolSync knows where to read/write.
/// Keyed by the canonical PVC id so the repo path (and thus backup history) survives restores.
pub(crate) async fn ensure_restic_secret(
    ns: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<()> {
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
    let managed: std::collections::HashSet<String> =
        list_managed_namespaces().await.into_iter().collect();

    let v = crate::kubectl::get_json(&["get", "pvc", "-A", "-o", "json"]).await?;
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
            Some(PvcInfo {
                namespace: ns,
                name,
            })
        })
        .collect())
}

/// Every namespace labeled `yolab.io/managed=true` — the set the cluster-metadata
/// export (k8s objects + catalog.json) walks. Exposed so callers that need to check
/// whether a PVC's namespace will actually be captured (see the doc comment on
/// `list_user_pvcs`) can compare the two sets instead of assuming they match.
pub(crate) async fn list_managed_namespaces() -> Vec<String> {
    crate::kubectl::run(&[
        "get",
        "namespaces",
        "-l",
        "yolab.io/managed=true",
        "-o",
        "jsonpath={.items[*].metadata.name}",
    ])
    .await
    .map(|s| s.split_whitespace().map(String::from).collect())
    .unwrap_or_default()
}

// ── VolSync ReplicationSource ─────────────────────────────────────────────────

/// Ensures a ReplicationSource exists for `pvc`. When `trigger_now` is true, stamps a fresh
/// manual trigger so VolSync starts a sync immediately.
///
/// VolSync RSes have NO independent schedule — they only run when explicitly triggered by
/// a concrete `trigger.manual` value, and otherwise idle in "Waiting for manual trigger".
/// This makes every backup a single coherent point-in-time: VolSync runs, completes, then
/// the cluster snapshot is taken.
///
/// When `trigger_now` is false and the RS already exists, this is a no-op — the existing RS
/// (and any in-progress manual trigger set by the backup) is left untouched. When it does NOT
/// yet exist (first call for a PVC — install time, or the hourly self-heal reconciler picking
/// up something new), it's still created with a concrete one-off manual value rather than an
/// empty trigger, so it syncs exactly once and then idles rather than looping continuously.
pub(crate) async fn ensure_replication_source(
    pvc: &PvcInfo,
    trigger_now: bool,
) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(&pvc.name);
    let rs_name = format!("volsync-{cid}");
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");

    // Self-healing path: only create if missing — never overwrite a live manual trigger.
    if !trigger_now {
        let exists =
            crate::kubectl::run(&["get", "replicationsource", &rs_name, "-n", &pvc.namespace])
                .await
                .is_ok();
        if exists {
            return Ok(());
        }
    }

    // A completely empty trigger ({}) does NOT mean "sync once and wait" — VolSync
    // treats it as "no schedule, no manual value to wait for", which in practice makes
    // the mover resync continuously in a tight back-to-back loop with no pause at all.
    // Observed live: a freshly-installed app's RS (trigger_now=false, first creation)
    // sat there spinning up a new mover pod every ~20s indefinitely, since nothing ever
    // gave it a concrete manual value to settle on until the next real backup ran.
    // Every RS this function creates — trigger_now true or false — must get a concrete
    // manual value so VolSync syncs exactly once and then idles in "Waiting for manual
    // trigger" until the next real backup changes it.
    let trigger = serde_json::json!({
        "manual": chrono::Utc::now()
            .format(if trigger_now { "backup-%Y%m%d%H%M%S" } else { "init-%Y%m%d%H%M%S" })
            .to_string()
    });
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
    crate::kubectl::get_json(&["get", "replicationsource", "-A", "-o", "json"])
        .await
        .unwrap_or(serde_json::json!({"items": []}))
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
        "resourceVersion",
        "uid",
        "creationTimestamp",
        "generation",
        "managedFields",
        "selfLink",
        "ownerReferences",
        "finalizers",
    ];
    const ANN_DROP: &[&str] = &[
        "kubectl.kubernetes.io/last-applied-configuration",
        "deployment.kubernetes.io/revision",
        "control-plane.alpha.kubernetes.io/leader",
    ];
    items
        .iter()
        .filter_map(|item| {
            let kind = item["kind"].as_str().unwrap_or("");
            if kind == "Secret"
                && item["type"].as_str() == Some("kubernetes.io/service-account-token")
            {
                return None;
            }
            let mut obj = item.clone();
            if let Some(meta) = obj["metadata"].as_object_mut() {
                for &f in META_DROP {
                    meta.remove(f);
                }
                if let Some(anns) = meta.get_mut("annotations").and_then(|a| a.as_object_mut()) {
                    for &f in ANN_DROP {
                        anns.remove(f);
                    }
                    if anns.is_empty() {
                        meta.remove("annotations");
                    }
                }
            }
            if let Some(m) = obj.as_object_mut() {
                m.remove("status");
            }
            if kind == "Service" {
                if let Some(spec) = obj["spec"].as_object_mut() {
                    spec.remove("clusterIP");
                    spec.remove("clusterIPs");
                }
            }
            Some(obj)
        })
        .collect()
}

pub(crate) fn parse_capacity_bytes(s: &str) -> u64 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("Ti") {
        return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024;
    }
    if let Some(n) = s.strip_suffix("Gi") {
        return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024;
    }
    if let Some(n) = s.strip_suffix("Mi") {
        return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024;
    }
    if let Some(n) = s.strip_suffix("Ki") {
        return n.trim().parse::<u64>().unwrap_or(0) * 1024;
    }
    s.parse::<u64>().unwrap_or(0)
}

/// Sums the requested capacity of existing app PVCs in the given namespaces — the
/// space an in-place restore will free (each PVC is deleted before being recreated)
/// and can therefore reuse. VolSync's own `volsync-*` cache PVCs are excluded, same
/// as everywhere else, since they aren't part of the restored app data.
pub(crate) async fn reclaimable_pvc_bytes(namespaces: &[String]) -> u64 {
    let mut total = 0u64;
    for ns in namespaces {
        let pvcs = crate::kubectl::get_json(&["get", "pvc", "-n", ns, "-o", "json"])
            .await
            .ok()
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default();
        for item in pvcs {
            let name = item["metadata"]["name"].as_str().unwrap_or("");
            if name.starts_with("volsync-") {
                continue;
            }
            let cap = item["spec"]["resources"]["requests"]["storage"]
                .as_str()
                .unwrap_or("0");
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
pub(crate) async fn delete_replication_destination_without_touching_pvc(
    name: &str,
    namespace: &str,
) {
    let _ = crate::kubectl::run(&[
        "patch",
        "replicationdestination",
        name,
        "-n",
        namespace,
        "--type=merge",
        "-p",
        r#"{"metadata":{"finalizers":[]}}"#,
    ])
    .await;
    let _ = crate::kubectl::run(&[
        "delete",
        "replicationdestination",
        name,
        "-n",
        namespace,
        "--ignore-not-found",
    ])
    .await;
}

pub(crate) async fn scale_deployment(
    namespace: &str,
    name: &str,
    replicas: u32,
) -> anyhow::Result<()> {
    crate::kubectl::run(&[
        "scale",
        "deployment",
        name,
        "-n",
        namespace,
        &format!("--replicas={replicas}"),
    ])
    .await?;
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

// NOTE: the old `delete_pvc_and_wait` lived here and blocked for up to 120s per PVC.
// It was the single biggest reason a restore `step()` could outlive its 90s lease and
// stall BackupRun reconciliation alongside it. Restores now issue a non-blocking delete
// and wait it out ACROSS reconcile ticks via the per-volume `Deleting` sub-phase — see
// `advance_volume` in restore_run.rs. Nothing in a step blocks on the cluster any more.

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
        let mangled = "volsync-emergency-restore-volsync-emergency-restore-gitea-data-dest-dest";
        assert_eq!(canonical_pvc_id(mangled), "gitea-data");
    }

    #[test]
    fn random_hex_length_and_charset() {
        let h = random_hex(16);
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn random_hex_does_not_repeat_itself() {
        // These become restic passwords and secret suffixes; a constant would be
        // catastrophic and is exactly what a broken RNG wiring looks like.
        assert_ne!(random_hex(16), random_hex(16));
    }

    // ── restic_repo ───────────────────────────────────────────────────────────

    fn cfg() -> BackupConfig {
        BackupConfig {
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            bucket: "yolab-backups".into(),
            endpoint: "https://s3.eu-central-003.backblazeb2.com".into(),
            restic_password: "pw".into(),
        }
    }

    #[test]
    fn restic_repo_builds_an_s3_url() {
        assert_eq!(
            cfg().restic_repo("volsync/yolab-ok/ok-data"),
            "s3:https://s3.eu-central-003.backblazeb2.com/yolab-backups/volsync/yolab-ok/ok-data"
        );
    }

    /// A trailing slash on the endpoint would produce `//bucket`, which restic
    /// treats as a *different* repository — the backup would silently start
    /// writing somewhere the restore never looks.
    #[test]
    fn restic_repo_never_doubles_the_separator() {
        let mut c = cfg();
        c.endpoint = "https://s3.example.com/".into();
        assert_eq!(
            c.restic_repo("p"),
            "s3:https://s3.example.com/yolab-backups/p"
        );
        c.endpoint = "https://s3.example.com///".into();
        assert_eq!(
            c.restic_repo("p"),
            "s3:https://s3.example.com/yolab-backups/p"
        );
    }

    // ── hours_since ───────────────────────────────────────────────────────────

    #[test]
    fn hours_since_measures_elapsed_time() {
        let t = (chrono::Utc::now() - chrono::Duration::hours(30)).to_rfc3339();
        assert_eq!(hours_since(&t), Some(30));
    }

    #[test]
    fn hours_since_truncates_toward_zero() {
        let t = (chrono::Utc::now() - chrono::Duration::minutes(119)).to_rfc3339();
        assert_eq!(hours_since(&t), Some(1));
    }

    /// A future timestamp means the clock moved backwards (NTP correction, or a
    /// laptop resuming from suspend). Callers compare with `>= 24`, so a negative
    /// value correctly reads as "recent" rather than triggering a backup.
    #[test]
    fn hours_since_goes_negative_for_a_future_timestamp() {
        let t = (chrono::Utc::now() + chrono::Duration::hours(5)).to_rfc3339();
        assert!(hours_since(&t).is_some_and(|h| h < 0));
    }

    #[test]
    fn hours_since_returns_none_for_unparseable_input() {
        assert_eq!(hours_since(""), None);
        assert_eq!(hours_since("never"), None);
        assert_eq!(hours_since("2026-01-01"), None); // date only, not RFC3339
    }

    #[test]
    fn hours_since_accepts_the_z_suffix_kubernetes_emits() {
        assert!(hours_since("2020-01-01T00:00:00Z").is_some());
    }

    // ── parse_capacity_bytes ──────────────────────────────────────────────────

    #[test]
    fn capacity_parses_binary_suffixes() {
        assert_eq!(parse_capacity_bytes("1Ki"), 1024);
        assert_eq!(parse_capacity_bytes("1Mi"), 1024 * 1024);
        assert_eq!(parse_capacity_bytes("5Gi"), 5 * 1024 * 1024 * 1024);
        assert_eq!(parse_capacity_bytes("2Ti"), 2 * 1024u64.pow(4));
    }

    #[test]
    fn capacity_parses_a_bare_byte_count() {
        assert_eq!(parse_capacity_bytes("1024"), 1024);
    }

    #[test]
    fn capacity_tolerates_surrounding_whitespace() {
        assert_eq!(parse_capacity_bytes("  5Gi "), 5 * 1024 * 1024 * 1024);
        assert_eq!(parse_capacity_bytes("5 Gi"), 5 * 1024 * 1024 * 1024);
    }

    /// This feeds "will the restore fit?" arithmetic. Zero understates free
    /// space, which makes the check refuse rather than proceed — the safe way to
    /// be wrong.
    #[test]
    fn an_unparseable_capacity_reads_as_zero() {
        assert_eq!(parse_capacity_bytes(""), 0);
        assert_eq!(parse_capacity_bytes("lots"), 0);
        assert_eq!(parse_capacity_bytes("Gi"), 0);
        assert_eq!(parse_capacity_bytes("-5Gi"), 0);
        assert_eq!(parse_capacity_bytes("1.5Gi"), 0); // fractional: K8s never emits this
    }

    /// Decimal SI suffixes are not handled; they must not be silently read as the
    /// bare-number branch, which would understate by a factor of a billion.
    #[test]
    fn decimal_suffixes_are_not_mistaken_for_byte_counts() {
        assert_eq!(parse_capacity_bytes("5G"), 0);
        assert_eq!(parse_capacity_bytes("5M"), 0);
    }

    // ── sanitize_k8s_items_for_backup ─────────────────────────────────────────

    #[test]
    fn sanitize_strips_cluster_assigned_metadata() {
        let items = vec![serde_json::json!({
            "kind": "Deployment",
            "metadata": {
                "name": "app",
                "namespace": "yolab-app",
                "resourceVersion": "12345",
                "uid": "abc-def",
                "creationTimestamp": "2026-01-01T00:00:00Z",
                "generation": 4,
                "managedFields": [{"manager": "kubectl"}],
                "selfLink": "/apis/apps/v1/…",
                "ownerReferences": [{"kind": "ReplicaSet"}],
                "finalizers": ["foregroundDeletion"],
            },
            "spec": {"replicas": 1},
            "status": {"readyReplicas": 1},
        })];
        let out = sanitize_k8s_items_for_backup(&items);
        let meta = out[0]["metadata"].as_object().unwrap();

        for dropped in [
            "resourceVersion",
            "uid",
            "creationTimestamp",
            "generation",
            "managedFields",
            "selfLink",
            "ownerReferences",
            "finalizers",
        ] {
            assert!(!meta.contains_key(dropped), "{dropped} must not survive");
        }
        // Identity and desired state must survive — that is the whole payload.
        assert_eq!(meta["name"], serde_json::json!("app"));
        assert_eq!(meta["namespace"], serde_json::json!("yolab-app"));
        assert_eq!(out[0]["spec"]["replicas"], serde_json::json!(1));
        assert!(
            out[0].get("status").is_none(),
            "status is always rebuilt on apply"
        );
    }

    #[test]
    fn sanitize_drops_controller_written_annotations_but_keeps_ours() {
        let items = vec![serde_json::json!({
            "kind": "Deployment",
            "metadata": {"name": "app", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "{…}",
                "deployment.kubernetes.io/revision": "7",
                "yolab.io/app-id": "gitea",
            }},
        })];
        let anns = &sanitize_k8s_items_for_backup(&items)[0]["metadata"]["annotations"];
        assert!(anns
            .get("kubectl.kubernetes.io/last-applied-configuration")
            .is_none());
        assert!(anns.get("deployment.kubernetes.io/revision").is_none());
        assert_eq!(anns["yolab.io/app-id"], serde_json::json!("gitea"));
    }

    #[test]
    fn sanitize_removes_the_annotations_key_when_nothing_is_left() {
        let items = vec![serde_json::json!({
            "kind": "Deployment",
            "metadata": {"name": "app", "annotations": {
                "deployment.kubernetes.io/revision": "7",
            }},
        })];
        let meta = sanitize_k8s_items_for_backup(&items)[0]["metadata"].clone();
        assert!(
            meta.get("annotations").is_none(),
            "an empty map is noise on re-apply"
        );
    }

    /// A pinned clusterIP blocks re-apply whenever the address is already taken or
    /// outside the new cluster's service CIDR — which is exactly the situation
    /// during a restore onto fresh hardware.
    #[test]
    fn sanitize_unpins_service_cluster_ips() {
        let items = vec![serde_json::json!({
            "kind": "Service",
            "metadata": {"name": "gitea"},
            "spec": {"clusterIP": "10.43.0.17", "clusterIPs": ["10.43.0.17"],
                     "ports": [{"port": 3000}]},
        })];
        let spec = &sanitize_k8s_items_for_backup(&items)[0]["spec"];
        assert!(spec.get("clusterIP").is_none());
        assert!(spec.get("clusterIPs").is_none());
        assert_eq!(spec["ports"][0]["port"], serde_json::json!(3000));
    }

    #[test]
    fn sanitize_only_touches_cluster_ips_on_services() {
        // A ConfigMap that happens to hold a `clusterIP` key keeps it.
        let items = vec![serde_json::json!({
            "kind": "ConfigMap",
            "metadata": {"name": "cm"},
            "spec": {"clusterIP": "10.43.0.17"},
        })];
        let out = sanitize_k8s_items_for_backup(&items);
        assert_eq!(out[0]["spec"]["clusterIP"], serde_json::json!("10.43.0.17"));
    }

    /// Service-account tokens are minted per cluster; restoring them would carry a
    /// credential that is meaningless at best and confusing at worst.
    #[test]
    fn sanitize_discards_service_account_token_secrets() {
        let items = vec![
            serde_json::json!({
                "kind": "Secret", "type": "kubernetes.io/service-account-token",
                "metadata": {"name": "default-token-x"},
            }),
            serde_json::json!({
                "kind": "Secret", "type": "Opaque",
                "metadata": {"name": "app-credentials"},
            }),
        ];
        let out = sanitize_k8s_items_for_backup(&items);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["metadata"]["name"],
            serde_json::json!("app-credentials")
        );
    }

    #[test]
    fn sanitize_leaves_the_input_untouched() {
        let items = vec![serde_json::json!({
            "kind": "Deployment",
            "metadata": {"name": "app", "uid": "abc"},
        })];
        let _ = sanitize_k8s_items_for_backup(&items);
        assert_eq!(items[0]["metadata"]["uid"], serde_json::json!("abc"));
    }

    #[test]
    fn sanitize_survives_objects_with_no_metadata() {
        let items = vec![
            serde_json::json!({}),
            serde_json::json!({"kind": "Service"}),
        ];
        assert_eq!(sanitize_k8s_items_for_backup(&items).len(), 2);
    }
}
