use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, AppState};

// ── shared credential reader ──────────────────────────────────────────────────

/// Read [yolab_external] url + account_token from config.toml.
/// Returns None if not configured or the file can't be read.
pub fn ye_creds(cfg: &Config) -> Option<(String, String)> {
    let text = std::fs::read_to_string(&cfg.config_path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let ye = table.get("yolab_external")?.as_table()?;
    let url = ye.get("url")?.as_str()?.trim_end_matches('/').to_string();
    let token = ye.get("account_token")?.as_str()?.to_string();
    if url.is_empty() || token.is_empty() { return None; }
    Some((url, token))
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
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "yolab_external not configured" })));
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

/// POST /api/backups/s3/enable — provision S3 bucket (idempotent) then restart
/// yolab-velero-bootstrap so Velero picks up the new credentials immediately.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("yolab_external not configured in config.toml").into());
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

    // Restart the bootstrap service so Velero picks up creds without a reboot.
    tokio::task::spawn_blocking(|| {
        let _ = std::process::Command::new("systemctl")
            .args(["restart", "yolab-velero-bootstrap.service"])
            .output();
    })
    .await
    .ok();

    Ok(Json(serde_json::json!({ "provisioned": true, "s3": body })))
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
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "yolab_external not configured" })));
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
