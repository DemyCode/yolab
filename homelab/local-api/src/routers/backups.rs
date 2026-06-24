use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, AppState};

// ── shared credential reader ──────────────────────────────────────────────────

/// Read platform API url + account_token from config.toml [tunnel] section.
/// Returns None if not configured or the file can't be read.
pub fn ye_creds(cfg: &Config) -> Option<(String, String)> {
    let text = std::fs::read_to_string(&cfg.config_path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;

    if let Some(tunnel) = table.get("tunnel").and_then(|v| v.as_table()) {
        let url = tunnel.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
        let token = tunnel.get("account_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !url.is_empty() && !token.is_empty() {
            return Some((url, token));
        }
    }

    None
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── S3 backup storage ─────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct S3StorageInfo {
    pub bucket_name: String,
    pub bucket_id: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub created_at: String,
}

/// GET /api/backups/s3 — return provisioned S3 info or 404.
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

/// POST /api/backups/s3/enable — provision S3 bucket (idempotent) then apply
/// the Velero credentials secret, BackupStorageLocation, and Schedule via kubectl.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };
    let body: serde_json::Value = http_client()
        .post(format!("{url}/storage/s3"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let bucket   = body["bucket_name"].as_str().unwrap_or("").to_string();
    let endpoint = body["endpoint"].as_str().unwrap_or("").to_string();
    let region   = body["region"].as_str().unwrap_or("").to_string();
    let key_id   = body["access_key_id"].as_str().unwrap_or("").to_string();
    let secret   = body["secret_access_key"].as_str().unwrap_or("").to_string();

    tokio::task::spawn_blocking(move || {
        apply_velero_config(&bucket, &endpoint, &region, &key_id, &secret)
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))??;

    Ok(Json(serde_json::json!({ "provisioned": true, "s3": body })))
}

fn apply_velero_config(
    bucket: &str,
    endpoint: &str,
    region: &str,
    key_id: &str,
    secret: &str,
) -> anyhow::Result<()> {
    use std::io::Write;

    // 1. cloud-credentials secret (AWS ini format Velero expects).
    let creds_ini = format!(
        "[default]\naws_access_key_id = {key_id}\naws_secret_access_key = {secret}\n"
    );
    let secret_manifest = format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: cloud-credentials\n  \
         namespace: velero\nstringData:\n  cloud: |\n    {}\n",
        creds_ini.replace('\n', "\n    ")
    );
    kubectl_apply(&secret_manifest)?;

    // 2. BackupStorageLocation.
    let bsl_manifest = format!(
        "apiVersion: velero.io/v1\nkind: BackupStorageLocation\n\
         metadata:\n  name: default\n  namespace: velero\n\
         spec:\n  provider: aws\n  objectStorage:\n    bucket: {bucket}\n  \
         config:\n    region: {region}\n    s3ForcePathStyle: \"true\"\n    \
         s3Url: {endpoint}\n"
    );
    kubectl_apply(&bsl_manifest)?;

    // 3. Daily full-cluster backup schedule.
    let schedule_manifest = "apiVersion: velero.io/v1\nkind: Schedule\n\
         metadata:\n  name: daily-full\n  namespace: velero\n\
         spec:\n  schedule: \"0 3 * * *\"\n  template:\n    ttl: 720h0m0s\n    \
         storageLocation: default\n    defaultVolumesToFsBackup: true\n";
    kubectl_apply(schedule_manifest)?;

    Ok(())
}

fn kubectl_apply(manifest: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(manifest.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "kubectl apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ── SFTP virtual drive ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct SftpStorageInfo {
    pub host: String,
    pub port: i32,
    pub username: String,
    pub password: String,
    pub created_at: String,
}

/// GET /api/backups/sftp — return provisioned SFTP info or not-provisioned.
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
