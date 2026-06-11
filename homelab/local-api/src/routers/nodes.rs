use axum::{extract::State, Json};
use serde::Serialize;

use crate::{error::Result, kubectl, AppState};

#[derive(Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub ip: String,
    pub ready: bool,
    pub roles: Vec<String>,
    pub joined_at: String,
}

#[derive(Serialize)]
pub struct JoinInfo {
    pub k3s_token: String,
    pub server_addr: String,
}

pub async fn nodes() -> Json<Vec<NodeInfo>> {
    let items = kubectl::get_nodes().await.unwrap_or_default();
    Json(
        items.iter().map(|item| {
            let meta = &item["metadata"];
            let roles = meta["labels"].as_object().map(|l| {
                l.keys()
                    .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/").map(String::from))
                    .collect()
            }).unwrap_or_default();
            let ip = item["status"]["addresses"].as_array()
                .and_then(|a| a.iter().find(|a| a["type"] == "InternalIP"))
                .and_then(|a| a["address"].as_str().map(String::from))
                .unwrap_or_default();
            let ready = item["status"]["conditions"].as_array()
                .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
                .unwrap_or(false);
            NodeInfo {
                name: meta["name"].as_str().unwrap_or("").to_string(),
                ip,
                ready,
                roles,
                joined_at: meta["creationTimestamp"].as_str().unwrap_or("").to_string(),
            }
        }).collect(),
    )
}

pub async fn join_info(State(state): State<AppState>) -> Result<Json<JoinInfo>> {
    let text = std::fs::read_to_string(&state.config.config_path)?;
    let table: toml::Table = toml::from_str(&text)?;
    let k3s_token = table["node"]["k3s"]["token"].as_str()
        .ok_or_else(|| anyhow::anyhow!("missing node.k3s.token"))?.to_string();
    let sub_ipv6_private = table["tunnel"]["sub_ipv6_private"].as_str()
        .ok_or_else(|| anyhow::anyhow!("missing tunnel.sub_ipv6_private"))?;
    Ok(Json(JoinInfo {
        k3s_token,
        server_addr: format!("https://[{sub_ipv6_private}]:6443"),
    }))
}
