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
    pub foreign_ceph: bool,
    pub osd_id: Option<i64>,
    pub desired: String,
    pub connected: bool,
}

#[derive(Deserialize)]
pub struct SetState {
    pub desired: String,
}

pub async fn list_disks(State(_s): State<AppState>) -> Json<HashMap<String, Vec<DiskInfo>>> {
    let config_raw = kubectl::get_json(&[
        "get", "configmap", CONFIG_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or(serde_json::Value::Object(Default::default()));

    let status_raw = kubectl::get_json(&[
        "get", "configmap", STATUS_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or(serde_json::Value::Object(Default::default()));

    let desired: HashMap<String, String> =
        serde_json::from_value(config_raw).unwrap_or_default();

    // Build a lookup: node → disk_id → live metadata (only if currently connected)
    let mut live: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();
    if let Some(status_map) = status_raw.as_object() {
        for (node, node_json) in status_map {
            let payload: serde_json::Value =
                serde_json::from_str(node_json.as_str().unwrap_or("{}")).unwrap_or_default();
            if let Some(disks) = payload["disks"].as_object() {
                live.entry(node.clone()).or_default()
                    .extend(disks.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
    }

    // Config CM is the authoritative list of all known disks.
    // Status CM enriches connected disks with live metadata.
    let mut result: HashMap<String, Vec<DiskInfo>> = HashMap::new();
    for (cm_key, desired_val) in &desired {
        let Some((node, disk_id)) = cm_key.split_once("--") else { continue };
        let meta = live.get(node).and_then(|m| m.get(disk_id));
        let connected = meta.is_some();
        let info = DiskInfo {
            id: disk_id.to_string(),
            desired: desired_val.clone(),
            connected,
            device: meta.and_then(|v| v["device"].as_str()).unwrap_or("").to_string(),
            model: meta.and_then(|v| v["model"].as_str()).unwrap_or("").to_string(),
            size_bytes: meta.and_then(|v| v["size_bytes"].as_u64()).unwrap_or(0),
            is_loop: meta.and_then(|v| v["is_loop"].as_bool()).unwrap_or(false),
            is_our_osd: meta.and_then(|v| v["is_our_osd"].as_bool()).unwrap_or(false),
            foreign_ceph: meta.and_then(|v| v["foreign_ceph"].as_bool()).unwrap_or(false),
            osd_id: meta.and_then(|v| v["osd_id"].as_i64()),
        };
        result.entry(node.to_string()).or_default().push(info);
    }

    // Sort each node's list: connected first, then system disk, then by size desc
    for disks in result.values_mut() {
        disks.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.is_loop.cmp(&a.is_loop))
                .then(b.size_bytes.cmp(&a.size_bytes))
        });
    }

    Json(result)
}

pub async fn set_disk_state(
    Path((node, id)): Path<(String, String)>,
    State(_s): State<AppState>,
    Json(body): Json<SetState>,
) -> Json<serde_json::Value> {
    if body.desired != "ON" && body.desired != "OFF" {
        return Json(serde_json::json!({"ok": false, "error": "desired must be ON or OFF"}));
    }

    let cm_key = format!("{}--{}", node, id);
    let patch = serde_json::json!({"data": {cm_key.as_str(): body.desired.as_str()}}).to_string();

    if kubectl::run(&[
        "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
    ])
    .await
    .is_err()
    {
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

/// Erase a foreign-Ceph disk so it can be provisioned as an OSD.
///
/// Zeros the first 100 MiB to destroy the BlueStore superblock and any backup
/// copies. Only operates on the local node; returns an error if the disk belongs
/// to a different node so the caller knows to hit that node's API instead.
pub async fn erase_disk(
    Path((node, id)): Path<(String, String)>,
    State(_s): State<AppState>,
) -> Json<serde_json::Value> {
    let this_node = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if node != this_node {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("disk is on node '{node}', not this node ('{this_node}')")
        }));
    }

    // Read the disk's published metadata from the status ConfigMap.
    let status_raw = kubectl::get_json(&[
        "get", "configmap", STATUS_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or_default();

    let node_payload: serde_json::Value = status_raw[&node]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let meta = &node_payload["disks"][&id];
    if meta.is_null() {
        return Json(serde_json::json!({"ok": false, "error": "disk not found in inventory"}));
    }

    // Safety gate: only erase disks explicitly flagged as foreign Ceph.
    if meta["foreign_ceph"].as_bool() != Some(true) {
        return Json(serde_json::json!({
            "ok": false,
            "error": "disk does not contain foreign Ceph data — refusing to erase"
        }));
    }

    let device = meta["device"].as_str().unwrap_or("");
    // Block any path traversal: device must be a bare kernel name (sda, nvme0n1, …).
    if device.is_empty() || device.contains('/') || device.contains('.') {
        return Json(serde_json::json!({"ok": false, "error": "invalid device name"}));
    }

    let dev_path = format!("/dev/{device}");
    tracing::warn!("erase_disk: zeroing foreign BlueStore disk {dev_path} on {node}");

    // Zero 100 MiB — enough to destroy the BlueStore superblock at offset 0
    // and any backup copies stored in the first few megabytes.
    let out = tokio::process::Command::new("dd")
        .args([
            "if=/dev/zero",
            &format!("of={dev_path}"),
            "bs=1M",
            "count=100",
            "oflag=direct",
        ])
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            tracing::info!("erase_disk: {dev_path} erased successfully");
            Json(serde_json::json!({"ok": true}))
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::error!("erase_disk: dd failed on {dev_path}: {err}");
            Json(serde_json::json!({"ok": false, "error": err}))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}
