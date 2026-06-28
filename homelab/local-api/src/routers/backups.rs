use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{config::Config, error::Result, AppState};

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

async fn kubectl_apply(manifest: &str) -> anyhow::Result<()> {
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(manifest.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "kubectl apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Returns the decoded Secret data, trimming trailing whitespace from each value.
async fn kubectl_get_secret(name: &str, ns: &str) -> Option<HashMap<String, String>> {
    let out = Command::new("kubectl")
        .args(["get", "secret", name, "-n", ns, "-o", "json"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let data = v.get("data")?.as_object()?;
    let mut result = HashMap::new();
    for (k, val) in data {
        if let Some(encoded) = val.as_str() {
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                if let Ok(s) = String::from_utf8(bytes) {
                    result.insert(k.clone(), s.trim().to_string());
                }
            }
        }
    }
    Some(result)
}

/// Create or replace a Secret using a JSON manifest (avoids YAML escaping issues).
async fn kubectl_apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
) -> anyhow::Result<()> {
    use base64::Engine as _;
    let data_map: serde_json::Map<String, serde_json::Value> = data
        .iter()
        .map(|(k, v)| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(v.as_bytes());
            (k.to_string(), serde_json::Value::String(b64))
        })
        .collect();

    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": ns,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "type": "Opaque",
        "data": data_map,
    });
    kubectl_apply(&manifest.to_string()).await
}

// ── Rclone password obfuscation ───────────────────────────────────────────────

/// Implements rclone's Obscure so passwords can be embedded in rclone.conf.
/// Algorithm: buf[0] ^= 0x9c; each subsequent byte XORs with the previous; base64url-nopad.
fn rclone_obscure(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return String::new();
    }
    let mut buf = plaintext.as_bytes().to_vec();
    buf[0] ^= 0x9c;
    for i in 1..buf.len() {
        let prev = buf[i - 1];
        buf[i] ^= prev;
    }
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

// ── Master backup config ──────────────────────────────────────────────────────

const MASTER_SECRET: &str = "yolab-backup-config";
const MASTER_NS: &str = "kube-system";
const RCLONE_SECRET: &str = "volsync-rclone";

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
    rclone_password: String,
    rclone_salt: String,
}

impl BackupConfig {
    /// Hostname-only endpoint for K3s etcd-snapshot (strips https://).
    fn s3_host(&self) -> String {
        self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    }
}

async fn ensure_master_config(url: &str, token: &str) -> anyhow::Result<BackupConfig> {
    if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
        return Ok(BackupConfig {
            access_key_id: data.get("access_key_id").cloned().unwrap_or_default(),
            secret_access_key: data.get("secret_access_key").cloned().unwrap_or_default(),
            bucket: data.get("bucket").cloned().unwrap_or_default(),
            endpoint: data.get("endpoint").cloned().unwrap_or_default(),
            rclone_password: data.get("rclone_password").cloned().unwrap_or_default(),
            rclone_salt: data.get("rclone_salt").cloned().unwrap_or_default(),
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

    // Encryption keys generated locally — never sent to yolab-external.
    let rclone_password = random_hex(32);
    let rclone_salt = random_hex(16);

    let cfg = BackupConfig {
        access_key_id: s3.access_key_id.clone(),
        secret_access_key: s3.secret_access_key.clone(),
        bucket: s3.bucket_name.clone(),
        endpoint: s3.endpoint.clone(),
        rclone_password: rclone_password.clone(),
        rclone_salt: rclone_salt.clone(),
    };

    kubectl_apply_secret(
        MASTER_SECRET,
        MASTER_NS,
        &[
            ("access_key_id", &s3.access_key_id),
            ("secret_access_key", &s3.secret_access_key),
            ("bucket", &s3.bucket_name),
            ("endpoint", &s3.endpoint),
            ("rclone_password", &rclone_password),
            ("rclone_salt", &rclone_salt),
        ],
    )
    .await?;

    Ok(cfg)
}

fn build_rclone_conf(cfg: &BackupConfig) -> String {
    format!(
        "[b2]\ntype = b2\naccount = {}\nkey = {}\n\n\
         [b2-crypt]\ntype = crypt\nremote = b2:{}\n\
         filename_encryption = standard\ndirectory_name_encryption = true\n\
         password = {}\npassword2 = {}\n",
        cfg.access_key_id,
        cfg.secret_access_key,
        cfg.bucket,
        rclone_obscure(&cfg.rclone_password),
        rclone_obscure(&cfg.rclone_salt),
    )
}

async fn ensure_rclone_secret(ns: &str, cfg: &BackupConfig) -> anyhow::Result<()> {
    if kubectl_get_secret(RCLONE_SECRET, ns).await.is_some() {
        return Ok(());
    }
    let conf = build_rclone_conf(cfg);
    kubectl_apply_secret(RCLONE_SECRET, ns, &[("rclone.conf", &conf)]).await
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
            Some(PvcInfo { namespace: ns, name })
        })
        .collect())
}

// ── VolSync ReplicationSource ─────────────────────────────────────────────────

async fn ensure_replication_source(pvc: &PvcInfo) -> anyhow::Result<()> {
    let rs_name = format!("volsync-{}", pvc.name);
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
            "trigger": { "schedule": "0 3 * * *" },
            "rclone": {
                "rcloneConfigSection": "b2-crypt",
                "rcloneDestPath": format!("b2-crypt:{}/{}", pvc.namespace, pvc.name),
                "rcloneConfig": RCLONE_SECRET,
                "copyMethod": "Direct",
                "moverSecurityContext": {
                    "runAsUser": 1000,
                    "runAsGroup": 1000,
                    "fsGroup": 1000
                }
            }
        }
    });
    kubectl_apply(&manifest.to_string()).await
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

/// POST /api/backups/s3/enable — idempotent: provisions B2, configures VolSync per PVC.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };

    let cfg = ensure_master_config(&url, &token).await?;
    let pvcs = list_user_pvcs().await.unwrap_or_default();

    let mut namespaces_seen: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();

    for pvc in &pvcs {
        if !namespaces_seen.contains(&pvc.namespace) {
            ensure_rclone_secret(&pvc.namespace, &cfg).await?;
            namespaces_seen.push(pvc.namespace.clone());
        }
        ensure_replication_source(pvc).await?;
        sources.push(format!("{}/{}", pvc.namespace, pvc.name));
    }

    Ok(Json(serde_json::json!({
        "provisioned": true,
        "pvcs_configured": sources,
        "schedule": "daily at 03:00 UTC",
        "etcd_snapshots": "daily at 02:00 UTC (background task)",
    })))
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

    let pvcs: Vec<serde_json::Value> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| {
            let namespace = item["metadata"]["namespace"].as_str().unwrap_or("").to_string();
            let pvc = item["spec"]["sourcePVC"].as_str().unwrap_or("").to_string();
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            let last_sync_duration =
                item["status"]["lastSyncDuration"].as_str().map(String::from);
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or(if last_sync_time.is_some() { "Successful" } else { "Pending" })
                .to_string();
            serde_json::json!({
                "namespace": namespace,
                "pvc": pvc,
                "last_sync_time": last_sync_time,
                "last_sync_duration": last_sync_duration,
                "result": result,
            })
        })
        .collect();

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
    })))
}

/// POST /api/backups/restore/:namespace/:pvc — create a ReplicationDestination.
pub async fn trigger_restore(
    State(_state): State<AppState>,
    Path((namespace, pvc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let dest_name = format!("restore-{pvc}");
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();

    // Read PVC spec to get capacity + storage class + access modes.
    let pvc_out = Command::new("kubectl")
        .args(["get", "pvc", &pvc, "-n", &namespace, "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let pvc_v: serde_json::Value = serde_json::from_slice(&pvc_out.stdout).unwrap_or_default();
    let capacity = pvc_v["spec"]["resources"]["requests"]["storage"]
        .as_str()
        .unwrap_or("10Gi")
        .to_string();
    let storage_class = pvc_v["spec"]["storageClassName"]
        .as_str()
        .unwrap_or("yolab-cephfs")
        .to_string();
    let access_mode = pvc_v["spec"]["accessModes"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|m| m.as_str())
        .unwrap_or("ReadWriteMany")
        .to_string();

    let manifest = serde_json::json!({
        "apiVersion": "volsync.backube/v1alpha1",
        "kind": "ReplicationDestination",
        "metadata": {
            "name": dest_name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "spec": {
            "trigger": { "manual": format!("restore-{timestamp}") },
            "rclone": {
                "rcloneConfigSection": "b2-crypt",
                "rcloneDestPath": format!("b2-crypt:{}/{}", namespace, pvc),
                "rcloneConfig": RCLONE_SECRET,
                "copyMethod": "Snapshot",
                "storageClassName": storage_class,
                "capacity": capacity,
                "accessModes": [access_mode],
            }
        }
    });
    kubectl_apply(&manifest.to_string()).await?;

    Ok(Json(serde_json::json!({
        "started": true,
        "destination_name": dest_name,
        "restored_pvc": format!("volsync-{dest_name}-dst"),
        "note": "When complete, point your app at the restored PVC name above.",
    })))
}

/// GET /api/backups/restore/:namespace/:pvc/status
pub async fn restore_status(
    State(_state): State<AppState>,
    Path((namespace, pvc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let dest_name = format!("restore-{pvc}");
    let out = Command::new("kubectl")
        .args([
            "get",
            "replicationdestination",
            &dest_name,
            "-n",
            &namespace,
            "-o",
            "json",
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if !out.status.success() {
        return Ok(Json(serde_json::json!({ "found": false })));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let result = v["status"]["latestMoverStatus"]["result"]
        .as_str()
        .unwrap_or("Running")
        .to_string();
    let last_sync_time = v["status"]["lastSyncTime"].as_str().map(String::from);
    let restored_pvc = v["status"]["latestImage"]["name"].as_str().map(String::from);

    Ok(Json(serde_json::json!({
        "found": true,
        "result": result,
        "last_sync_time": last_sync_time,
        "restored_pvc": restored_pvc,
    })))
}

// ── Emergency restore helpers ─────────────────────────────────────────────────

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

/// POST /api/backups/restore/:namespace/:pvc/emergency
/// Scales down apps, deletes corrupted PVC to free space, then pulls from B2.
/// No rollback possible — only use when data is already lost/corrupted.
pub async fn emergency_restore(
    State(_state): State<AppState>,
    Path((namespace, pvc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let dest_name = format!("emergency-restore-{pvc}");
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();

    // Capture PVC spec before deletion.
    let pvc_out = Command::new("kubectl")
        .args(["get", "pvc", &pvc, "-n", &namespace, "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let pvc_v: serde_json::Value = serde_json::from_slice(&pvc_out.stdout).unwrap_or_default();
    let capacity = pvc_v["spec"]["resources"]["requests"]["storage"]
        .as_str()
        .unwrap_or("10Gi")
        .to_string();
    let storage_class = pvc_v["spec"]["storageClassName"]
        .as_str()
        .unwrap_or("yolab-cephfs")
        .to_string();
    let access_mode = pvc_v["spec"]["accessModes"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|m| m.as_str())
        .unwrap_or("ReadWriteMany")
        .to_string();

    // Scale down deployments using this PVC before deleting it.
    let deployments = find_deployments_for_pvc(&namespace, &pvc).await?;
    for deploy in &deployments {
        scale_deployment(&namespace, deploy, 0).await?;
    }

    // Delete old PVC — non-blocking; Ceph reclaims space once pods finish terminating.
    let del_out = Command::new("kubectl")
        .args(["delete", "pvc", &pvc, "-n", &namespace, "--wait=false"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if !del_out.status.success() {
        anyhow::bail!(
            "delete pvc failed: {}",
            String::from_utf8_lossy(&del_out.stderr).trim()
        );
    }

    // Create ReplicationDestination — VolSync retries until Ceph has space.
    let manifest = serde_json::json!({
        "apiVersion": "volsync.backube/v1alpha1",
        "kind": "ReplicationDestination",
        "metadata": {
            "name": dest_name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "spec": {
            "trigger": { "manual": format!("emergency-{timestamp}") },
            "rclone": {
                "rcloneConfigSection": "b2-crypt",
                "rcloneDestPath": format!("b2-crypt:{}/{}", namespace, pvc),
                "rcloneConfig": RCLONE_SECRET,
                "copyMethod": "Snapshot",
                "storageClassName": storage_class,
                "capacity": capacity,
                "accessModes": [access_mode],
                "moverSecurityContext": {
                    "runAsUser": 1000,
                    "runAsGroup": 1000,
                    "fsGroup": 1000
                }
            }
        }
    });
    kubectl_apply(&manifest.to_string()).await?;

    Ok(Json(serde_json::json!({
        "started": true,
        "destination_name": dest_name,
        "deployments_scaled_down": deployments,
        "note": "Old PVC deleted. Poll /emergency/status. Call /emergency/apply when Successful.",
    })))
}

/// GET /api/backups/restore/:namespace/:pvc/emergency/status
pub async fn emergency_restore_status(
    State(_state): State<AppState>,
    Path((namespace, pvc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let dest_name = format!("emergency-restore-{pvc}");
    let out = Command::new("kubectl")
        .args([
            "get",
            "replicationdestination",
            &dest_name,
            "-n",
            &namespace,
            "-o",
            "json",
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    if !out.status.success() {
        return Ok(Json(serde_json::json!({ "found": false })));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let result = v["status"]["latestMoverStatus"]["result"]
        .as_str()
        .unwrap_or("Running")
        .to_string();
    let last_sync_time = v["status"]["lastSyncTime"].as_str().map(String::from);

    Ok(Json(serde_json::json!({
        "found": true,
        "result": result,
        "last_sync_time": last_sync_time,
        "restored_pvc": format!("volsync-{dest_name}-dest"),
    })))
}

/// POST /api/backups/restore/:namespace/:pvc/emergency/apply
/// Patches deployments to use the restored PVC and scales them back up.
pub async fn apply_emergency_restore(
    State(_state): State<AppState>,
    Path((namespace, pvc)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let dest_name = format!("emergency-restore-{pvc}");
    let restored_pvc = format!("volsync-{dest_name}-dest");

    // Verify restore completed.
    let rd_out = Command::new("kubectl")
        .args([
            "get",
            "replicationdestination",
            &dest_name,
            "-n",
            &namespace,
            "-o",
            "json",
        ])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if !rd_out.status.success() {
        return Err(
            anyhow::anyhow!("Emergency restore not found — run /emergency first").into(),
        );
    }
    let rd_v: serde_json::Value = serde_json::from_slice(&rd_out.stdout).unwrap_or_default();
    let result = rd_v["status"]["latestMoverStatus"]["result"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if result.to_lowercase() != "successful" {
        return Err(anyhow::anyhow!("Restore not complete yet (status: {result})").into());
    }

    // Verify restored PVC exists.
    let pvc_check = Command::new("kubectl")
        .args(["get", "pvc", &restored_pvc, "-n", &namespace])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if !pvc_check.status.success() {
        return Err(anyhow::anyhow!("Restored PVC {restored_pvc} not found yet").into());
    }

    // Find deployments still referencing the original (now deleted) PVC name.
    let deployments = find_deployments_for_pvc(&namespace, &pvc).await?;

    for deploy in &deployments {
        let dep_out = Command::new("kubectl")
            .args(["get", "deployment", deploy, "-n", &namespace, "-o", "json"])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut dep_v: serde_json::Value =
            serde_json::from_slice(&dep_out.stdout).unwrap_or_default();

        if let Some(volumes) = dep_v["spec"]["template"]["spec"]["volumes"].as_array_mut() {
            for vol in volumes.iter_mut() {
                if vol["persistentVolumeClaim"]["claimName"].as_str() == Some(&pvc) {
                    vol["persistentVolumeClaim"]["claimName"] =
                        serde_json::Value::String(restored_pvc.clone());
                }
            }
        }

        let patch = serde_json::json!({
            "spec": {
                "template": {
                    "spec": {
                        "volumes": dep_v["spec"]["template"]["spec"]["volumes"]
                    }
                }
            }
        });
        let patch_out = Command::new("kubectl")
            .args([
                "patch",
                "deployment",
                deploy,
                "-n",
                &namespace,
                "--type=merge",
                "-p",
                &patch.to_string(),
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        if !patch_out.status.success() {
            return Err(anyhow::anyhow!(
                "patch deployment {} failed: {}",
                deploy,
                String::from_utf8_lossy(&patch_out.stderr).trim()
            )
            .into());
        }

        scale_deployment(&namespace, deploy, 1).await?;
    }

    // Point the ReplicationSource at the new PVC so future backups stay intact.
    let rs_name = format!("volsync-{pvc}");
    let rs_patch = serde_json::json!({ "spec": { "sourcePVC": restored_pvc } });
    let _ = Command::new("kubectl")
        .args([
            "patch",
            "replicationsource",
            &rs_name,
            "-n",
            &namespace,
            "--type=merge",
            "-p",
            &rs_patch.to_string(),
        ])
        .output()
        .await;

    // Clean up ReplicationDestination.
    let _ = Command::new("kubectl")
        .args(["delete", "replicationdestination", &dest_name, "-n", &namespace])
        .output()
        .await;

    Ok(Json(serde_json::json!({
        "applied": true,
        "restored_pvc": restored_pvc,
        "deployments_updated": deployments,
    })))
}

// ── Background etcd snapshot task ─────────────────────────────────────────────

/// Runs daily at 02:00 UTC: reads B2 creds from the master Secret and calls
/// `k3s etcd-snapshot save --etcd-s3 ...` to push a snapshot to B2.
pub async fn run_etcd_snapshots(_config: Arc<Config>) {
    loop {
        // Sleep until next 02:00 UTC.
        let now = chrono::Utc::now();
        let next = {
            let today_2am = now
                .date_naive()
                .and_hms_opt(2, 0, 0)
                .unwrap()
                .and_utc();
            if now < today_2am {
                today_2am
            } else {
                (now.date_naive() + chrono::Duration::days(1))
                    .and_hms_opt(2, 0, 0)
                    .unwrap()
                    .and_utc()
            }
        };
        let secs = (next - now).num_seconds().max(0) as u64;
        tokio::time::sleep(Duration::from_secs(secs)).await;

        if let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await {
            let key_id = data.get("access_key_id").cloned().unwrap_or_default();
            let secret = data.get("secret_access_key").cloned().unwrap_or_default();
            let bucket = data.get("bucket").cloned().unwrap_or_default();
            let endpoint = data.get("endpoint").cloned().unwrap_or_default();

            if key_id.is_empty() || bucket.is_empty() {
                tracing::warn!("etcd-snapshot: master Secret missing B2 creds, skipping");
                continue;
            }

            let s3_host = endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            let name = format!("etcd-daily-{}", chrono::Utc::now().format("%Y-%m-%d"));

            let out = Command::new("k3s")
                .args([
                    "etcd-snapshot",
                    "save",
                    "--etcd-s3",
                    &format!("--etcd-s3-endpoint={s3_host}"),
                    &format!("--etcd-s3-bucket={bucket}"),
                    &format!("--etcd-s3-access-key={key_id}"),
                    &format!("--etcd-s3-secret-key={secret}"),
                    &format!("--name={name}"),
                ])
                .output()
                .await;

            match out {
                Ok(o) if o.status.success() => {
                    tracing::info!("etcd-snapshot: saved {name} to B2 bucket {bucket}");
                }
                Ok(o) => {
                    tracing::warn!(
                        "etcd-snapshot: failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    );
                }
                Err(e) => {
                    tracing::warn!("etcd-snapshot: could not run k3s: {e}");
                }
            }
        } else {
            tracing::debug!("etcd-snapshot: backup not configured, skipping");
        }
    }
}
