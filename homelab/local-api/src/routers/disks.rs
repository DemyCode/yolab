use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{kubectl, AppState};

const STATUS_CM: &str = "yolab-disk-status";
const CONFIG_CM: &str = "yolab-disk-config";
const NS: &str = "rook-ceph";

#[derive(Serialize)]
pub struct DiskInfo {
    pub id: String,
    pub device: String,
    pub model: String,
    pub size_bytes: u64,
    pub is_loop: bool,
    pub is_our_osd: bool,
    pub osd_id: Option<i64>,
    pub desired: String,
}

#[derive(Deserialize)]
pub struct SetState {
    pub desired: String,
}

pub async fn list_disks(State(_s): State<AppState>) -> Json<HashMap<String, Vec<DiskInfo>>> {
    let status_raw = kubectl::get_json(&[
        "get", "configmap", STATUS_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or(serde_json::Value::Object(Default::default()));

    let config_raw = kubectl::get_json(&[
        "get", "configmap", CONFIG_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or(serde_json::Value::Object(Default::default()));

    let desired: HashMap<String, String> =
        serde_json::from_value(config_raw).unwrap_or_default();

    let mut result: HashMap<String, Vec<DiskInfo>> = HashMap::new();

    if let Some(status_map) = status_raw.as_object() {
        for (node, node_json) in status_map {
            let disks_raw: HashMap<String, serde_json::Value> =
                serde_json::from_str(node_json.as_str().unwrap_or("{}")).unwrap_or_default();

            let mut disks: Vec<DiskInfo> = disks_raw
                .into_iter()
                .map(|(disk_id, v)| {
                    let cm_key = format!("{}--{}", node, disk_id);
                    DiskInfo {
                        desired: desired
                            .get(&cm_key)
                            .cloned()
                            .unwrap_or_else(|| "USING".into()),
                        id: disk_id,
                        device: v["device"].as_str().unwrap_or("").to_string(),
                        model: v["model"].as_str().unwrap_or("").to_string(),
                        size_bytes: v["size_bytes"].as_u64().unwrap_or(0),
                        is_loop: v["is_loop"].as_bool().unwrap_or(false),
                        is_our_osd: v["is_our_osd"].as_bool().unwrap_or(false),
                        osd_id: v["osd_id"].as_i64(),
                    }
                })
                .collect();

            // System disk (loop) first, then largest first
            disks.sort_by(|a, b| {
                b.is_loop
                    .cmp(&a.is_loop)
                    .then(b.size_bytes.cmp(&a.size_bytes))
            });

            result.insert(node.clone(), disks);
        }
    }

    Json(result)
}

pub async fn set_disk_state(
    Path((node, id)): Path<(String, String)>,
    State(_s): State<AppState>,
    Json(body): Json<SetState>,
) -> Json<serde_json::Value> {
    if body.desired != "USING" && body.desired != "OFF" {
        return Json(serde_json::json!({"ok": false, "error": "desired must be USING or OFF"}));
    }

    let cm_key = format!("{}--{}", node, id);
    let patch = serde_json::json!({"data": {cm_key.as_str(): body.desired.as_str()}}).to_string();

    if kubectl::run(&[
        "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
    ])
    .await
    .is_err()
    {
        // ConfigMap doesn't exist yet — create it, then patch
        let _ = kubectl::run(&["create", "configmap", CONFIG_CM, "-n", NS]).await;
        if kubectl::run(&[
            "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
        ])
        .await
        .is_err()
        {
            return Json(serde_json::json!({"ok": false, "error": "failed to save disk config"}));
        }
    }

    Json(serde_json::json!({"ok": true}))
}
