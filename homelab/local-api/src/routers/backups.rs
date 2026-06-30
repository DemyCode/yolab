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

fn random_hex(bytes: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
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
async fn ensure_restic_secret(ns: &str, pvc: &str, cfg: &BackupConfig) -> anyhow::Result<()> {
    let secret_name = format!("{pvc}{RESTIC_SECRET_SUFFIX}");
    let repo = cfg.restic_repo(&format!("volsync/{ns}/{pvc}"));
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
            Some(PvcInfo { namespace: ns, name })
        })
        .collect())
}

// ── VolSync ReplicationSource ─────────────────────────────────────────────────

async fn ensure_replication_source(pvc: &PvcInfo) -> anyhow::Result<()> {
    let rs_name = format!("volsync-{}", pvc.name);
    let secret_name = format!("{}{RESTIC_SECRET_SUFFIX}", pvc.name);
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
            "restic": {
                "repository": secret_name,
                "pruneIntervalDays": 7,
                "retain": { "daily": 7, "weekly": 4, "monthly": 12 },
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

    let mut sources: Vec<String> = Vec::new();

    for pvc in &pvcs {
        ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await?;
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

    // Build a (namespace, pvc_name) → phase map from all PVCs.
    let pvc_phase_map: HashMap<(String, String), String> = Command::new("kubectl")
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
            Some(((ns, name), phase))
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
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            let last_sync_duration =
                item["status"]["lastSyncDuration"].as_str().map(String::from);
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or(if last_sync_time.is_some() { "Successful" } else { "Pending" })
                .to_string();
            let pvc_phase = pvc_phase_map
                .get(&(namespace.clone(), pvc.clone()))
                .cloned()
                .unwrap_or_else(|| "NotFound".to_string());
            serde_json::json!({
                "namespace": namespace,
                "pvc": pvc,
                "last_sync_time": last_sync_time,
                "last_sync_duration": last_sync_duration,
                "result": result,
                "pvc_phase": pvc_phase,
            })
        })
        .collect();

    let dr_mode = pvcs
        .iter()
        .any(|p| matches!(p["pvc_phase"].as_str(), Some("Lost") | Some("NotFound")));

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

    let secret_name = format!("{pvc}{RESTIC_SECRET_SUFFIX}");
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
            "restic": {
                "repository": secret_name,
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
        "restored_pvc": format!("volsync-{dest_name}-dest"),
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

async fn patch_deployment_pvc(
    namespace: &str,
    deploy: &str,
    old_pvc: &str,
    new_pvc: &str,
) -> anyhow::Result<()> {
    let dep_out = Command::new("kubectl")
        .args(["get", "deployment", deploy, "-n", namespace, "-o", "json"])
        .output()
        .await?;
    let mut dep_v: serde_json::Value =
        serde_json::from_slice(&dep_out.stdout).unwrap_or_default();
    if let Some(volumes) = dep_v["spec"]["template"]["spec"]["volumes"].as_array_mut() {
        for vol in volumes.iter_mut() {
            if vol["persistentVolumeClaim"]["claimName"].as_str() == Some(old_pvc) {
                vol["persistentVolumeClaim"]["claimName"] =
                    serde_json::Value::String(new_pvc.to_string());
            }
        }
    }
    let patch = serde_json::json!({
        "spec": { "template": { "spec": {
            "volumes": dep_v["spec"]["template"]["spec"]["volumes"]
        }}}
    });
    let out = Command::new("kubectl")
        .args([
            "patch", "deployment", deploy, "-n", namespace,
            "--type=merge", "-p", &patch.to_string(),
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "patch deployment {} failed: {}",
            deploy,
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
        return Err(anyhow::anyhow!(
            "delete pvc failed: {}",
            String::from_utf8_lossy(&del_out.stderr).trim()
        )
        .into());
    }

    // Create ReplicationDestination — VolSync retries until Ceph has space.
    let secret_name = format!("{pvc}{RESTIC_SECRET_SUFFIX}");
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
            "restic": {
                "repository": secret_name,
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
        patch_deployment_pvc(&namespace, deploy, &pvc, &restored_pvc).await?;
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

// ── Disaster-recovery bulk restore ───────────────────────────────────────────

/// POST /api/backups/dr/start
/// For every ReplicationSource whose sourcePVC is Lost/NotFound, triggers an
/// emergency restore (scale-down → delete PVC → pull from B2).
pub async fn dr_start(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let rs_out = Command::new("kubectl")
        .args(["get", "replicationsource", "-A", "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let rs_v: serde_json::Value =
        serde_json::from_slice(&rs_out.stdout).unwrap_or(serde_json::json!({"items": []}));

    let pvc_phase_map: HashMap<(String, String), String> = Command::new("kubectl")
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
            Some(((ns, name), phase))
        })
        .collect();

    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();
    let mut started: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for item in rs_v["items"].as_array().cloned().unwrap_or_default() {
        let namespace = item["metadata"]["namespace"].as_str().unwrap_or("").to_string();
        let pvc_name = item["spec"]["sourcePVC"].as_str().unwrap_or("").to_string();

        if namespace.is_empty() || pvc_name.is_empty() || EXCLUDED_NS.contains(&namespace.as_str()) {
            continue;
        }

        let phase = pvc_phase_map
            .get(&(namespace.clone(), pvc_name.clone()))
            .cloned()
            .unwrap_or_else(|| "NotFound".to_string());

        if phase != "Lost" && phase != "NotFound" {
            skipped.push(format!("{namespace}/{pvc_name} (phase: {phase})"));
            continue;
        }

        // Skip if this restore is already in progress.
        let dest_name = format!("emergency-restore-{pvc_name}");
        let rd_exists = Command::new("kubectl")
            .args(["get", "replicationdestination", &dest_name, "-n", &namespace])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if rd_exists {
            skipped.push(format!("{namespace}/{pvc_name} (already restoring)"));
            continue;
        }

        // Read PVC spec for capacity / storage-class / access-mode.
        let (capacity, storage_class, access_mode) = {
            let pvc_out = Command::new("kubectl")
                .args(["get", "pvc", &pvc_name, "-n", &namespace, "-o", "json"])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            if pvc_out.status.success() {
                let pv: serde_json::Value =
                    serde_json::from_slice(&pvc_out.stdout).unwrap_or_default();
                (
                    pv["spec"]["resources"]["requests"]["storage"]
                        .as_str().unwrap_or("10Gi").to_string(),
                    pv["spec"]["storageClassName"]
                        .as_str().unwrap_or("yolab-cephfs").to_string(),
                    pv["spec"]["accessModes"].as_array()
                        .and_then(|a| a.first())
                        .and_then(|m| m.as_str())
                        .unwrap_or("ReadWriteMany")
                        .to_string(),
                )
            } else {
                ("10Gi".to_string(), "yolab-cephfs".to_string(), "ReadWriteMany".to_string())
            }
        };

        // Scale down any deployments using this PVC.
        let deploys = find_deployments_for_pvc(&namespace, &pvc_name)
            .await
            .unwrap_or_default();
        for d in &deploys {
            let _ = scale_deployment(&namespace, d, 0).await;
        }

        // Delete the Lost PVC (non-blocking).
        let _ = Command::new("kubectl")
            .args(["delete", "pvc", &pvc_name, "-n", &namespace, "--wait=false"])
            .output()
            .await;

        // Create ReplicationDestination to pull data from B2.
        let secret_name = format!("{pvc_name}{RESTIC_SECRET_SUFFIX}");
        let manifest = serde_json::json!({
            "apiVersion": "volsync.backube/v1alpha1",
            "kind": "ReplicationDestination",
            "metadata": {
                "name": dest_name,
                "namespace": namespace,
                "labels": { "app.kubernetes.io/managed-by": "yolab" }
            },
            "spec": {
                "trigger": { "manual": format!("dr-{timestamp}") },
                "restic": {
                    "repository": secret_name,
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

        match kubectl_apply(&manifest.to_string()).await {
            Ok(_) => started.push(format!("{namespace}/{pvc_name}")),
            Err(e) => tracing::warn!("DR start: failed RD for {namespace}/{pvc_name}: {e}"),
        }
    }

    Ok(Json(serde_json::json!({ "started": started, "skipped": skipped })))
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
                "pvc": pvc_name,
                "result": result,
                "last_sync_time": last_sync_time,
                "restored_pvc": format!("volsync-{name}-dest"),
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

/// POST /api/backups/dr/apply
/// Patches deployments to use restored PVCs and scales them back up for all
/// completed (Successful) emergency restores.
pub async fn dr_apply(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let out = Command::new("kubectl")
        .args(["get", "replicationdestination", "-A", "-o", "json"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!({"items": []}));

    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

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

        if item["status"]["latestMoverStatus"]["result"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            != "successful"
        {
            continue;
        }

        let restored_pvc = format!("volsync-{name}-dest");

        // Verify restored PVC exists.
        let pvc_ok = Command::new("kubectl")
            .args(["get", "pvc", &restored_pvc, "-n", &namespace])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !pvc_ok {
            errors.push(format!("{namespace}/{pvc_name}: restored PVC not found yet"));
            continue;
        }

        let deploys = find_deployments_for_pvc(&namespace, &pvc_name)
            .await
            .unwrap_or_default();
        let mut had_error = false;
        for deploy in &deploys {
            match patch_deployment_pvc(&namespace, deploy, &pvc_name, &restored_pvc).await {
                Ok(_) => {
                    let _ = scale_deployment(&namespace, deploy, 1).await;
                }
                Err(e) => {
                    errors.push(format!("{namespace}/{deploy}: {e}"));
                    had_error = true;
                }
            }
        }

        if !had_error {
            // Update ReplicationSource so future backups target the new PVC.
            let rs_patch = serde_json::json!({ "spec": { "sourcePVC": restored_pvc } });
            let _ = Command::new("kubectl")
                .args([
                    "patch", "replicationsource", &format!("volsync-{pvc_name}"),
                    "-n", &namespace, "--type=merge", "-p", &rs_patch.to_string(),
                ])
                .output()
                .await;

            // Clean up ReplicationDestination.
            let _ = Command::new("kubectl")
                .args(["delete", "replicationdestination", &name, "-n", &namespace])
                .output()
                .await;

            applied.push(format!("{namespace}/{pvc_name}"));
        }
    }

    Ok(Json(serde_json::json!({ "applied": applied, "errors": errors })))
}

// ── Background cluster backup task ───────────────────────────────────────────

/// Runs daily at 02:00 UTC. Bundles:
///   1. etcd snapshot (saved locally via k3s, then encrypted by restic)
///   2. kubectl exports of all yolab-* namespace objects
///   3. catalog.json (namespace + PVC list for the restore UI)
/// Everything is pushed to B2 via restic — fully client-side encrypted.
pub async fn run_cluster_backup(_config: Arc<Config>) {
    loop {
        // Sleep until next 02:00 UTC.
        let now = chrono::Utc::now();
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

        let Some(data) = kubectl_get_secret(MASTER_SECRET, MASTER_NS).await else {
            tracing::debug!("cluster-backup: not configured, skipping");
            continue;
        };

        let key_id          = data.get("access_key_id").cloned().unwrap_or_default();
        let secret_key      = data.get("secret_access_key").cloned().unwrap_or_default();
        let bucket          = data.get("bucket").cloned().unwrap_or_default();
        let endpoint        = data.get("endpoint").cloned().unwrap_or_default();
        let restic_password = data.get("restic_password").cloned().unwrap_or_default();

        if key_id.is_empty() || restic_password.is_empty() {
            tracing::warn!("cluster-backup: missing creds or restic_password, skipping");
            continue;
        }

        let date    = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let tmp_dir = format!("/tmp/yolab-cluster-backup-{date}");
        let repo    = format!("s3:{}/{}/cluster-backup", endpoint.trim_end_matches('/'), bucket);

        if let Err(e) = tokio::fs::create_dir_all(&tmp_dir).await {
            tracing::warn!("cluster-backup: mkdir failed: {e}");
            continue;
        }

        // 1. Save etcd snapshot to k3s default dir, then copy into our tmp bundle.
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
                                tracing::warn!("cluster-backup: copy etcd snapshot failed: {e}");
                            }
                            // Clean up k3s-managed snapshot to avoid accumulation.
                            let _ = Command::new("kubectl")
                                .args(["delete", "etcdsnapshotfile", &fname_str.to_string(), "--ignore-not-found"])
                                .output().await;
                            break;
                        }
                    }
                }
            }
            Ok(o) => tracing::warn!("cluster-backup: etcd-snapshot failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => tracing::warn!("cluster-backup: could not run k3s: {e}"),
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
                    .args(["get", "deploy,svc,pvc,secret,configmap,replicationsource",
                           "-n", ns, "-o", "yaml", "--ignore-not-found"])
                    .output().await;
                if let Ok(obj) = obj_out {
                    let path = format!("{tmp_dir}/{ns}.yaml");
                    let _ = tokio::fs::write(&path, &obj.stdout).await;
                }
            }
        }

        // 3. Write catalog.json.
        let catalog = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "namespaces": namespaces,
        });
        let _ = tokio::fs::write(
            format!("{tmp_dir}/catalog.json"),
            catalog.to_string(),
        ).await;

        // 4. Init restic repo if it doesn't exist yet.
        let snapshots_check = Command::new("restic")
            .args(["snapshots"])
            .env("RESTIC_REPOSITORY", &repo)
            .env("RESTIC_PASSWORD", &restic_password)
            .env("AWS_ACCESS_KEY_ID", &key_id)
            .env("AWS_SECRET_ACCESS_KEY", &secret_key)
            .output().await;
        if snapshots_check.map(|o| !o.status.success()).unwrap_or(true) {
            let _ = Command::new("restic")
                .args(["init"])
                .env("RESTIC_REPOSITORY", &repo)
                .env("RESTIC_PASSWORD", &restic_password)
                .env("AWS_ACCESS_KEY_ID", &key_id)
                .env("AWS_SECRET_ACCESS_KEY", &secret_key)
                .output().await;
        }

        // 5. Backup tmp dir with restic.
        let backup_out = Command::new("restic")
            .args(["backup", &tmp_dir, "--tag", "cluster-backup"])
            .env("RESTIC_REPOSITORY", &repo)
            .env("RESTIC_PASSWORD", &restic_password)
            .env("AWS_ACCESS_KEY_ID", &key_id)
            .env("AWS_SECRET_ACCESS_KEY", &secret_key)
            .output().await;

        match backup_out {
            Ok(o) if o.status.success() => {
                tracing::info!("cluster-backup: restic snapshot complete for {date}");
                // Prune: keep 7 daily, 4 weekly, 12 monthly.
                let _ = Command::new("restic")
                    .args(["forget", "--tag", "cluster-backup",
                           "--keep-daily", "7", "--keep-weekly", "4",
                           "--keep-monthly", "12", "--prune"])
                    .env("RESTIC_REPOSITORY", &repo)
                    .env("RESTIC_PASSWORD", &restic_password)
                    .env("AWS_ACCESS_KEY_ID", &key_id)
                    .env("AWS_SECRET_ACCESS_KEY", &secret_key)
                    .output().await;
            }
            Ok(o) => tracing::warn!("cluster-backup: restic failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => tracing::warn!("cluster-backup: could not run restic: {e}"),
        }

        // 6. Clean up temp dir.
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    }
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

/// POST /api/backups/restore/from-snapshot
/// Body: { snapshot_id: string, namespaces: string[] }
/// For each selected namespace:
///   - If namespace doesn't exist: extract its YAML from the snapshot and apply it.
///   - Ensure the restic secret exists for every PVC (needed for new namespaces).
///   - Scale down deployments, delete PVCs, create ReplicationDestinations.
/// Progress is tracked by the existing /api/backups/dr/status + /dr/apply endpoints.
#[derive(Deserialize)]
pub struct RestoreFromSnapshotBody {
    pub snapshot_id: String,
    pub namespaces:  Vec<String>,
}

pub async fn restore_from_snapshot(
    State(_state): State<AppState>,
    Json(body): Json<RestoreFromSnapshotBody>,
) -> Result<Json<serde_json::Value>> {
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
    let repo      = cfg.restic_repo("cluster-backup");
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();

    let mut started: Vec<String> = Vec::new();
    let mut errors:  Vec<String> = Vec::new();

    for ns in &body.namespaces {
        let ns_exists = Command::new("kubectl")
            .args(["get", "namespace", ns])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !ns_exists {
            // Extract the namespace YAML from the snapshot and apply it.
            let target = format!("/tmp/yolab-restore-yaml-{}", random_hex(8));
            let pattern = format!("**/{ns}.yaml");

            let restore_out = Command::new("restic")
                .args(["restore", &body.snapshot_id, "--target", &target, "--include", &pattern])
                .env("RESTIC_REPOSITORY", &repo)
                .env("RESTIC_PASSWORD", &cfg.restic_password)
                .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
                .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                .output()
                .await;

            match restore_out {
                Ok(o) if o.status.success() => {
                    let find = Command::new("find")
                        .args([&target, "-name", &format!("{ns}.yaml"), "-type", "f"])
                        .output()
                        .await;

                    if let Ok(f) = find {
                        let yaml_path = String::from_utf8_lossy(&f.stdout).trim().to_string();
                        if !yaml_path.is_empty() {
                            if let Ok(yaml_bytes) = tokio::fs::read(&yaml_path).await {
                                let _ = kubectl_apply(&String::from_utf8_lossy(&yaml_bytes)).await;
                                // Give K8s a moment to process the apply before we query PVCs.
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                        }
                    }
                    let _ = tokio::fs::remove_dir_all(&target).await;
                }
                _ => {
                    let _ = tokio::fs::remove_dir_all(&target).await;
                    errors.push(format!("{ns}: failed to extract snapshot YAML"));
                    continue;
                }
            }
        }

        let pvcs: Vec<PvcInfo> = list_user_pvcs()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.namespace == *ns)
            .collect();

        if pvcs.is_empty() {
            errors.push(format!("{ns}: no PVCs found"));
            continue;
        }

        for pvc in &pvcs {
            // Recreate restic secret (always needed for new namespaces, harmless for existing).
            if let Err(e) = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await {
                errors.push(format!("{}/{}: secret: {e}", pvc.namespace, pvc.name));
                continue;
            }

            // Ensure a ReplicationSource exists for future backups.
            let _ = ensure_replication_source(pvc).await;

            let dest_name = format!("emergency-restore-{}", pvc.name);
            let rd_exists = Command::new("kubectl")
                .args(["get", "replicationdestination", &dest_name, "-n", &pvc.namespace])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            if rd_exists {
                started.push(format!("{}/{} (already in progress)", pvc.namespace, pvc.name));
                continue;
            }

            let pvc_out = Command::new("kubectl")
                .args(["get", "pvc", &pvc.name, "-n", &pvc.namespace, "-o", "json"])
                .output()
                .await;

            let (capacity, storage_class, access_mode) = match pvc_out {
                Ok(o) if o.status.success() => {
                    let pv: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
                    (
                        pv["spec"]["resources"]["requests"]["storage"].as_str().unwrap_or("10Gi").to_string(),
                        pv["spec"]["storageClassName"].as_str().unwrap_or("yolab-cephfs").to_string(),
                        pv["spec"]["accessModes"].as_array()
                            .and_then(|a| a.first())
                            .and_then(|m| m.as_str())
                            .unwrap_or("ReadWriteMany")
                            .to_string(),
                    )
                }
                _ => ("10Gi".to_string(), "yolab-cephfs".to_string(), "ReadWriteMany".to_string()),
            };

            let deploys = find_deployments_for_pvc(&pvc.namespace, &pvc.name).await.unwrap_or_default();
            for d in &deploys {
                let _ = scale_deployment(&pvc.namespace, d, 0).await;
            }

            let _ = Command::new("kubectl")
                .args(["delete", "pvc", &pvc.name, "-n", &pvc.namespace, "--wait=false"])
                .output()
                .await;

            let secret_name = format!("{}{RESTIC_SECRET_SUFFIX}", pvc.name);
            let manifest = serde_json::json!({
                "apiVersion": "volsync.backube/v1alpha1",
                "kind": "ReplicationDestination",
                "metadata": {
                    "name": dest_name,
                    "namespace": pvc.namespace,
                    "labels": { "app.kubernetes.io/managed-by": "yolab" }
                },
                "spec": {
                    "trigger": { "manual": format!("snapshot-restore-{timestamp}") },
                    "restic": {
                        "repository": secret_name,
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

            match kubectl_apply(&manifest.to_string()).await {
                Ok(_) => started.push(format!("{}/{}", pvc.namespace, pvc.name)),
                Err(e) => errors.push(format!("{}/{}: {e}", pvc.namespace, pvc.name)),
            }
        }
    }

    Ok(Json(serde_json::json!({ "started": started, "errors": errors })))
}
