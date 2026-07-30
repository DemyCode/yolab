use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::{config::Config, error::Result, AppState};

// ── Backup/restore operation state ──────────────────────────────────────────
//
// Single source of truth the frontend reads (GET /api/backups/state) instead of
// tracking its own progress client-side — a page refresh, a second tab, or a lost
// connection should never desync from what's actually happening on the backend.
//
// "Backing up" is derived from cluster state: a ConfigMap lock for the cluster-
// metadata backup path, and a kubectl pod check for VolSync mover syncs.  Both
// are cluster-wide, so state is correct regardless of which node's local-api
// the frontend hits.  The lock is cleaned up on Drop; if the process crashes
// hard, the stale-lock path in acquire() removes any lock older than 2 h.
//
// "Restoring" is also derived from real cluster state — checking whether any
// yolab-managed ReplicationDestination exists.  A flag could get stuck forever
// if the process restarted mid-restore; kubectl queries don't have that problem.

const LOCK_NAME: &str = "yolab-backup-lock";
const LOCK_NS: &str = "kube-system";

/// Cluster-wide mutex via a ConfigMap.  `kubectl create` is atomic — if two
/// processes race, the first one wins and the second gets AlreadyExists.
///
/// On Drop the lock is released by deleting the ConfigMap.  If this process
/// crashes hard (kill -9, power loss) the ConfigMap lingers; the stale-lock
/// path in `acquire()` deletes any lock that is older than 2 hours because no
/// real backup takes longer than that.
struct ClusterBackupGuard {
    acquired: bool,
}

impl ClusterBackupGuard {
    async fn acquire() -> Option<Self> {
        // ── Stale-lock recovery ──────────────────────────────────────
        let get_out = Command::new("kubectl")
            .args(["get", "configmap", LOCK_NAME, "-n", LOCK_NS, "-o", "json", "--ignore-not-found"])
            .output().await.ok()?;  // ok() → if kubectl completely fails, bail

        if get_out.status.success() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&get_out.stdout) {
                let ts_str = v["metadata"]["annotations"]["yolab.io/lock-started"]
                    .as_str().unwrap_or("");
                if let Ok(ts) = ts_str.parse::<i64>() {
                    let age_secs = chrono::Utc::now().timestamp() - ts;
                    if age_secs > 7200 {
                        tracing::warn!("backup-lock: stale lock ({age_secs}s old) — removing");
                        let _ = Command::new("kubectl")
                            .args(["delete", "configmap", LOCK_NAME, "-n", LOCK_NS, "--ignore-not-found"])
                            .output().await;
                    } else {
                        return None; // another node is backing up
                    }
                }
            }
        }

        // ── Try to acquire ───────────────────────────────────────────
        let ts = chrono::Utc::now().timestamp().to_string();
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": LOCK_NAME,
                "namespace": LOCK_NS,
                "labels": { "app.kubernetes.io/managed-by": "yolab" },
                "annotations": { "yolab.io/lock-started": ts, "yolab.io/lock-reason": "backup" },
            }
        });

        use tokio::io::AsyncWriteExt;
        use std::process::Stdio as ProcessStdio;
        let mut child = match Command::new("kubectl")
            .args(["create", "-f", "-"])
            .stdin(ProcessStdio::piped())
            .stdout(ProcessStdio::null())
            .stderr(ProcessStdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return None,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(manifest.to_string().as_bytes()).await;
        }
        let out = child.wait_with_output().await.ok()?;
        if out.status.success() {
            Some(ClusterBackupGuard { acquired: true })
        } else {
            // AlreadyExists or any other error → someone else holds the lock
            None
        }
    }
}

impl Drop for ClusterBackupGuard {
    fn drop(&mut self) {
        if self.acquired {
            let _ = std::process::Command::new("kubectl")
                .args(["delete", "configmap", LOCK_NAME, "-n", LOCK_NS, "--ignore-not-found"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

const DR_LOCK_NAME: &str = "yolab-dr-lock";

/// Cluster-wide mutex for DR restores, symmetric to ClusterBackupGuard.
/// Acquired before spawning the background restore task so restore_in_progress()
/// returns true from the moment dr_start accepts a request — not only after the
/// first ReplicationDestination is created (which can be minutes later if
/// CephFS is recovering).
struct DrRestoreGuard;

impl DrRestoreGuard {
    async fn acquire() -> Option<Self> {
        let get_out = Command::new("kubectl")
            .args(["get", "configmap", DR_LOCK_NAME, "-n", LOCK_NS,
                   "-o", "json", "--ignore-not-found"])
            .output().await.ok()?;
        if get_out.status.success() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&get_out.stdout) {
                let ts_str = v["metadata"]["annotations"]["yolab.io/lock-started"]
                    .as_str().unwrap_or("");
                if let Ok(ts) = ts_str.parse::<i64>() {
                    let age_secs = chrono::Utc::now().timestamp() - ts;
                    if age_secs > 7200 {
                        tracing::warn!("dr-lock: stale lock ({age_secs}s old) — removing");
                        let _ = Command::new("kubectl")
                            .args(["delete", "configmap", DR_LOCK_NAME, "-n", LOCK_NS,
                                   "--ignore-not-found"])
                            .output().await;
                    } else {
                        return None;
                    }
                }
            }
        }
        let ts = chrono::Utc::now().timestamp().to_string();
        let manifest = serde_json::json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {
                "name": DR_LOCK_NAME, "namespace": LOCK_NS,
                "labels": { "app.kubernetes.io/managed-by": "yolab" },
                "annotations": { "yolab.io/lock-started": ts, "yolab.io/lock-reason": "dr-restore" },
            }
        });
        use tokio::io::AsyncWriteExt;
        use std::process::Stdio as ProcessStdio;
        let mut child = Command::new("kubectl")
            .args(["create", "-f", "-"])
            .stdin(ProcessStdio::piped())
            .stdout(ProcessStdio::null())
            .stderr(ProcessStdio::piped())
            .spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(manifest.to_string().as_bytes()).await;
        }
        let out = child.wait_with_output().await.ok()?;
        if out.status.success() { Some(DrRestoreGuard) } else { None }
    }
}

impl Drop for DrRestoreGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kubectl")
            .args(["delete", "configmap", DR_LOCK_NAME, "-n", LOCK_NS, "--ignore-not-found"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

async fn restore_in_progress() -> bool {
    // Check the DR lock first — it's held from before the first RD is created
    // until the entire restore task completes, so this is reliable even during
    // the CephFS-wait phase when no ReplicationDestinations exist yet.
    let lock_held = Command::new("kubectl")
        .args(["get", "configmap", DR_LOCK_NAME, "-n", LOCK_NS,
               "--ignore-not-found", "-o", "name"])
        .output().await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if lock_held { return true; }
    // Fallback: catch restores started without the lock (pre-lock binary versions).
    Command::new("kubectl")
        .args([
            "get", "replicationdestination", "-A",
            "-l", "app.kubernetes.io/managed-by=yolab", "-o", "name",
        ])
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Detects whether any VolSync backup mover pod is currently running — i.e. a
/// `volsync-src-*` pod that is actively pushing PVC data to B2.  The cluster-
/// metadata backup is guarded by a ConfigMap lock; this covers the other half.
async fn volsync_backup_in_progress() -> bool {
    Command::new("kubectl")
        .args([
            "get", "pods", "-A",
            "-l", "app.kubernetes.io/created-by=volsync",
            "--field-selector=status.phase=Running",
            "-o", "name",
        ])
        .output()
        .await
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.contains("volsync-src-"))
        })
        .unwrap_or(false)
}

async fn cluster_backup_lock_held() -> bool {
    Command::new("kubectl")
        .args(["get", "configmap", LOCK_NAME, "-n", LOCK_NS, "--ignore-not-found", "-o", "name"])
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// True when any kind of backup is happening — cluster-metadata backup
/// (guarded by the ConfigMap lock) OR a VolSync PVC data sync.
async fn any_backup_in_progress() -> bool {
    cluster_backup_lock_held().await || volsync_backup_in_progress().await
}

/// Refuses to start a new backup/restore action while either is already in progress.
async fn ensure_no_operation_in_progress() -> anyhow::Result<()> {
    if cluster_backup_lock_held().await {
        anyhow::bail!("A backup is currently running — try again once it finishes.");
    }
    if volsync_backup_in_progress().await {
        anyhow::bail!("VolSync is currently backing up PVC data — try again once it finishes.");
    }
    if restore_in_progress().await {
        anyhow::bail!("A restore is currently in progress — try again once it finishes.");
    }
    Ok(())
}

/// GET /api/backups/state — read-only; the frontend polls this instead of tracking
/// backup/restore progress itself.
pub async fn operation_state(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({
        "backing_up": any_backup_in_progress().await,
        "restoring": restore_in_progress().await,
    })))
}

// ── Config reader ─────────────────────────────────────────────────────────────

pub fn ye_creds(cfg: &Config) -> Option<(String, String)> {
    let text = std::fs::read_to_string(&cfg.config_path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    if let Some(tunnel) = table.get("tunnel").and_then(|v| v.as_table()) {
        let url = tunnel
            .get("platform_api_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        let token = tunnel
            .get("account_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !url.is_empty() && !token.is_empty() {
            return Some((url, token));
        }
    }
    None
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── S3 / SFTP pass-through endpoints ─────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct S3StorageInfo {
    pub bucket_name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub created_at: String,
}

pub async fn get_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "platform API not configured" })));
    };
    let resp = http_client()
        .get(format!("{url}/storage/s3"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Json(serde_json::json!({ "provisioned": false })));
    }
    let body: serde_json::Value = resp
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(Json(serde_json::json!({ "provisioned": true, "s3": body })))
}

pub async fn get_sftp(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "platform API not configured" })));
    };
    let resp = http_client()
        .get(format!("{url}/storage/sftp"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Json(serde_json::json!({ "provisioned": false })));
    }
    let body: serde_json::Value = resp
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(Json(serde_json::json!({ "provisioned": true, "sftp": body })))
}

// ── kubectl helpers ───────────────────────────────────────────────────────────
//
// Thin wrappers over the shared crate::kubectl helpers, preserving the call
// sites and behavior in this module (secrets here are labelled managed-by=yolab).

const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "yolab");

async fn kubectl_apply(manifest: &str) -> anyhow::Result<()> {
    crate::kubectl::apply(manifest).await
}

async fn kubectl_get_secret(name: &str, ns: &str) -> Option<HashMap<String, String>> {
    crate::kubectl::get_secret(name, ns).await
}

async fn kubectl_apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
) -> anyhow::Result<()> {
    crate::kubectl::apply_secret(name, ns, data, &[MANAGED_BY]).await
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Collapses a (possibly restore-mangled) PVC name back to its original identity.
///
/// Restores rename the live PVC to `volsync-emergency-restore-{id}-dest`. Without this,
/// every subsequent restore of an already-restored PVC mints a longer, uglier name and a
/// brand new ReplicationSource/restic-secret/S3-path — fragmenting backup history and
/// leaving the previous RS behind as an orphaned duplicate. Names derived from this id
/// (RS name, restic secret name, S3 repo path) stay stable across any number of restores.
fn canonical_pvc_id(pvc_name: &str) -> String {
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

const MASTER_SECRET: &str = "yolab-backup-config";
const MASTER_NS: &str = "kube-system";
// Secret name per PVC: "<pvc-name>-restic" in the PVC's namespace.
const RESTIC_SECRET_SUFFIX: &str = "-restic";

const EXCLUDED_NS: &[&str] = &[
    "kube-system",
    "rook-ceph",
    "velero",
    "volsync-system",
    "cattle-system",
    "local-path-storage",
    "default",
];

#[derive(Clone)]
struct BackupConfig {
    access_key_id: String,
    secret_access_key: String,
    bucket: String,
    /// Full S3 endpoint URL e.g. https://s3.eu-central-003.backblazeb2.com
    endpoint: String,
    /// restic encryption password — generated once, never sent to yolab-external.
    restic_password: String,
}

impl BackupConfig {
    fn restic_repo(&self, path: &str) -> String {
        format!(
            "s3:{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            path
        )
    }
}

async fn ensure_master_config(url: &str, token: &str) -> anyhow::Result<BackupConfig> {
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

/// Create (or update) the per-PVC restic secret in its namespace.
/// Contains the full repo URL so VolSync knows where to read/write.
/// Keyed by the canonical PVC id so the repo path (and thus backup history) survives restores.
async fn ensure_restic_secret(ns: &str, pvc: &str, cfg: &BackupConfig) -> anyhow::Result<()> {
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
struct PvcInfo {
    namespace: String,
    name: String,
}

async fn list_user_pvcs() -> anyhow::Result<Vec<PvcInfo>> {
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
            if EXCLUDED_NS.contains(&ns.as_str()) {
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
async fn ensure_replication_source(pvc: &PvcInfo, trigger_now: bool) -> anyhow::Result<()> {
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

/// Polls until every PVC's ReplicationSource reports Successful with a `lastSyncTime`
/// newer than `since` (the moment we triggered this backup session). The `since` guard
/// prevents a stale Successful from a previous session from satisfying the wait.
async fn wait_for_volsync_sync(
    pvcs: &[PvcInfo],
    since: chrono::DateTime<chrono::Utc>,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    if pvcs.is_empty() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timed out waiting for VolSync PVC backups");
        }
        let rs_out = Command::new("kubectl")
            .args(["get", "replicationsource", "-A", "-o", "json"])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let v: serde_json::Value =
            serde_json::from_slice(&rs_out.stdout).unwrap_or(serde_json::json!({"items": []}));

        let mut all_ok = true;
        let mut pending: Vec<String> = Vec::new();

        for pvc in pvcs {
            let cid = canonical_pvc_id(&pvc.name);
            let rs_name = format!("volsync-{cid}");
            let item = v["items"].as_array().and_then(|items| {
                items.iter().find(|i| {
                    i["metadata"]["name"].as_str() == Some(&rs_name)
                        && i["metadata"]["namespace"].as_str() == Some(&pvc.namespace)
                })
            });
            let result = item.and_then(|i| i["status"]["latestMoverStatus"]["result"].as_str());
            let synced_after_trigger = item
                .and_then(|i| i["status"]["lastSyncTime"].as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc) >= since)
                .unwrap_or(false);

            if result == Some("Successful") && synced_after_trigger {
                // This PVC's backup completed in this session.
            } else {
                all_ok = false;
                let label = format!("{}/{}", pvc.namespace, pvc.name);
                pending.push(match result {
                    Some(s) if !synced_after_trigger => format!("{label}: {s} (previous session)"),
                    Some(s) => format!("{label}: {s}"),
                    None    => format!("{label}: waiting…"),
                });
            }
        }

        if all_ok {
            return Ok(());
        }

        tracing::info!("volsync: {} PVC(s) still syncing — {:?}", pending.len(), pending.first());
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

/// POST /api/backups/s3/enable — idempotent: provisions B2, configures VolSync per PVC.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };

    let cfg = ensure_master_config(&url, &token).await?;
    let pvcs = list_user_pvcs().await.unwrap_or_default();

    let mut sources: Vec<String> = Vec::new();

    for pvc in &pvcs {
        ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await?;
        ensure_replication_source(pvc, false).await?;
        sources.push(format!("{}/{}", pvc.namespace, pvc.name));
    }

    Ok(Json(serde_json::json!({
        "provisioned": true,
        "pvcs_configured": sources,
        "backup": "PVC data + cluster state snapshotted together daily at 02:00 UTC",
    })))
}

/// A PVC hasn't synced in this long → flag it as stale rather than silently "Pending" forever.
/// 36 h comfortably exceeds the daily 02:00 UTC backup window plus retry slack.
const STALE_AFTER_HOURS: i64 = 36;

fn hours_since(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_hours())
}

/// GET /api/backups/status — per-PVC VolSync ReplicationSource status + etcd snapshot.
pub async fn backup_status(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let rs_out = Command::new("kubectl")
        .args(["get", "replicationsource", "-A", "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let v: serde_json::Value =
        serde_json::from_slice(&rs_out.stdout).unwrap_or(serde_json::json!({"items": []}));

    // Build a (namespace, pvc_name) → (phase, deletionTimestamp) map from all PVCs.
    let pvc_health_map: HashMap<(String, String), (String, Option<String>)> = Command::new("kubectl")
        .args(["get", "pvc", "-A", "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            let ns = item["metadata"]["namespace"].as_str()?.to_string();
            let name = item["metadata"]["name"].as_str()?.to_string();
            let phase = item["status"]["phase"].as_str().unwrap_or("Unknown").to_string();
            let deletion_ts = item["metadata"]["deletionTimestamp"].as_str().map(String::from);
            Some(((ns, name), (phase, deletion_ts)))
        })
        .collect();

    let pvcs: Vec<serde_json::Value> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| {
            let namespace = item["metadata"]["namespace"].as_str().unwrap_or("").to_string();
            let pvc = item["spec"]["sourcePVC"].as_str().unwrap_or("").to_string();
            let created = item["metadata"]["creationTimestamp"].as_str().map(String::from);
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            let last_sync_duration =
                item["status"]["lastSyncDuration"].as_str().map(String::from);
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or(if last_sync_time.is_some() { "Successful" } else { "Pending" })
                .to_string();
            let (pvc_phase, pvc_deletion_ts) = pvc_health_map
                .get(&(namespace.clone(), pvc.clone()))
                .cloned()
                .unwrap_or(("NotFound".to_string(), None));

            // Stale: never synced and this RS has existed longer than the grace window, or
            // its last successful sync is older than the grace window. Either way, a backup
            // that looks "Pending" forever with no visible alert is exactly how a dead backup
            // goes unnoticed until the day it's needed.
            let stale = match &last_sync_time {
                Some(t) => hours_since(t).map_or(true, |h| h > STALE_AFTER_HOURS),
                None => created
                    .as_deref()
                    .and_then(hours_since)
                    .map_or(false, |h| h > STALE_AFTER_HOURS),
            };
            // The PVC has a pending deletion but is still present (finalizer blocking) —
            // the exact state that makes every future backup job permanently unschedulable.
            let stuck_terminating = pvc_deletion_ts.is_some();

            serde_json::json!({
                "namespace": namespace,
                "pvc": pvc,
                "last_sync_time": last_sync_time,
                "last_sync_duration": last_sync_duration,
                "result": result,
                "pvc_phase": pvc_phase,
                "stale": stale,
                "stuck_terminating": stuck_terminating,
                "pvc_deletion_timestamp": pvc_deletion_ts,
            })
        })
        .collect();

    let dr_mode = pvcs
        .iter()
        .any(|p| matches!(p["pvc_phase"].as_str(), Some("Lost") | Some("NotFound")));
    let backup_alert = pvcs.iter().any(|p| {
        p["stale"].as_bool().unwrap_or(false) || p["stuck_terminating"].as_bool().unwrap_or(false)
    });

    // Latest etcd snapshot from K3s CRD.
    let etcd_last = Command::new("kubectl")
        .args(["get", "etcdsnapshotfile", "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| {
            v["items"]
                .as_array()?
                .iter()
                .filter(|i| {
                    i["metadata"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("etcd-daily-")
                })
                .filter_map(|i| i["status"]["creationTime"].as_str().map(String::from))
                .max()
        });

    Ok(Json(serde_json::json!({
        "pvcs": pvcs,
        "etcd_last_snapshot": etcd_last,
        "dr_mode": dr_mode,
        "backup_alert": backup_alert,
    })))
}

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
async fn delete_replication_destination_without_touching_pvc(name: &str, namespace: &str) {
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

async fn find_deployments_for_pvc(namespace: &str, pvc_name: &str) -> anyhow::Result<Vec<String>> {
    let out = Command::new("kubectl")
        .args(["get", "deployments", "-n", namespace, "-o", "json"])
        .output()
        .await?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let names = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            let name = item["metadata"]["name"].as_str()?.to_string();
            let volumes = item["spec"]["template"]["spec"]["volumes"].as_array()?;
            let refs_pvc = volumes
                .iter()
                .any(|vol| vol["persistentVolumeClaim"]["claimName"].as_str() == Some(pvc_name));
            if refs_pvc { Some(name) } else { None }
        })
        .collect();
    Ok(names)
}

async fn scale_deployment(namespace: &str, name: &str, replicas: u32) -> anyhow::Result<()> {
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

/// Finishes one completed restore: scales the app's deployments back up and cleans up the
/// ReplicationDestination. Called by /dr/apply and the background reconciler.
/// Idempotent: safe to call more than once (scaling to existing replica count is a no-op,
/// RD delete ignores not-found).
async fn apply_one_restore(namespace: &str, rd_name: &str, pvc_name: &str) -> anyhow::Result<Vec<String>> {
    let pvc_ok = Command::new("kubectl")
        .args(["get", "pvc", pvc_name, "-n", namespace])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !pvc_ok {
        anyhow::bail!("restored PVC {pvc_name} not found yet");
    }

    let deployments = find_deployments_for_pvc(namespace, pvc_name).await.unwrap_or_default();
    for deploy in &deployments {
        let _ = scale_deployment(namespace, deploy, 1).await;
    }

    delete_replication_destination_without_touching_pvc(rd_name, namespace).await;

    Ok(deployments)
}

/// Creates the PVC a restore will write into, owned by us rather than VolSync.
///
/// Every RD manifest in this file uses copyMethod "Direct" with this PVC passed as
/// `destinationPVC`, instead of copyMethod "Snapshot" (which was the original design).
/// Per VolSync's own docs, a Snapshot-copyMethod destination PVC is explicitly internal to
/// VolSync's own bookkeeping — it can be recreated/replaced on subsequent reconciles — which
/// is fundamentally incompatible with what this code does with it (repoint a Deployment to
/// use it as a permanent, ongoing data volume). That mismatch, not any particular delete
/// ordering, is what caused the restored PVC to vanish out from under running pods, observed
/// live and reproduced twice even after two different delete-ordering fixes. With Direct +
/// destinationPVC, the PVC is ours from creation onward; VolSync only ever writes into it.
async fn ensure_destination_pvc(
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
async fn delete_pvc_and_wait(namespace: &str, pvc_name: &str) -> anyhow::Result<()> {
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

// ── Disaster-recovery restore ────────────────────────────────────────────────

/// Polls until CephFilesystem yolab-fs reports phase=Ready (filesystem is mountable).
/// Returns immediately if already ready. Creating a PVC on a down CephFS hangs the
/// CSI provisioner indefinitely, so DR restore gates on this before any PVC work.
async fn wait_for_cephfs_ready(timeout_secs: u64) -> anyhow::Result<()> {
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

#[derive(Deserialize)]
pub struct DrStartBody {
    pub snapshot_id: String,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub namespaces: Vec<String>,
}

/// POST /api/backups/dr/start
///
/// Single DR restore path for all scenarios:
///   - Storage healthy: restores one or all namespaces from a snapshot, in place.
///   - Storage recovering (lost disk): waits up to 10 min for CephFS to become
///     mountable, then restores. The reconciler (purge_drained_osds + Rook) handles
///     OSD cleanup in the background; we just gate on the result.
///
/// Flow:
///   1. Extract catalog.json from snapshot (PVC list + total size).
///   2. Space pre-flight: ceph df available bytes vs catalog total_pvc_bytes × 1.2.
///   3. Wait for CephFilesystem phase=Ready (instant if storage is healthy).
///   4. For each namespace: apply K8s YAML (deploy/svc/secret/configmap),
///      scale all deployments to 0, delete PVCs, create fresh PVCs, create RDs.
///   5. Background reconciler (reconcile_restores) scales each deployment back up
///      when its PVC's ReplicationDestination reports Successful.
pub async fn dr_start(
    State(_state): State<AppState>,
    Json(body): Json<DrStartBody>,
) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;

    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else {
        return Err(anyhow::anyhow!("backup not configured").into());
    };
    let cfg = BackupConfig {
        access_key_id:     data.get("access_key_id").cloned().unwrap_or_default(),
        secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
        bucket:            data.get("bucket").cloned().unwrap_or_default(),
        endpoint:          data.get("endpoint").cloned().unwrap_or_default(),
        restic_password:   data.get("restic_password").cloned().unwrap_or_default(),
    };
    let repo = cfg.restic_repo("cluster-backup");

    // ── 1. Extract catalog from snapshot (synchronous — needed for pre-flight) ─
    let cat_target = format!("/tmp/yolab-dr-catalog-{}", random_hex(8));
    let restore_out = Command::new("restic")
        .args(["restore", &body.snapshot_id, "--target", &cat_target, "--include", "**/catalog.json"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await
        .map_err(|e| anyhow::anyhow!("restic unavailable: {e}"))?;
    if !restore_out.status.success() {
        let _ = tokio::fs::remove_dir_all(&cat_target).await;
        return Err(anyhow::anyhow!(
            "could not extract catalog from snapshot: {}",
            String::from_utf8_lossy(&restore_out.stderr).trim()
        ).into());
    }
    let find_out = Command::new("find")
        .args([&cat_target, "-name", "catalog.json", "-type", "f"])
        .output().await.map_err(|e| anyhow::anyhow!("find: {e}"))?;
    let cat_path = String::from_utf8_lossy(&find_out.stdout).trim().to_string();
    let catalog: serde_json::Value = if !cat_path.is_empty() {
        tokio::fs::read(&cat_path).await.ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let _ = tokio::fs::remove_dir_all(&cat_target).await;

    // ── 2. Resolve namespace list ─────────────────────────────────────────────
    let namespaces: Vec<String> = if body.all {
        catalog["namespaces"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    } else {
        body.namespaces.clone()
    };
    if namespaces.is_empty() {
        return Err(anyhow::anyhow!(
            "no namespaces found — snapshot may predate this feature, or pass namespaces[] explicitly"
        ).into());
    }

    // ── 3. Space pre-flight (ceph df talks to MON, available even mid-recovery) ─
    let total_pvc_bytes = catalog["total_pvc_bytes"].as_u64().unwrap_or(0);
    if total_pvc_bytes > 0 {
        match crate::kubectl::ceph_exec(&["df", "-f", "json"]).await {
            Ok(df_raw) => {
                if let Ok(df) = serde_json::from_str::<serde_json::Value>(&df_raw) {
                    let avail = df["stats"]["total_avail_bytes"].as_u64().unwrap_or(u64::MAX);
                    let need  = total_pvc_bytes * 6 / 5;
                    if avail < need {
                        return Err(anyhow::anyhow!(
                            "insufficient storage: {avail} bytes available, ~{need} bytes needed \
                             ({total_pvc_bytes} bytes of PVC data + 20% headroom). \
                             Add more disks or reduce replication before restoring."
                        ).into());
                    }
                    tracing::info!("dr: space pre-flight ok — {avail} avail, {total_pvc_bytes} needed");
                }
            }
            Err(e) => tracing::warn!("dr: space pre-flight skipped (ceph unavailable: {e})"),
        }
    }

    // ── 4. Acquire DR lock before spawning — restore_in_progress() is true
    //       from this point onward, not only after the first RD is created. ────
    let Some(guard) = DrRestoreGuard::acquire().await else {
        return Err(anyhow::anyhow!("A restore is already in progress.").into());
    };

    let snapshot_id = body.snapshot_id.clone();

    // ── 5. Background: wait for CephFS + full per-namespace restore ───────────
    // Moved out of the handler so a gateway/proxy timeout (~60 s) cannot cancel
    // the restore mid-flight. CephFS recovery alone can take up to 600 s;
    // the per-namespace restore loop adds further time per PVC (delete + restic
    // restore from B2). The frontend polls /api/backups/state for progress.
    tokio::spawn(async move {
        let _guard = guard; // DrRestoreGuard released when this task exits

        if let Err(e) = wait_for_cephfs_ready(600).await {
            tracing::warn!("dr: aborting restore — {e}");
            return;
        }

        let restore_as_of: Option<String> = Command::new("restic")
            .args(["snapshots", &snapshot_id, "--json"])
            .env("RESTIC_REPOSITORY", &repo)
            .env("RESTIC_PASSWORD", &cfg.restic_password)
            .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
            .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
            .output().await.ok()
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| v.as_array()?.first()?["time"].as_str().map(String::from));

        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();

        for ns in &namespaces {
            let ns_exists = Command::new("kubectl")
                .args(["get", "namespace", ns]).output().await
                .map(|o| o.status.success()).unwrap_or(false);
            if !ns_exists {
                if let Err(e) = kubectl_apply(&serde_json::json!({
                    "apiVersion": "v1", "kind": "Namespace",
                    "metadata": { "name": ns, "labels": { "yolab.io/managed": "true" } }
                }).to_string()).await {
                    tracing::warn!("dr: {ns}: create namespace: {e}");
                    continue;
                }
            }

            // Apply K8s objects from snapshot YAML. The backup no longer includes PVCs or
            // ReplicationSources — those had stale claimRefs/triggers that caused apply
            // failures. Deployments, Services, Secrets, and ConfigMaps are safe to re-apply.
            {
                let yaml_target = format!("/tmp/yolab-dr-yaml-{}", random_hex(8));
                let pattern = format!("**/{ns}.yaml");
                let r = Command::new("restic")
                    .args(["restore", &snapshot_id, "--target", &yaml_target, "--include", &pattern])
                    .env("RESTIC_REPOSITORY", &repo)
                    .env("RESTIC_PASSWORD", &cfg.restic_password)
                    .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
                    .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                    .output().await;
                if let Ok(o) = r {
                    if o.status.success() {
                        let f = Command::new("find")
                            .args([&yaml_target, "-name", &format!("{ns}.yaml"), "-type", "f"])
                            .output().await;
                        if let Ok(f) = f {
                            let yaml_path = String::from_utf8_lossy(&f.stdout).trim().to_string();
                            if !yaml_path.is_empty() {
                                if let Ok(bytes) = tokio::fs::read(&yaml_path).await {
                                    if let Err(e) = kubectl_apply(&String::from_utf8_lossy(&bytes)).await {
                                        tracing::warn!("dr: {ns}: YAML apply partial: {e}");
                                    } else {
                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                    }
                                }
                            }
                        }
                    }
                }
                let _ = tokio::fs::remove_dir_all(&yaml_target).await;
            }

            let _ = Command::new("kubectl")
                .args(["scale", "deployment", "--all", "-n", ns, "--replicas=0"])
                .output().await;

            let catalog_pvcs: Vec<(String, String)> = catalog["services"].as_array()
                .and_then(|svcs| svcs.iter().find(|s| s["namespace"].as_str() == Some(ns.as_str())))
                .and_then(|s| s["pvcs"].as_array())
                .map(|pvcs| pvcs.iter().filter_map(|p| {
                    let name     = p["name"].as_str()?.to_string();
                    let capacity = p["capacity"].as_str().unwrap_or("10Gi").to_string();
                    Some((name, capacity))
                }).collect())
                .unwrap_or_default();

            if catalog_pvcs.is_empty() {
                tracing::info!("dr: {ns}: no PVCs — YAML applied only");
                continue;
            }

            for (pvc_name, capacity) in &catalog_pvcs {
                if let Err(e) = ensure_restic_secret(ns, pvc_name, &cfg).await {
                    tracing::warn!("dr: {ns}/{pvc_name}: restic secret: {e}");
                }
                let _ = ensure_replication_source(
                    &PvcInfo { namespace: ns.clone(), name: pvc_name.clone() }, false,
                ).await;

                let dest_name = format!("emergency-restore-{}", canonical_pvc_id(pvc_name));
                let rd_exists = Command::new("kubectl")
                    .args(["get", "replicationdestination", &dest_name, "-n", ns])
                    .output().await.map(|o| o.status.success()).unwrap_or(false);
                if rd_exists {
                    tracing::info!("dr: {ns}/{pvc_name}: already in progress — skipping");
                    continue;
                }

                let pvc_repo = cfg.restic_repo(&format!("volsync/{ns}/{}", canonical_pvc_id(pvc_name)));
                let has_snapshot = Command::new("restic")
                    .args(["snapshots", "--json", "--last"])
                    .env("RESTIC_REPOSITORY", &pvc_repo)
                    .env("RESTIC_PASSWORD", &cfg.restic_password)
                    .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
                    .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                    .output().await.ok()
                    .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                    .and_then(|v| v.as_array().map(|a| !a.is_empty()))
                    .unwrap_or(false);
                if !has_snapshot {
                    tracing::warn!(
                        "dr: {ns}/{pvc_name}: no backup snapshot found — PVC preserved. \
                         Run a backup first before restoring."
                    );
                    continue;
                }

                if let Err(e) = delete_pvc_and_wait(ns, pvc_name).await {
                    tracing::warn!("dr: {ns}/{pvc_name}: delete pvc: {e}");
                    continue;
                }
                if let Err(e) = ensure_destination_pvc(
                    pvc_name, ns, capacity, "yolab-cephfs", "ReadWriteMany",
                ).await {
                    tracing::warn!("dr: {ns}/{pvc_name}: create pvc: {e}");
                    continue;
                }

                let secret_name = format!("{}{RESTIC_SECRET_SUFFIX}", canonical_pvc_id(pvc_name));
                let mut restic_spec = serde_json::json!({
                    "repository": secret_name,
                    "copyMethod": "Direct",
                    "cacheStorageClassName": "yolab-cephfs",
                    "destinationPVC": pvc_name,
                    "moverSecurityContext": { "runAsUser": 0, "runAsGroup": 0, "fsGroup": 0 }
                });
                if let Some(ref t) = restore_as_of {
                    restic_spec["restoreAsOf"] = serde_json::Value::String(t.clone());
                }
                let manifest = serde_json::json!({
                    "apiVersion": "volsync.backube/v1alpha1",
                    "kind": "ReplicationDestination",
                    "metadata": {
                        "name": dest_name, "namespace": ns,
                        "labels": { "app.kubernetes.io/managed-by": "yolab" }
                    },
                    "spec": {
                        "trigger": { "manual": format!("dr-{timestamp}") },
                        "restic": restic_spec
                    }
                });
                match kubectl_apply(&manifest.to_string()).await {
                    Ok(_)  => tracing::info!("dr: {ns}/{pvc_name}: ReplicationDestination created"),
                    Err(e) => tracing::warn!("dr: {ns}/{pvc_name}: RD: {e}"),
                }
            }
        }

        tracing::info!("dr: restore setup complete for {} namespace(s)", namespaces.len());
    });

    Ok(Json(serde_json::json!({ "ok": true, "started": true })))
}

/// GET /api/backups/dr/status
/// Returns the status of all in-progress emergency restores (emergency-restore-* RDs).
pub async fn dr_status(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let out = Command::new("kubectl")
        .args(["get", "replicationdestination", "-A", "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!({"items": []}));

    let restores: Vec<serde_json::Value> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            let name = item["metadata"]["name"].as_str()?.to_string();
            if !name.starts_with("emergency-restore-") {
                return None;
            }
            let namespace = item["metadata"]["namespace"].as_str()?.to_string();
            let pvc_name = name.strip_prefix("emergency-restore-")?.to_string();
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or("Running")
                .to_string();
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            Some(serde_json::json!({
                "namespace": namespace,
                "pvc": pvc_name.clone(),
                "result": result,
                "last_sync_time": last_sync_time,
                "restored_pvc": pvc_name,
            }))
        })
        .collect();

    let total = restores.len();
    let done = restores
        .iter()
        .filter(|r| r["result"].as_str().unwrap_or("").to_lowercase() == "successful")
        .count();
    let failed = restores
        .iter()
        .filter(|r| r["result"].as_str().unwrap_or("").to_lowercase() == "failed")
        .count();

    Ok(Json(serde_json::json!({
        "restores": restores,
        "total": total,
        "done": done,
        "failed": failed,
        "all_complete": total > 0 && done + failed == total,
    })))
}

/// Finds every restore whose data pull has succeeded but hasn't been applied yet (its
/// ReplicationDestination still exists), and finishes each one via apply_one_restore().
/// Used both by the manual POST /api/backups/dr/apply endpoint and by the background
/// reconciler — restores complete the same way regardless of who/what triggers the check.
async fn reconcile_restores() -> (Vec<String>, Vec<String>) {
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let Ok(out) = Command::new("kubectl")
        .args(["get", "replicationdestination", "-A", "-o", "json"])
        .output()
        .await
    else {
        return (applied, errors);
    };
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!({"items": []}));

    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let name = match item["metadata"]["name"].as_str() {
            Some(n) if n.starts_with("emergency-restore-") => n.to_string(),
            _ => continue,
        };
        let namespace = item["metadata"]["namespace"].as_str().unwrap_or("").to_string();
        let pvc_name = match name.strip_prefix("emergency-restore-") {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => continue,
        };

        let result = item["status"]["latestMoverStatus"]["result"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();

        if result == "failed" {
            // The restore mover failed — the PVC exists but is empty.
            // Scale deployments back up so the app is at least running (even
            // with empty data) rather than stuck at 0 replicas silently forever.
            let deployments = find_deployments_for_pvc(&namespace, &pvc_name)
                .await
                .unwrap_or_default();
            for deploy in &deployments {
                let _ = scale_deployment(&namespace, deploy, 1).await;
            }
            delete_replication_destination_without_touching_pvc(&name, &namespace).await;
            errors.push(format!(
                "{namespace}/{pvc_name}: restore FAILED — app restarted with empty volume. \
                 Check VolSync mover logs and re-run DR when storage is healthy."
            ));
            continue;
        }

        if result != "successful" {
            continue;
        }

        match apply_one_restore(&namespace, &name, &pvc_name).await {
            Ok(_) => applied.push(format!("{namespace}/{pvc_name}")),
            Err(e) => errors.push(format!("{namespace}/{pvc_name}: {e}")),
        }
    }

    (applied, errors)
}

/// POST /api/backups/dr/apply — apply right now instead of waiting for the reconciler's next
/// tick. Nothing depends on this being called anymore; it's a convenience for testing/impatience.
pub async fn dr_apply(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let (applied, errors) = reconcile_restores().await;
    Ok(Json(serde_json::json!({ "applied": applied, "errors": errors })))
}

/// Background reconciler — periodically finishes any restore whose data pull already
/// succeeded but hasn't been applied yet. Makes restores fully server-driven: once triggered
/// (emergency_restore, dr_start, restore_from_snapshot), they complete on their own, whether
/// or not any client stays connected to watch — this is what makes a browser tab closing (or
/// simply not being open) harmless instead of leaving the restore stuck forever.
pub async fn run_restore_reconciler() {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let (applied, errors) = reconcile_restores().await;
        for a in &applied {
            tracing::info!("restore-reconciler: applied {a}");
        }
        for e in &errors {
            tracing::warn!("restore-reconciler: {e}");
        }
    }
}

// ── Background cluster backup task ───────────────────────────────────────────

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
fn sanitize_k8s_items_for_backup(items: &[serde_json::Value]) -> Vec<serde_json::Value> {
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

fn parse_capacity_bytes(s: &str) -> u64 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("Ti") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Gi") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Mi") { return n.trim().parse::<u64>().unwrap_or(0) * 1024 * 1024; }
    if let Some(n) = s.strip_suffix("Ki") { return n.trim().parse::<u64>().unwrap_or(0) * 1024; }
    s.parse::<u64>().unwrap_or(0)
}

/// Core backup logic — called by both the daily scheduler and the manual trigger.
async fn do_cluster_backup() -> anyhow::Result<String> {
    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else {
        anyhow::bail!("backup not configured");
    };

    let key_id          = data.get("access_key_id").cloned().unwrap_or_default();
    let secret_key      = data.get("secret_access_key").cloned().unwrap_or_default();
    let bucket          = data.get("bucket").cloned().unwrap_or_default();
    let endpoint        = data.get("endpoint").cloned().unwrap_or_default();
    let restic_password = data.get("restic_password").cloned().unwrap_or_default();

    if key_id.is_empty() || restic_password.is_empty() {
        anyhow::bail!("missing credentials or restic_password");
    }

    let date    = chrono::Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let tmp_dir = format!("/tmp/yolab-cluster-backup-{date}");
    let repo    = format!("s3:{}/{}/cluster-backup", endpoint.trim_end_matches('/'), bucket);

    tokio::fs::create_dir_all(&tmp_dir).await?;

    // 1. etcd snapshot — archived as etcd.db in this restic snapshot.
    //    NOTE: etcd.db is NOT consumed by dr_start. It is used exclusively by
    //    the external dr-restore.sh script, which runs before K3s/local-api are
    //    started and restores the etcd database directly.
    let snap_name = format!("yolab-cluster-{date}");
    let snap_saved = Command::new("k3s")
        .args(["etcd-snapshot", "save", &format!("--name={snap_name}")])
        .output().await;

    match snap_saved {
        Ok(o) if o.status.success() => {
            let snap_dir = "/var/lib/rancher/k3s/server/db/snapshots";
            if let Ok(entries) = std::fs::read_dir(snap_dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name();
                    let fname_str = fname.to_string_lossy();
                    if fname_str.starts_with(&snap_name) {
                        let dst = format!("{tmp_dir}/etcd.db");
                        if let Err(e) = std::fs::copy(entry.path(), &dst) {
                            tracing::warn!("cluster-backup: copy etcd snapshot: {e}");
                        } else {
                            // Delete the local snapshot file now that it's been copied into
                            // the backup staging dir (restic will upload it). Without this,
                            // snapshots accumulate on disk indefinitely.
                            let _ = std::fs::remove_file(entry.path());
                        }
                        let _ = Command::new("kubectl")
                            .args(["delete", "etcdsnapshotfile", &fname_str.to_string(), "--ignore-not-found"])
                            .output().await;
                        break;
                    }
                }
            }
        }
        Ok(o) => tracing::warn!("cluster-backup: etcd-snapshot: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => tracing::warn!("cluster-backup: k3s unavailable: {e}"),
    }

    // 2. Export K8s objects for all yolab-managed namespaces.
    let ns_out = Command::new("kubectl")
        .args(["get", "namespaces", "-l", "yolab.io/managed=true",
               "-o", "jsonpath={.items[*].metadata.name}"])
        .output().await;

    let mut namespaces: Vec<String> = Vec::new();
    if let Ok(o) = ns_out {
        for ns in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            namespaces.push(ns.to_string());
            let obj_out = Command::new("kubectl")
                .args(["get", "deploy,svc,secret,configmap",
                       "-n", ns, "-o", "json", "--ignore-not-found"])
                .output().await;
            if let Ok(obj) = obj_out {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&obj.stdout) {
                    let raw = v["items"].as_array().cloned().unwrap_or_default();
                    let sanitized = sanitize_k8s_items_for_backup(&raw);
                    if !sanitized.is_empty() {
                        let list = serde_json::json!({
                            "apiVersion": "v1",
                            "kind": "List",
                            "items": sanitized,
                        });
                        if let Ok(s) = serde_json::to_string_pretty(&list) {
                            let _ = tokio::fs::write(format!("{tmp_dir}/{ns}.yaml"), s.as_bytes()).await;
                        }
                    }
                }
            }
        }
    }

    // 3. catalog.json — includes per-namespace PVC info so the restore UI can
    //    show service names, PVC counts, and storage sizes without extra API calls.
    let mut services: Vec<serde_json::Value> = Vec::new();
    for ns in &namespaces {
        let pvc_out = Command::new("kubectl")
            .args(["get", "pvc", "-n", ns, "-o", "json"])
            .output().await;
        let pvcs: Vec<serde_json::Value> = pvc_out
            .ok()
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let name = item["metadata"]["name"].as_str()?.to_string();
                // Same exclusion as list_user_pvcs(): VolSync's own restic-cache PVCs live in
                // the same namespace as the real app data and shouldn't be listed as a "service".
                if name.starts_with("volsync-") {
                    return None;
                }
                let capacity = item["spec"]["resources"]["requests"]["storage"]
                    .as_str().unwrap_or("?").to_string();
                Some(serde_json::json!({ "name": name, "capacity": capacity }))
            })
            .collect();
        services.push(serde_json::json!({ "namespace": ns, "pvcs": pvcs }));
    }
    let total_pvc_bytes: u64 = services.iter()
        .flat_map(|s| s["pvcs"].as_array().cloned().unwrap_or_default())
        .map(|p| parse_capacity_bytes(p["capacity"].as_str().unwrap_or("0")))
        .sum();
    let catalog = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "namespaces": namespaces,
        "services": services,
        "total_pvc_bytes": total_pvc_bytes,
    });
    let _ = tokio::fs::write(format!("{tmp_dir}/catalog.json"), catalog.to_string()).await;

    // 4. Init restic repo if needed.
    let check = Command::new("restic").args(["snapshots"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &restic_password)
        .env("AWS_ACCESS_KEY_ID", &key_id).env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .output().await;
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        let init = Command::new("restic").args(["init"])
            .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &restic_password)
            .env("AWS_ACCESS_KEY_ID", &key_id).env("AWS_SECRET_ACCESS_KEY", &secret_key)
            .output().await?;
        if !init.status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            anyhow::bail!("restic init failed: {}", String::from_utf8_lossy(&init.stderr).trim());
        }
    }

    // 5. Backup.
    let backup = Command::new("restic")
        .args(["backup", &tmp_dir, "--tag", "cluster-backup"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &restic_password)
        .env("AWS_ACCESS_KEY_ID", &key_id).env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .output().await?;

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    if !backup.status.success() {
        anyhow::bail!("restic backup failed: {}", String::from_utf8_lossy(&backup.stderr).trim());
    }

    tracing::info!("cluster-backup: snapshot complete ({date})");
    record_cluster_backup_success();

    // 6. Prune old snapshots.
    let _ = Command::new("restic")
        .args(["forget", "--tag", "cluster-backup",
               "--keep-daily", "7", "--keep-weekly", "4", "--keep-monthly", "12", "--prune"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &restic_password)
        .env("AWS_ACCESS_KEY_ID", &key_id).env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .output().await;

    Ok(date)
}

const LAST_BACKUP_TS_FILE: &str = "/var/lib/yolab/last-cluster-backup";

/// Returns hours since the most recent successful cluster backup, or a large value
/// if no backup has ever run. Written by do_cluster_backup() on success.
fn last_cluster_backup_age_hours() -> i64 {
    let ts: i64 = std::fs::read_to_string(LAST_BACKUP_TS_FILE)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if ts == 0 {
        return i64::MAX;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    (now - ts) / 3600
}

fn record_cluster_backup_success() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Ensure the directory exists — on a brand-new install it may not yet.
    let _ = std::fs::create_dir_all("/var/lib/yolab");
    let _ = std::fs::write(LAST_BACKUP_TS_FILE, now.to_string());
}

/// Daily scheduler — sleeps until 02:00 UTC then calls do_cluster_backup.
/// On startup, immediately runs a catch-up backup if the last one was >23 hours ago,
/// so a node coming back after extended downtime does not wait another full day.
pub async fn run_cluster_backup(_config: Arc<Config>) {
    // Give K3s and Ceph ~60 seconds to settle after (re)start before the first backup attempt.
    tokio::time::sleep(Duration::from_secs(60)).await;

    if last_cluster_backup_age_hours() > 23 && !restore_in_progress().await {
        if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
            tracing::info!("cluster-backup: missed backup detected — running catch-up now");
            if let Some(_guard) = ClusterBackupGuard::acquire().await {
                let restic_password = data.get("restic_password").cloned().unwrap_or_default();
                if !restic_password.is_empty() {
                    let cfg = BackupConfig {
                        access_key_id:     data.get("access_key_id").cloned().unwrap_or_default(),
                        secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
                        bucket:            data.get("bucket").cloned().unwrap_or_default(),
                        endpoint:          data.get("endpoint").cloned().unwrap_or_default(),
                        restic_password,
                    };
                    let pvcs = list_user_pvcs().await.unwrap_or_default();
                    let since = chrono::Utc::now();
                    trigger_and_wait_volsync(&cfg, &pvcs, since).await;
                }
                if let Err(e) = do_cluster_backup().await {
                    tracing::warn!("cluster-backup catch-up: {e}");
                }
            }
        }
    }

    loop {
        let now  = chrono::Utc::now();
        let next = {
            let today_2am = now.date_naive().and_hms_opt(2, 0, 0).unwrap().and_utc();
            if now < today_2am { today_2am }
            else {
                (now.date_naive() + chrono::Duration::days(1))
                    .and_hms_opt(2, 0, 0).unwrap().and_utc()
            }
        };
        tokio::time::sleep(Duration::from_secs(
            (next - now).num_seconds().max(0) as u64,
        )).await;

        if restore_in_progress().await {
            tracing::warn!("cluster-backup: skipping scheduled run — a restore is in progress");
            continue;
        }
        let Some(_guard) = ClusterBackupGuard::acquire().await else {
            tracing::warn!("cluster-backup: skipping scheduled run — a backup is already running");
            continue;
        };
        // Trigger VolSync for all PVCs before capturing the cluster snapshot so
        // both K8s state and PVC filesystem data come from the same session.
        if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
            let restic_password = data.get("restic_password").cloned().unwrap_or_default();
            if !restic_password.is_empty() {
                let cfg = BackupConfig {
                    access_key_id:     data.get("access_key_id").cloned().unwrap_or_default(),
                    secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
                    bucket:            data.get("bucket").cloned().unwrap_or_default(),
                    endpoint:          data.get("endpoint").cloned().unwrap_or_default(),
                    restic_password,
                };
                let pvcs = list_user_pvcs().await.unwrap_or_default();
                let since = chrono::Utc::now();
                trigger_and_wait_volsync(&cfg, &pvcs, since).await;
            }
        }
        if let Err(e) = do_cluster_backup().await {
            tracing::warn!("cluster-backup: {e}");
        }
    }
}

/// Triggers VolSync for every user PVC and waits for all to finish.
/// `since` is set just before triggering — prevents accepting a stale
/// Successful from a previous session as completion proof.
async fn trigger_and_wait_volsync(
    cfg: &BackupConfig,
    pvcs: &[PvcInfo],
    since: chrono::DateTime<chrono::Utc>,
) {
    for pvc in pvcs {
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, cfg).await;
        let _ = ensure_replication_source(pvc, true).await;
    }
    if let Err(e) = wait_for_volsync_sync(pvcs, since, 1800).await {
        tracing::warn!("volsync wait: {e} — proceeding with cluster backup anyway");
    }
}

/// POST /api/backups/cluster/run-now — manual trigger.
/// Triggers VolSync for every PVC, waits for all to reach Successful, then
/// runs the cluster-metadata backup. Returns a single snapshot timestamp that
/// represents both K8s state and PVC filesystem data.
pub async fn run_backup_now(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if restore_in_progress().await {
        return Err(anyhow::anyhow!("A restore is currently in progress — try again once it finishes.").into());
    }
    if volsync_backup_in_progress().await {
        return Err(anyhow::anyhow!("VolSync is already backing up PVC data — try again once it finishes.").into());
    }
    let Some(guard) = ClusterBackupGuard::acquire().await else {
        return Err(anyhow::anyhow!("A backup is already running.").into());
    };
    let creds = ye_creds(&state.config);

    // Run the backup in a detached task so it survives the HTTP request ending.
    // A manual backup takes minutes (VolSync sync + cluster snapshot). If we did
    // this work inline, a gateway/proxy timeout (~60s) or the user navigating away
    // would cancel the handler future at its next await point, drop the guard, and
    // abandon the backup *after* VolSync had already run — leaving PVC data backed
    // up but no cluster snapshot, and nothing in the backup list. The guard is moved
    // into the task so the cluster-wide lock is held for the real duration of the
    // work, not the lifetime of the request. The frontend polls /api/backups/state.
    tokio::spawn(async move {
        let _guard = guard; // released only when this task finishes
        if let Some((url, token)) = creds {
            match ensure_master_config(&url, &token).await {
                Ok(cfg) => {
                    let pvcs = list_user_pvcs().await.unwrap_or_default();
                    let since = chrono::Utc::now();
                    trigger_and_wait_volsync(&cfg, &pvcs, since).await;
                }
                Err(e) => tracing::warn!("run_backup_now: master config: {e}"),
            }
        }
        match do_cluster_backup().await {
            Ok(date) => tracing::info!("run_backup_now: cluster snapshot complete ({date})"),
            Err(e)   => tracing::warn!("run_backup_now: cluster backup failed: {e}"),
        }
    });

    Ok(Json(serde_json::json!({ "ok": true, "started": true })))
}

/// GET /api/backups/snapshots — list available cluster-backup restic snapshots.
/// Returns timestamps the restore UI can offer as restore points.
pub async fn list_snapshots(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else {
        return Ok(Json(serde_json::json!({ "snapshots": [], "configured": false })));
    };

    let key_id          = data.get("access_key_id").cloned().unwrap_or_default();
    let secret_key      = data.get("secret_access_key").cloned().unwrap_or_default();
    let bucket          = data.get("bucket").cloned().unwrap_or_default();
    let endpoint        = data.get("endpoint").cloned().unwrap_or_default();
    let restic_password = data.get("restic_password").cloned().unwrap_or_default();

    if restic_password.is_empty() {
        return Ok(Json(serde_json::json!({ "snapshots": [], "configured": false })));
    }

    let repo = format!("s3:{}/{}/cluster-backup", endpoint.trim_end_matches('/'), bucket);

    let out = Command::new("restic")
        .args(["snapshots", "--json", "--tag", "cluster-backup"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &restic_password)
        .env("AWS_ACCESS_KEY_ID", &key_id)
        .env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("restic not available: {e}"))?;

    if !out.status.success() {
        // Repo not initialised yet — no snapshots exist.
        return Ok(Json(serde_json::json!({ "snapshots": [], "configured": true })));
    }

    let snapshots: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!([]));

    Ok(Json(serde_json::json!({ "snapshots": snapshots, "configured": true })))
}

/// GET /api/backups/snapshots/:id/catalog
/// Extracts catalog.json from a specific restic cluster-backup snapshot.
/// Returns { timestamp, namespaces: [...] }
pub async fn snapshot_catalog(
    State(_state): State<AppState>,
    Path(snapshot_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else {
        return Err(anyhow::anyhow!("backup not configured").into());
    };
    let key_id          = data.get("access_key_id").cloned().unwrap_or_default();
    let secret_key      = data.get("secret_access_key").cloned().unwrap_or_default();
    let bucket          = data.get("bucket").cloned().unwrap_or_default();
    let endpoint        = data.get("endpoint").cloned().unwrap_or_default();
    let restic_password = data.get("restic_password").cloned().unwrap_or_default();
    let repo = format!("s3:{}/{}/cluster-backup", endpoint.trim_end_matches('/'), bucket);

    let target = format!("/tmp/yolab-catalog-{}", random_hex(8));

    let restore_out = Command::new("restic")
        .args(["restore", &snapshot_id, "--target", &target, "--include", "**/catalog.json"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &restic_password)
        .env("AWS_ACCESS_KEY_ID", &key_id)
        .env("AWS_SECRET_ACCESS_KEY", &secret_key)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("restic not available: {e}"))?;

    if !restore_out.status.success() {
        let _ = tokio::fs::remove_dir_all(&target).await;
        return Err(anyhow::anyhow!(
            "restic restore failed: {}",
            String::from_utf8_lossy(&restore_out.stderr).trim()
        ).into());
    }

    let find_out = Command::new("find")
        .args([&target, "-name", "catalog.json", "-type", "f"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("find failed: {e}"))?;

    let file_path = String::from_utf8_lossy(&find_out.stdout).trim().to_string();
    let catalog: serde_json::Value = if file_path.is_empty() {
        serde_json::json!({"namespaces": [], "timestamp": null})
    } else {
        let bytes = tokio::fs::read(&file_path).await
            .map_err(|e| anyhow::anyhow!("read catalog.json: {e}"))?;
        serde_json::from_slice(&bytes)
            .unwrap_or(serde_json::json!({"namespaces": [], "timestamp": null}))
    };

    let _ = tokio::fs::remove_dir_all(&target).await;
    Ok(Json(catalog))
}

/// Background task: every 10 minutes, ensure every user PVC has a VolSync
/// ReplicationSource and restic secret, if backup is already configured.
///
/// This is "backups on by default" — newly installed apps get covered
/// automatically within one reconcile cycle (≤10 min) after the master
/// backup config exists. The apply is idempotent so re-running over already
/// configured PVCs is harmless.
/// Creates the restic secret and ReplicationSource for a single namespace at
/// install time. Called by apps.rs immediately after the namespace is created.
/// `namespace` is the raw app namespace (e.g. "yolab-gitea"), not the instance name.
pub async fn setup_namespace_backup(namespace: &str) {
    let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else { return };
    let restic_password = data.get("restic_password").cloned().unwrap_or_default();
    if restic_password.is_empty() { return }
    let cfg = BackupConfig {
        access_key_id:     data.get("access_key_id").cloned().unwrap_or_default(),
        secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
        bucket:            data.get("bucket").cloned().unwrap_or_default(),
        endpoint:          data.get("endpoint").cloned().unwrap_or_default(),
        restic_password,
    };
    let Ok(pvcs) = list_user_pvcs().await else { return };
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await;
        let _ = ensure_replication_source(&pvc, false).await;
    }
}

/// One-time migration: patches the independent `schedule` out of any managed
/// ReplicationSources that still carry the old `0 3 * * *` cron trigger.
/// After this, RSes only fire when the backup job explicitly stamps a `manual` trigger.
async fn strip_rs_schedules() {
    let out = match Command::new("kubectl")
        .args(["get", "replicationsource", "-A",
               "-l", "app.kubernetes.io/managed-by=yolab", "-o", "json"])
        .output().await
    {
        Ok(o) => o,
        Err(_) => return,
    };
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!({"items": []}));
    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let name = item["metadata"]["name"].as_str().unwrap_or("");
        let ns   = item["metadata"]["namespace"].as_str().unwrap_or("");
        if name.is_empty() || ns.is_empty() { continue; }
        if item["spec"]["trigger"]["schedule"].as_str().is_none() { continue; }
        tracing::info!("backup-reconciler: {ns}/{name} — removing legacy schedule trigger");
        let _ = Command::new("kubectl")
            .args([
                "patch", "replicationsource", name, "-n", ns,
                "--type=json",
                r#"-p=[{"op":"remove","path":"/spec/trigger/schedule"}]"#,
            ])
            .output().await;
    }
}

/// Self-healing reconciler — runs hourly to catch any PVCs whose RS/secret was
/// missed at install time (e.g. race between app deploy and local-api restart).
/// Never overwrites a live manual trigger set by the backup job (ensure_replication_source
/// skips PVCs where an RS already exists when trigger_now=false).
pub async fn run_replication_source_reconciler() {
    tokio::time::sleep(Duration::from_secs(120)).await;
    // Migrate any pre-existing RSes that still carry an independent schedule.
    strip_rs_schedules().await;
    loop {
        if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
            let restic_password = data.get("restic_password").cloned().unwrap_or_default();
            if !restic_password.is_empty() {
                let cfg = BackupConfig {
                    access_key_id:     data.get("access_key_id").cloned().unwrap_or_default(),
                    secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
                    bucket:            data.get("bucket").cloned().unwrap_or_default(),
                    endpoint:          data.get("endpoint").cloned().unwrap_or_default(),
                    restic_password,
                };
                if let Ok(pvcs) = list_user_pvcs().await {
                    for pvc in pvcs {
                        if let Err(e) = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await {
                            tracing::debug!("backup-reconciler: restic secret {}/{}: {e}", pvc.namespace, pvc.name);
                        }
                        if let Err(e) = ensure_replication_source(&pvc, false).await {
                            tracing::debug!("backup-reconciler: RS {}/{}: {e}", pvc.namespace, pvc.name);
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
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
