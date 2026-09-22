use std::time::Duration;

use axum::{extract::State, Json};

use crate::{auth::CLUSTER_AUTH_HEADER, kubectl, AppState};

const REBOOT_DELAY_SECS: u64 = 3;

fn spawn_reboot() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(REBOOT_DELAY_SECS)).await;
        tracing::warn!("rebooting this machine now");
        let _ = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .stdin(std::process::Stdio::null())
            .status()
            .await;
    });
}

pub async fn reboot() -> Json<serde_json::Value> {
    spawn_reboot();
    Json(serde_json::json!({ "status": "rebooting" }))
}

pub async fn reboot_all(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();
    let token = cfg.cluster_token();

    let nodes = kubectl::get_nodes().await.unwrap_or_default();
    let peers = kubectl::peer_ipv6(&nodes, &self_ip);

    let client = reqwest::Client::new();
    let mut asked = Vec::new();
    let mut unreachable = Vec::new();
    for addr in &peers {
        let url = format!("http://[{addr}]:{}/api/system/reboot", cfg.port);
        match client
            .post(&url)
            .header(CLUSTER_AUTH_HEADER, &token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => asked.push(addr.clone()),
            _ => unreachable.push(addr.clone()),
        }
    }

    if !unreachable.is_empty() {
        tracing::warn!("reboot: could not reach {unreachable:?} — rebooting the rest anyway");
    }

    spawn_reboot();

    Json(serde_json::json!({
        "status": "rebooting",
        "asked": asked,
        "unreachable": unreachable,
    }))
}
