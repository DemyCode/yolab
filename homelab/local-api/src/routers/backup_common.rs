use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "yolab");

pub(crate) async fn kubectl_apply(manifest: &str) -> anyhow::Result<()> {
    Ok(crate::kubectl::apply(manifest).await?)
}

pub(crate) async fn kubectl_get_secret(
    name: &str,
    ns: &str,
) -> Result<Option<HashMap<String, String>>, crate::exec::CmdError> {
    crate::kubectl::get_secret(name, ns).await
}

pub(crate) async fn kubectl_apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
) -> anyhow::Result<()> {
    Ok(crate::kubectl::apply_secret(name, ns, data, &[MANAGED_BY]).await?)
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

pub(crate) const MASTER_SECRET: &str = "yolab-backup-config";
pub(crate) const MASTER_NS: &str = "kube-system";
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
    pub endpoint: String,
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
        self.unlock_reporting(path, false).await
    }

    pub async fn unlock_reporting(&self, path: &str, loud: bool) {
        restic_unlock_reporting(
            &self.restic_repo(path),
            &self.restic_password,
            &self.access_key_id,
            &self.secret_access_key,
            loud,
        )
        .await;
    }
}

const RESTIC_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) async fn restic(
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    restic_timeout(repo, cfg, args, RESTIC_TIMEOUT).await
}

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

pub(crate) async fn restic_unlock(repo: &str, password: &str, key_id: &str, secret_key: &str) {
    restic_unlock_reporting(repo, password, key_id, secret_key, false).await
}

pub(crate) async fn restic_unlock_reporting(
    repo: &str,
    password: &str,
    key_id: &str,
    secret_key: &str,
    loud: bool,
) {
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
        Ok(Ok(o)) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if loud {
                tracing::warn!("restic unlock ({repo}) failed: {err}");
            } else {
                tracing::debug!("restic unlock ({repo}): {err}");
            }
        }
        Ok(Err(e)) if loud => tracing::warn!("restic unlock ({repo}) could not run: {e}"),
        Ok(Err(e)) => tracing::debug!("restic unlock ({repo}): {e}"),
        Err(_) if loud => tracing::warn!(
            "restic unlock ({repo}) timed out after {}s",
            RESTIC_TIMEOUT.as_secs()
        ),
        Err(_) => tracing::debug!(
            "restic unlock ({repo}): timed out after {}s",
            RESTIC_TIMEOUT.as_secs()
        ),
    }
}

pub(crate) async fn read_master_config() -> Option<BackupConfig> {
    match load_master_config().await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("backup config unreadable right now: {e}");
            None
        }
    }
}

pub(crate) async fn load_master_config() -> Result<Option<BackupConfig>, crate::exec::CmdError> {
    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await? else {
        return Ok(None);
    };
    Ok(config_from_secret(&data))
}

fn config_from_secret(data: &HashMap<String, String>) -> Option<BackupConfig> {
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

pub(crate) async fn ensure_master_config(url: &str, token: &str) -> anyhow::Result<BackupConfig> {
    if let Some(cfg) = load_master_config().await? {
        return Ok(cfg);
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

pub(crate) async fn ensure_restic_secret(
    ns: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<()> {
    ensure_restic_secret_for_repo(ns, ns, pvc, cfg).await
}

pub(crate) async fn ensure_restic_secret_for_repo(
    ns: &str,
    repo_ns: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(pvc);
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");
    let repo = cfg.restic_repo(&format!("volsync/{repo_ns}/{cid}"));
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

#[derive(Clone)]
pub(crate) struct PvcInfo {
    pub namespace: String,
    pub name: String,
    pub capacity: String,
}

pub(crate) async fn list_user_pvcs() -> anyhow::Result<Vec<PvcInfo>> {
    let managed: std::collections::HashSet<String> =
        list_managed_namespaces().await?.into_iter().collect();

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
            if name.starts_with("volsync-") {
                return None;
            }
            Some(PvcInfo {
                namespace: ns,
                name,
                capacity: item["spec"]["resources"]["requests"]["storage"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string(),
            })
        })
        .collect())
}

pub(crate) async fn list_managed_namespaces() -> anyhow::Result<Vec<String>> {
    let out = crate::kubectl::run(&[
        "get",
        "namespaces",
        "-l",
        "yolab.io/managed=true",
        "-o",
        "jsonpath={.items[*].metadata.name}",
    ])
    .await?;
    Ok(out.split_whitespace().map(String::from).collect())
}

pub(crate) fn replication_source_name(pvc_name: &str) -> String {
    format!("volsync-{}", canonical_pvc_id(pvc_name))
}

pub(crate) async fn ensure_replication_source(
    pvc: &PvcInfo,
    trigger_now: bool,
) -> anyhow::Result<Option<String>> {
    let cid = canonical_pvc_id(&pvc.name);
    let rs_name = replication_source_name(&pvc.name);
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");

    if !trigger_now {
        let exists =
            crate::kubectl::run(&["get", "replicationsource", &rs_name, "-n", &pvc.namespace])
                .await
                .is_ok();
        if exists {
            return Ok(None);
        }
    }

    let manual = chrono::Utc::now()
        .format(if trigger_now {
            "backup-%Y%m%d%H%M%S%3f"
        } else {
            "init-%Y%m%d%H%M%S%3f"
        })
        .to_string();
    let trigger = serde_json::json!({ "manual": manual });
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
                "moverSecurityContext": {
                    "runAsUser": 0,
                    "runAsGroup": 0,
                    "fsGroup": 0
                }
            }
        }
    });
    kubectl_apply(&manifest.to_string()).await?;
    Ok(Some(manual))
}

pub(crate) fn hours_since(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_hours())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_snapshots_listing_passes_no_lock() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        fn walk(dir: &std::path::Path, offenders: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, offenders);
                    continue;
                }
                if path.extension().is_some_and(|e| e == "rs") {
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    for (n, line) in text.lines().enumerate() {
                        if line.trim_start().starts_with("//") {
                            continue;
                        }
                        if line.contains("&[\"snapshots\"") && !line.contains("--no-lock") {
                            offenders.push(format!("{}:{}", path.display(), n + 1));
                        }
                    }
                }
            }
        }
        walk(&root, &mut offenders);

        assert!(
            offenders.is_empty(),
            "these `restic snapshots` calls take a lock they do not need, and an \
             interrupted one blocks that repository's retention forever — add \
             \"--no-lock\": {offenders:?}"
        );
    }

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
        assert_ne!(random_hex(16), random_hex(16));
    }

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

    #[test]
    fn hours_since_goes_negative_for_a_future_timestamp() {
        let t = (chrono::Utc::now() + chrono::Duration::hours(5)).to_rfc3339();
        assert!(hours_since(&t).is_some_and(|h| h < 0));
    }

    #[test]
    fn hours_since_returns_none_for_unparseable_input() {
        assert_eq!(hours_since(""), None);
        assert_eq!(hours_since("never"), None);
        assert_eq!(hours_since("2026-01-01"), None);
    }

    #[test]
    fn hours_since_accepts_the_z_suffix_kubernetes_emits() {
        assert!(hours_since("2020-01-01T00:00:00Z").is_some());
    }

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

    #[test]
    fn an_unparseable_capacity_reads_as_zero() {
        assert_eq!(parse_capacity_bytes(""), 0);
        assert_eq!(parse_capacity_bytes("lots"), 0);
        assert_eq!(parse_capacity_bytes("Gi"), 0);
        assert_eq!(parse_capacity_bytes("-5Gi"), 0);
        assert_eq!(parse_capacity_bytes("1.5Gi"), 0);
    }

    #[test]
    fn decimal_suffixes_are_not_mistaken_for_byte_counts() {
        assert_eq!(parse_capacity_bytes("5G"), 0);
        assert_eq!(parse_capacity_bytes("5M"), 0);
    }

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
        let items = vec![serde_json::json!({
            "kind": "ConfigMap",
            "metadata": {"name": "cm"},
            "spec": {"clusterIP": "10.43.0.17"},
        })];
        let out = sanitize_k8s_items_for_backup(&items);
        assert_eq!(out[0]["spec"]["clusterIP"], serde_json::json!("10.43.0.17"));
    }

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
