use axum::Json;
use serde::Serialize;

use crate::kubectl;

#[derive(Serialize)]
pub struct CephStatus {
    pub available: bool,
    pub health: String,
    pub osd_count: u32,
    pub osd_up: u32,
    pub total_bytes: u64,
    pub used_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn ceph_status() -> Json<CephStatus> {
    match cluster_status_from_k8s().await {
        Ok((status, osd_total, osd_ready)) => {
            let cap = status.get("ceph")
                .and_then(|c| c.get("capacity"))
                .cloned()
                .unwrap_or_default();
            Json(CephStatus {
                available: status.get("phase").and_then(|p| p.as_str()) == Some("Ready"),
                health: status.get("ceph")
                    .and_then(|c| c.get("health"))
                    .and_then(|h| h.as_str())
                    .unwrap_or("HEALTH_UNKNOWN")
                    .to_string(),
                osd_count: osd_total,
                osd_up: osd_ready,
                total_bytes: cap.get("bytesTotal").and_then(|v| v.as_u64()).unwrap_or(0),
                used_bytes: cap.get("bytesUsed").and_then(|v| v.as_u64()).unwrap_or(0),
                error: None,
            })
        }
        Err(e) => Json(CephStatus {
            available: false,
            health: "HEALTH_UNKNOWN".into(),
            osd_count: 0,
            osd_up: 0,
            total_bytes: 0,
            used_bytes: 0,
            error: Some(e.to_string()),
        }),
    }
}

pub async fn cluster_status_from_k8s()
-> anyhow::Result<(serde_json::Map<String, serde_json::Value>, u32, u32)> {
    let status_str = kubectl::run(&[
        "get", "cephcluster", "-n", "rook-ceph", "rook-ceph", "-o", "jsonpath={.status}",
    ]).await?;
    let status: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&status_str).unwrap_or_default();

    let ready_str = kubectl::run(&[
        "get", "deploy", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "-o", "jsonpath={.items[*].status.readyReplicas}",
    ]).await.unwrap_or_default();

    let osd_ready: u32 = ready_str.split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .sum();

    let items_str = kubectl::run(&[
        "get", "deploy", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "-o", "jsonpath={.items}",
    ]).await.unwrap_or_default();

    let osd_total = serde_json::from_str::<serde_json::Value>(&items_str)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len() as u32))
        .unwrap_or(0);

    Ok((status, osd_total, osd_ready))
}
