use kube::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::host::{CommandOutput, Host};

const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "yolab");

#[derive(Clone)]
pub(crate) struct Backend<H: Host = crate::host::RealHost> {
    pub kube: Client,
    pub host: H,
}

impl Backend {
    pub(crate) async fn real() -> anyhow::Result<Self> {
        Ok(Backend {
            kube: crate::k8s::client().await?,
            host: crate::host::RealHost,
        })
    }
}

pub(crate) async fn apply_secret(
    client: &Client,
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
) -> anyhow::Result<()> {
    crate::k8s::apply(
        client,
        &crate::k8s::secret_manifest(name, ns, data, &[MANAGED_BY]),
    )
    .await
}

pub(crate) fn random_hex(bytes: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
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

pub(crate) fn rebase_pvc_name(
    source_name: &str,
    source_instance: &str,
    dest_instance: &str,
) -> String {
    match source_name.strip_prefix(source_instance) {
        Some(suffix) => format!("{dest_instance}{suffix}"),
        None => source_name.to_string(),
    }
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

    pub async fn unlock<H: Host>(&self, host: &H, path: &str) {
        self.unlock_reporting(host, path, false).await
    }

    pub async fn unlock_reporting<H: Host>(&self, host: &H, path: &str, loud: bool) {
        restic_unlock_with(
            host,
            &self.restic_repo(path),
            &self.restic_password,
            &self.access_key_id,
            &self.secret_access_key,
            loud,
        )
        .await;
    }
}

pub(crate) const RESTIC_TIMEOUT: Duration = Duration::from_secs(180);

fn restic_env<'a>(
    repo: &'a str,
    password: &'a str,
    key_id: &'a str,
    secret_key: &'a str,
) -> [(&'a str, &'a str); 4] {
    [
        ("RESTIC_REPOSITORY", repo),
        ("RESTIC_PASSWORD", password),
        ("AWS_ACCESS_KEY_ID", key_id),
        ("AWS_SECRET_ACCESS_KEY", secret_key),
    ]
}

pub(crate) async fn restic_with<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
    timeout: Duration,
) -> anyhow::Result<CommandOutput> {
    let env = restic_env(
        repo,
        &cfg.restic_password,
        &cfg.access_key_id,
        &cfg.secret_access_key,
    );
    Ok(host.run_cmd_env("restic", args, &env, timeout).await?)
}

pub(crate) async fn restic_unlock_with<H: Host>(
    host: &H,
    repo: &str,
    password: &str,
    key_id: &str,
    secret_key: &str,
    loud: bool,
) {
    let env = restic_env(repo, password, key_id, secret_key);
    match host
        .run_cmd_env("restic", &["unlock"], &env, RESTIC_TIMEOUT)
        .await
    {
        Ok(o) if o.success => {
            let msg = o.stdout.trim();
            if !msg.is_empty() {
                tracing::info!("restic unlock ({repo}): {msg}");
            }
        }
        Ok(o) if loud => tracing::warn!("restic unlock ({repo}) failed: {}", o.stderr.trim()),
        Ok(o) => tracing::debug!("restic unlock ({repo}): {}", o.stderr.trim()),
        Err(e) if loud => tracing::warn!("restic unlock ({repo}) could not run: {e}"),
        Err(e) => tracing::debug!("restic unlock ({repo}): {e}"),
    }
}

pub(crate) async fn master_config(client: &Client) -> anyhow::Result<Option<BackupConfig>> {
    let Some(data) = crate::k8s::secret_data(client, MASTER_NS, MASTER_SECRET).await? else {
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

pub(crate) async fn ensure_master_config_with(
    client: &Client,
    url: &str,
    token: &str,
) -> anyhow::Result<BackupConfig> {
    if let Some(cfg) = master_config(client).await? {
        return Ok(cfg);
    }

    let s3 = platform_storage(url, token).await?;
    let restic_password = random_hex(32);

    apply_secret(
        client,
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

async fn platform_storage(url: &str, token: &str) -> anyhow::Result<S3StorageInfo> {
    let resp = crate::http::client()
        .post(format!("{url}/storage/s3"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?;
    resp.json().await.map_err(|e| anyhow::anyhow!(e))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyRefresh {
    NotEnabled,
    Current,
    Replaced,
    OtherBucket,
}

pub(crate) fn key_refresh(cfg: &BackupConfig, s3: &S3StorageInfo) -> KeyRefresh {
    if s3.bucket_name != cfg.bucket {
        KeyRefresh::OtherBucket
    } else if s3.access_key_id.is_empty()
        || s3.secret_access_key.is_empty()
        || (s3.access_key_id == cfg.access_key_id
            && s3.secret_access_key == cfg.secret_access_key)
    {
        KeyRefresh::Current
    } else {
        KeyRefresh::Replaced
    }
}

pub(crate) async fn refresh_master_key_with(
    client: &Client,
    url: &str,
    token: &str,
) -> anyhow::Result<KeyRefresh> {
    let Some(cfg) = master_config(client).await? else {
        return Ok(KeyRefresh::NotEnabled);
    };
    let s3 = platform_storage(url, token).await?;
    let outcome = key_refresh(&cfg, &s3);
    if outcome == KeyRefresh::Replaced {
        apply_secret(
            client,
            MASTER_SECRET,
            MASTER_NS,
            &[
                ("access_key_id", &s3.access_key_id),
                ("secret_access_key", &s3.secret_access_key),
                ("bucket", &cfg.bucket),
                ("endpoint", &cfg.endpoint),
                ("restic_password", &cfg.restic_password),
            ],
        )
        .await?;
    }
    Ok(outcome)
}

pub(crate) async fn allow_privileged_movers(client: &Client, ns: &str) {
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": ns,
            "annotations": { "volsync.backube/privileged-movers": "true" }
        }
    });
    if let Err(e) = crate::k8s::merge_patch(client, &patch).await {
        tracing::debug!("{ns}: could not allow privileged backup movers ({e})");
    }
}

pub(crate) async fn restic_secret(
    client: &Client,
    ns: &str,
    repo_ns: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(pvc);
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");
    let repo = cfg.restic_repo(&format!("volsync/{repo_ns}/{cid}"));
    apply_secret(
        client,
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

pub(crate) async fn user_pvcs(client: &Client) -> anyhow::Result<Vec<PvcInfo>> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let managed: std::collections::HashSet<String> =
        managed_namespaces(client).await?.into_iter().collect();

    let claims = kube::Api::<PersistentVolumeClaim>::all(client.clone())
        .list(&Default::default())
        .await?;

    Ok(claims
        .items
        .into_iter()
        .filter_map(|claim| {
            let ns = claim.metadata.namespace?;
            let name = claim.metadata.name?;
            if EXCLUDED_NS.contains(&ns.as_str()) || !managed.contains(&ns) {
                return None;
            }
            if name.starts_with("volsync-") {
                return None;
            }
            let capacity = claim
                .spec
                .and_then(|s| s.resources)
                .and_then(|r| r.requests)
                .and_then(|r| r.get("storage").map(|q| q.0.clone()))
                .unwrap_or_else(|| "?".to_string());
            Some(PvcInfo {
                namespace: ns,
                name,
                capacity,
            })
        })
        .collect())
}

pub(crate) async fn managed_namespaces(client: &Client) -> anyhow::Result<Vec<String>> {
    use k8s_openapi::api::core::v1::Namespace;
    let listed = kube::Api::<Namespace>::all(client.clone())
        .list(&kube::api::ListParams::default().labels("yolab.io/managed=true"))
        .await?;
    Ok(listed
        .items
        .into_iter()
        .filter_map(|ns| ns.metadata.name)
        .collect())
}

pub(crate) fn replication_source_name(pvc_name: &str) -> String {
    format!("volsync-{}", canonical_pvc_id(pvc_name))
}

pub(crate) async fn replication_source(
    client: &Client,
    pvc: &PvcInfo,
    trigger_now: bool,
) -> anyhow::Result<Option<String>> {
    let cid = canonical_pvc_id(&pvc.name);
    let rs_name = replication_source_name(&pvc.name);
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");

    if !trigger_now {
        let existing = crate::k8s::reference(
            "volsync.backube/v1alpha1",
            "ReplicationSource",
            &pvc.namespace,
            &rs_name,
        );
        if crate::k8s::exists(client, &existing).await? {
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
    crate::k8s::apply(client, &manifest).await?;
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

pub(crate) async fn drop_replication_destination(client: &Client, name: &str, namespace: &str) {
    let release = serde_json::json!({
        "apiVersion": "volsync.backube/v1alpha1",
        "kind": "ReplicationDestination",
        "metadata": { "name": name, "namespace": namespace, "finalizers": [] }
    });
    if let Err(e) = crate::k8s::merge_patch(client, &release).await {
        tracing::debug!("{namespace}/{name}: finalizers not cleared ({e})");
    }
    if let Err(e) = crate::k8s::delete_if_present(client, &release).await {
        tracing::debug!("{namespace}/{name}: not deleted ({e})");
    }
}

pub(crate) async fn scale(
    client: &Client,
    namespace: &str,
    name: &str,
    replicas: u32,
) -> anyhow::Result<()> {
    crate::k8s::merge_patch(
        client,
        &serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": name, "namespace": namespace },
            "spec": { "replicas": replicas }
        }),
    )
    .await
}

pub(crate) fn destination_pvc(
    name: &str,
    namespace: &str,
    capacity: &str,
    storage_class: &str,
    access_mode: &str,
) -> serde_json::Value {
    serde_json::json!({
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
    })
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
    fn rebase_pvc_name_replaces_the_instance_prefix() {
        assert_eq!(
            rebase_pvc_name(
                "filebrowser-test-yq46-data",
                "filebrowser-test-yq46",
                "filebrowser-test-k3m9"
            ),
            "filebrowser-test-k3m9-data"
        );
    }

    #[test]
    fn rebase_pvc_name_keeps_a_name_that_does_not_start_with_the_instance() {
        assert_eq!(
            rebase_pvc_name("data", "filebrowser-test-yq46", "filebrowser-test-k3m9"),
            "data"
        );
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

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list, status};
        use serde_json::json;
        use wiremock::matchers::{body_partial_json, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const CONFIG_PATH: &str = "/api/v1/namespaces/kube-system/secrets/yolab-backup-config";

        fn cfg() -> BackupConfig {
            BackupConfig {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
                bucket: "bucket-1".into(),
                endpoint: "https://s3.example".into(),
                restic_password: "pw".into(),
            }
        }

        fn stored_config(restic_password: &str) -> serde_json::Value {
            use base64::Engine as _;
            let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" },
                "data": {
                    "access_key_id": b("AKID"), "secret_access_key": b("SECRET"),
                    "bucket": b("bucket-1"), "endpoint": b("https://s3.example"),
                    "restic_password": b(restic_password)
                }
            })
        }

        async fn serve_config(server: &MockServer, code: u16, body: serde_json::Value) {
            Mock::given(method("GET"))
                .and(path(CONFIG_PATH))
                .respond_with(ResponseTemplate::new(code).set_body_json(body))
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn restic_gets_the_repository_and_credentials_through_its_environment() {
            let host = FakeHost::new().ok("restic snapshots", "[]");
            let out = restic_with(
                &host,
                "s3:x/cluster-backup",
                &cfg(),
                &["snapshots", "--no-lock", "--json"],
                RESTIC_TIMEOUT,
            )
            .await
            .unwrap();
            assert!(out.success);
            assert_eq!(
                host.env_of("restic snapshots").unwrap(),
                vec![
                    (
                        "RESTIC_REPOSITORY".to_string(),
                        "s3:x/cluster-backup".to_string()
                    ),
                    ("RESTIC_PASSWORD".to_string(), "pw".to_string()),
                    ("AWS_ACCESS_KEY_ID".to_string(), "AKID".to_string()),
                    ("AWS_SECRET_ACCESS_KEY".to_string(), "SECRET".to_string()),
                ]
            );
            assert!(host
                .calls()
                .iter()
                .all(|c| !c.contains("pw") && !c.contains("SECRET")));
        }

        #[tokio::test]
        async fn a_failing_unlock_is_logged_not_fatal() {
            let host = FakeHost::new().fail("restic unlock", "repository does not exist");
            restic_unlock_with(&host, "s3:x/y", "pw", "AKID", "SECRET", true).await;
            assert!(host.ran("restic unlock"));
        }

        #[tokio::test]
        async fn the_backup_config_is_read_from_its_secret() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("pw")).await;
            let found = master_config(&client).await.unwrap().unwrap();
            assert_eq!(found.restic_password, "pw");
            assert_eq!(
                found.restic_repo("cluster-backup"),
                "s3:https://s3.example/bucket-1/cluster-backup"
            );
        }

        #[tokio::test]
        async fn no_secret_means_backups_are_not_set_up() {
            let (server, client) = api_server().await;
            serve_config(&server, 404, status(404, "NotFound")).await;
            assert!(master_config(&client).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn a_secret_without_a_password_is_not_a_usable_config() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("")).await;
            assert!(master_config(&client).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn an_existing_config_is_used_as_it_is_and_the_platform_is_not_asked() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("pw")).await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&platform)
                .await;

            let found = ensure_master_config_with(&client, &platform.uri(), "tok")
                .await
                .unwrap();

            assert_eq!(found.restic_password, "pw");
        }

        #[tokio::test]
        async fn a_first_backup_asks_the_platform_for_storage_and_keeps_a_new_recovery_key() {
            let (server, client) = api_server().await;
            serve_config(&server, 404, status(404, "NotFound")).await;
            Mock::given(method("PATCH"))
                .and(path(CONFIG_PATH))
                .and(body_partial_json(
                    json!({ "stringData": { "bucket": "b-new", "endpoint": "https://s3.new" } }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(stored_config("x")))
                .expect(1)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/storage/s3"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "bucket_name": "b-new", "endpoint": "https://s3.new", "region": "eu",
                    "access_key_id": "K", "secret_access_key": "S", "created_at": "now"
                })))
                .expect(1)
                .mount(&platform)
                .await;

            let made = ensure_master_config_with(&client, &platform.uri(), "tok")
                .await
                .unwrap();

            assert_eq!(made.bucket, "b-new");
            assert_eq!(
                made.restic_password.len(),
                64,
                "32 random bytes, hex-encoded"
            );
        }

        #[tokio::test]
        async fn an_unreachable_cluster_never_replaces_the_recovery_key() {
            let (server, client) = api_server().await;
            serve_config(&server, 503, status(503, "ServiceUnavailable")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&platform)
                .await;

            assert!(ensure_master_config_with(&client, &platform.uri(), "tok")
                .await
                .is_err());
        }

        #[tokio::test]
        async fn a_platform_that_refuses_leaves_nothing_half_written() {
            let (server, client) = api_server().await;
            serve_config(&server, 404, status(404, "NotFound")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(402))
                .mount(&platform)
                .await;

            assert!(ensure_master_config_with(&client, &platform.uri(), "tok")
                .await
                .is_err());
        }

        fn platform_key(bucket: &str, key: &str, secret: &str) -> serde_json::Value {
            json!({
                "bucket_name": bucket, "endpoint": "https://s3.example", "region": "eu",
                "access_key_id": key, "secret_access_key": secret, "created_at": "now"
            })
        }

        fn info(bucket: &str, key: &str, secret: &str) -> S3StorageInfo {
            serde_json::from_value(platform_key(bucket, key, secret)).unwrap()
        }

        #[test]
        fn a_key_reissued_after_a_pause_replaces_the_stored_one() {
            assert_eq!(
                key_refresh(&cfg(), &info("bucket-1", "AKID2", "SECRET2")),
                KeyRefresh::Replaced
            );
            assert_eq!(
                key_refresh(&cfg(), &info("bucket-1", "AKID", "SECRET2")),
                KeyRefresh::Replaced
            );
        }

        #[test]
        fn the_same_key_is_left_alone() {
            assert_eq!(
                key_refresh(&cfg(), &info("bucket-1", "AKID", "SECRET")),
                KeyRefresh::Current
            );
        }

        #[test]
        fn an_empty_key_from_the_platform_never_overwrites_a_working_one() {
            assert_eq!(
                key_refresh(&cfg(), &info("bucket-1", "", "")),
                KeyRefresh::Current
            );
        }

        #[test]
        fn a_key_for_another_bucket_is_never_adopted() {
            assert_eq!(
                key_refresh(&cfg(), &info("bucket-2", "AKID2", "SECRET2")),
                KeyRefresh::OtherBucket
            );
        }

        #[tokio::test]
        async fn a_reissued_key_is_written_with_the_same_recovery_key_and_bucket() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("pw")).await;
            Mock::given(method("PATCH"))
                .and(path(CONFIG_PATH))
                .and(body_partial_json(json!({ "stringData": {
                    "access_key_id": "AKID2", "secret_access_key": "SECRET2",
                    "bucket": "bucket-1", "endpoint": "https://s3.example",
                    "restic_password": "pw"
                } })))
                .respond_with(ResponseTemplate::new(200).set_body_json(stored_config("pw")))
                .expect(1)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/storage/s3"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(platform_key("bucket-1", "AKID2", "SECRET2")),
                )
                .expect(1)
                .mount(&platform)
                .await;

            assert_eq!(
                refresh_master_key_with(&client, &platform.uri(), "tok")
                    .await
                    .unwrap(),
                KeyRefresh::Replaced
            );
        }

        #[tokio::test]
        async fn an_unchanged_key_writes_nothing() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("pw")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(platform_key("bucket-1", "AKID", "SECRET")),
                )
                .mount(&platform)
                .await;

            assert_eq!(
                refresh_master_key_with(&client, &platform.uri(), "tok")
                    .await
                    .unwrap(),
                KeyRefresh::Current
            );
        }

        #[tokio::test]
        async fn backups_that_were_never_enabled_are_not_enabled_by_a_key_refresh() {
            let (server, client) = api_server().await;
            serve_config(&server, 404, status(404, "NotFound")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&platform)
                .await;

            assert_eq!(
                refresh_master_key_with(&client, &platform.uri(), "tok")
                    .await
                    .unwrap(),
                KeyRefresh::NotEnabled
            );
        }

        #[tokio::test]
        async fn an_unreachable_platform_keeps_the_stored_key() {
            let (server, client) = api_server().await;
            serve_config(&server, 200, stored_config("pw")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&platform)
                .await;

            assert!(refresh_master_key_with(&client, &platform.uri(), "tok")
                .await
                .is_err());
        }

        #[tokio::test]
        async fn a_volumes_restic_secret_points_at_its_own_repository() {
            let (server, client) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path("/api/v1/namespaces/yolab-new/secrets/data-restic"))
                .and(body_partial_json(json!({ "stringData": {
                    "RESTIC_REPOSITORY": "s3:https://s3.example/bucket-1/volsync/yolab-old/data",
                    "RESTIC_PASSWORD": "pw"
                }})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "v1", "kind": "Secret",
                    "metadata": { "name": "data-restic", "namespace": "yolab-new" }
                })))
                .expect(1)
                .mount(&server)
                .await;

            restic_secret(&client, "yolab-new", "yolab-old", "data", &cfg())
                .await
                .unwrap();
        }

        fn claim(ns: &str, name: &str, storage: Option<&str>) -> serde_json::Value {
            let mut c = json!({ "metadata": { "name": name, "namespace": ns }, "spec": {} });
            if let Some(s) = storage {
                c["spec"] = json!({ "resources": { "requests": { "storage": s } } });
            }
            c
        }

        #[tokio::test]
        async fn only_the_volumes_of_installed_apps_are_backed_up() {
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces"))
                .and(query_param("labelSelector", "yolab.io/managed=true"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Namespace",
                    vec![
                        json!({ "metadata": { "name": "yolab-a" } }),
                        json!({ "metadata": { "name": "default" } }),
                    ],
                )))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/persistentvolumeclaims"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "PersistentVolumeClaim",
                    vec![
                        claim("yolab-a", "data", Some("5Gi")),
                        claim("yolab-a", "volsync-data-cache", Some("1Gi")),
                        claim("yolab-a", "config", None),
                        claim("default", "stray", Some("1Gi")),
                        claim("yolab-unmanaged", "x", Some("1Gi")),
                    ],
                )))
                .mount(&server)
                .await;

            let pvcs = user_pvcs(&client).await.unwrap();

            let names: Vec<(&str, &str)> = pvcs
                .iter()
                .map(|p| (p.name.as_str(), p.capacity.as_str()))
                .collect();
            assert_eq!(names, vec![("data", "5Gi"), ("config", "?")]);
        }

        fn source(pvc: &str) -> PvcInfo {
            PvcInfo {
                namespace: "yolab-a".into(),
                name: pvc.into(),
                capacity: "5Gi".into(),
            }
        }

        const RS_PATH: &str =
            "/apis/volsync.backube/v1alpha1/namespaces/yolab-a/replicationsources/volsync-data";

        fn rs_body() -> serde_json::Value {
            json!({
                "apiVersion": "volsync.backube/v1alpha1", "kind": "ReplicationSource",
                "metadata": { "name": "volsync-data", "namespace": "yolab-a" }
            })
        }

        #[tokio::test]
        async fn an_existing_schedule_is_left_alone_when_not_asked_to_run_now() {
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .and(path(RS_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(rs_body()))
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            assert_eq!(
                replication_source(&client, &source("data"), false)
                    .await
                    .unwrap(),
                None
            );
        }

        #[tokio::test]
        async fn a_volume_never_backed_up_gets_its_first_backup_started() {
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .and(path(RS_PATH))
                .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .and(path(RS_PATH))
                .and(body_partial_json(json!({ "spec": { "sourcePVC": "data", "restic": { "repository": "data-restic" } } })))
                .respond_with(ResponseTemplate::new(200).set_body_json(rs_body()))
                .expect(1)
                .mount(&server)
                .await;

            let trigger = replication_source(&client, &source("data"), false)
                .await
                .unwrap()
                .unwrap();
            assert!(trigger.starts_with("init-"));
        }

        #[tokio::test]
        async fn back_up_now_triggers_a_run_without_asking_whether_one_exists() {
            let (server, client) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path(RS_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(rs_body()))
                .expect(1)
                .mount(&server)
                .await;

            let trigger = replication_source(&client, &source("data"), true)
                .await
                .unwrap()
                .unwrap();
            assert!(trigger.starts_with("backup-"));
            assert!(server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method.as_str() != "GET"));
        }

        #[tokio::test]
        async fn an_unreachable_cluster_does_not_start_a_backup_blindly() {
            let (server, client) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;

            assert!(replication_source(&client, &source("data"), false)
                .await
                .is_err());
        }

        #[tokio::test]
        async fn backup_movers_are_allowed_to_run_privileged_in_the_apps_namespace() {
            let (server, client) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path("/api/v1/namespaces/yolab-a"))
                .and(body_partial_json(json!({ "metadata": { "annotations": {
                    "volsync.backube/privileged-movers": "true"
                }}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "yolab-a" }
                })))
                .expect(1)
                .mount(&server)
                .await;

            allow_privileged_movers(&client, "yolab-a").await;
        }

        #[tokio::test]
        async fn a_finished_restore_destination_is_released_then_removed() {
            let (server, client) = api_server().await;
            let rd = "/apis/volsync.backube/v1alpha1/namespaces/yolab-a/replicationdestinations/restore-data";
            Mock::given(method("PATCH"))
                .and(path(rd))
                .and(body_partial_json(
                    json!({ "metadata": { "finalizers": [] } }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "volsync.backube/v1alpha1", "kind": "ReplicationDestination",
                    "metadata": { "name": "restore-data", "namespace": "yolab-a" }
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .and(path(rd))
                .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
                .expect(1)
                .mount(&server)
                .await;

            drop_replication_destination(&client, "restore-data", "yolab-a").await;

            let order: Vec<String> = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| r.method.to_string())
                .collect();
            assert_eq!(
                order,
                vec!["PATCH", "DELETE"],
                "finalizers go first, or the delete hangs"
            );
        }

        #[tokio::test]
        async fn scaling_sets_the_replica_count() {
            let (server, client) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path("/apis/apps/v1/namespaces/yolab-a/deployments/web"))
                .and(body_partial_json(json!({ "spec": { "replicas": 0 } })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": { "name": "web", "namespace": "yolab-a" }
                })))
                .expect(1)
                .mount(&server)
                .await;

            scale(&client, "yolab-a", "web", 0).await.unwrap();
        }

        #[test]
        fn a_restore_volume_asks_for_the_size_class_and_access_it_needs() {
            let pvc = destination_pvc("data", "yolab-a", "5Gi", "yolab-cephfs", "ReadWriteMany");
            assert_eq!(pvc["spec"]["resources"]["requests"]["storage"], "5Gi");
            assert_eq!(pvc["spec"]["storageClassName"], "yolab-cephfs");
            assert_eq!(pvc["spec"]["accessModes"], json!(["ReadWriteMany"]));
            assert_eq!(
                pvc["metadata"]["labels"]["app.kubernetes.io/managed-by"],
                "yolab"
            );
        }
    }
}
