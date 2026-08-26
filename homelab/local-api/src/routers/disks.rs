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
    /// The disk has a partition table — something is already on it.
    pub has_partitions: bool,
    /// This machine has a filesystem from it mounted. Never usable for storage.
    pub mounted: bool,
    /// Where the reconciler has got to with this disk: see disks_reconciler::phase.
    ///
    /// Without this the UI had to guess from desired/connected/is_our_osd, and
    /// that guess cannot distinguish "started five seconds ago" from "has failed
    /// fourteen times" — both render as "Setting up…". Every failure lived only
    /// in a log line.
    pub phase: String,
    /// Plain-language detail for `phase`, including the last error. Shown as-is.
    pub message: String,
    /// Failed attempts at the current transition. 0 once it succeeds.
    pub attempts: u32,
}

#[derive(Deserialize)]
pub struct SetState {
    pub desired: String,
}


/// The inverse of `disks_reconciler::record_key`.
///
/// Keys come in two shapes, and this is the half that was missing when they did:
/// `record_key` gained a bare form for hardware ids, nothing here learned to read it,
/// and `split_once("--")` skipped every such key with `else { continue }`. The record
/// driving a live disk drain was therefore absent from the page entirely — no row, no
/// toggle, no way to see or undo it. A writer changed and its reader did not.
///
/// Returns the node a record is scoped to, or None when the record belongs to the disk
/// itself and so to whichever machine currently holds it.
fn split_record_key(key: &str) -> (Option<&str>, &str) {
    // Checked before splitting: a hardware id is never node-scoped, and splitting one
    // on the first `--` it happens to contain would cut it in the wrong place.
    if crate::disks_reconciler::is_globally_unique_id(key) {
        return (None, key);
    }
    match key.split_once("--") {
        Some((node, id)) => (Some(node), id),
        // Neither shape. Not skipped: a key nobody can parse still governs a disk, and
        // dropping it is exactly how one became invisible.
        None => (None, key),
    }
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
    let status_raw2 = status_raw.clone();

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

    // Where each disk was last seen, so a bare-keyed record for a disk that is
    // currently unplugged still lands under a machine rather than nowhere.
    let mut last_seen: HashMap<String, String> = HashMap::new();
    if let Some(status_map) = status_raw2.as_object() {
        for (node, node_json) in status_map {
            let payload: serde_json::Value =
                serde_json::from_str(node_json.as_str().unwrap_or("{}")).unwrap_or_default();
            if let Some(disks) = payload["knownDisks"].as_object().or(payload["disks"].as_object()) {
                for id in disks.keys() {
                    last_seen.entry(id.clone()).or_insert_with(|| node.clone());
                }
            }
        }
    }

    // Config CM is the authoritative list of all known disks.
    // Status CM enriches connected disks with live metadata.
    let mut result: HashMap<String, Vec<DiskInfo>> = HashMap::new();
    for (cm_key, desired_val) in &desired {
        let (scoped_node, disk_id) = split_record_key(cm_key);
        // A record that belongs to the disk rather than to a machine is shown under
        // whichever machine can actually see the disk right now — which is the whole
        // point of it not being node-scoped. When nobody can see it, it is listed
        // against the node it was last known at, so an unplugged disk still appears
        // instead of vanishing from the page.
        let node: &str = match scoped_node {
            Some(n) => n,
            None => live
                .iter()
                .find(|(_, disks)| disks.contains_key(disk_id))
                .map(|(n, _)| n.as_str())
                .or_else(|| last_seen.get(disk_id).map(String::as_str))
                .unwrap_or(""),
        };
        let meta = live.get(node).and_then(|m| m.get(disk_id));
        let connected = meta.is_some();
        let info = DiskInfo {
            id: disk_id.to_string(),
            desired: desired_val.clone(),
            connected,
            device: meta.and_then(|v| v["device"].as_str()).unwrap_or("").to_string(),
            model: meta.and_then(|v| v["model"].as_str()).unwrap_or("").to_string(),
            size_bytes: meta.and_then(|v| v["size_bytes"].as_u64()).unwrap_or(0),
            has_partitions: meta.and_then(|v| v["has_partitions"].as_bool()).unwrap_or(false),
            mounted: meta.and_then(|v| v["mounted"].as_bool()).unwrap_or(false),
            phase: meta
                .and_then(|v| v["phase"].as_str())
                .unwrap_or("")
                .to_string(),
            message: meta
                .and_then(|v| v["message"].as_str())
                .unwrap_or("")
                .to_string(),
            attempts: meta
                .and_then(|v| v["attempts"].as_u64())
                .unwrap_or(0) as u32,
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

    // Built by the same function the reconciler reads with, so a toggle can never
    // write a key nothing acts on (or act on one nothing can write).
    let cm_key = crate::disks_reconciler::record_key(&node, &id);
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

    // Two things may be erased, and only these two:
    //
    //   * a disk carrying another Ceph cluster's data
    //   * a disk with a partition table on it
    //
    // The second is new, and it is what makes the Storage page's "Erase and use"
    // button work at all: partitioned disks are listed now (most external drives
    // ship formatted), the reconciler refuses to build an OSD on one, and this is
    // the only way out of that state. Without it the button was offered and
    // always failed.
    let foreign = meta["foreign_ceph"].as_bool() == Some(true);
    let partitioned = meta["has_partitions"].as_bool() == Some(true);
    if !foreign && !partitioned {
        return Json(serde_json::json!({
            "ok": false,
            "error": "this disk has nothing on it to erase"
        }));
    }

    // Never erase a disk this machine is using, whatever else is true of it.
    // The inventory already skips mounted disks, so reaching here means one was
    // mounted between the scan and this call — rare, and exactly why the check
    // is repeated at the point of destruction rather than trusted from upstream.
    if meta["mounted"].as_bool() == Some(true) {
        return Json(serde_json::json!({
            "ok": false,
            "error": "this machine is using that disk — refusing to erase it"
        }));
    }

    // An OSD of ours is torn down by switching the disk OFF, which drains it
    // first. Erasing is the path for disks that are NOT part of the cluster, and
    // routing a live OSD through it would skip the drain entirely.
    if meta["is_our_osd"].as_bool() == Some(true) {
        return Json(serde_json::json!({
            "ok": false,
            "error": "this disk is part of your storage — switch it off instead, so its files move somewhere else first"
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Reading the keys the reconciler writes ────────────────────────────────
    //
    // These two halves drifted apart once: record_key gained a bare form for hardware
    // ids, this file kept `split_once("--") else { continue }`, and every bare key was
    // skipped. The record draining a live disk had no row on the page and no toggle —
    // the system was acting on state its owner could not see.

    #[test]
    fn a_node_scoped_key_splits_into_node_and_disk() {
        assert_eq!(split_record_key("node1--dev-sda"), (Some("node1"), "dev-sda"));
        assert_eq!(split_record_key("node1--system"), (Some("node1"), "system"));
    }

    /// A hardware id belongs to the disk, so it has no node in it — and must not be
    /// split on a `--` it merely happens to contain.
    #[test]
    fn a_hardware_key_keeps_its_whole_id() {
        assert_eq!(
            split_record_key("serial-wwn-0x50014ee214caf529"),
            (None, "serial-wwn-0x50014ee214caf529")
        );
        assert_eq!(
            split_record_key("serial-ata-wdc--wd10"),
            (None, "serial-ata-wdc--wd10"),
            "a hardware id is never cut in half"
        );
    }

    /// The failure this replaces: an unparseable key used to be dropped with
    /// `continue`. A key nobody can read still governs a disk.
    #[test]
    fn an_unrecognised_key_is_surfaced_rather_than_dropped() {
        assert_eq!(split_record_key("weird"), (None, "weird"));
        assert_eq!(split_record_key(""), (None, ""));
    }

    /// Round trip against the writer, which is the property that actually matters:
    /// whatever record_key produces, split_record_key has to recover.
    #[test]
    fn every_key_the_reconciler_writes_can_be_read_back() {
        for (node, id) in [
            ("node1", "dev-sda"),
            ("node1", "system"),
            ("node3", "system"),
            ("node1", "serial-wwn-0x50014ee214caf529"),
            ("node3", "serial-ata-wdc-wd10sdrw"),
        ] {
            let key = crate::disks_reconciler::record_key(node, id);
            let (got_node, got_id) = split_record_key(&key);
            assert_eq!(got_id, id, "id must survive {key}");
            match got_node {
                Some(n) => assert_eq!(n, node, "node must survive {key}"),
                None => assert!(
                    crate::disks_reconciler::is_globally_unique_id(id),
                    "{key} lost its node without being a hardware id"
                ),
            }
        }
    }

    /// The live ConfigMap at the moment the disk went missing from the page. Both
    /// records for the easystore have to be readable; previously the second was not.
    #[test]
    fn the_configmap_that_hid_a_draining_disk_now_parses_completely() {
        let keys = [
            "node1--dev-sda",
            "node1--dev-sdb",
            "node1--dev-sdc",
            "node1--serial-wwn-0x50014ee214caf529",
            "node1--system",
            "node3--system",
            "serial-wwn-0x50014ee214caf529",
        ];
        let parsed: Vec<_> = keys.iter().map(|k| split_record_key(k)).collect();
        assert_eq!(parsed.len(), keys.len(), "no key may be skipped");
        // The one that was invisible.
        assert_eq!(parsed[6], (None, "serial-wwn-0x50014ee214caf529"));
        // And it names the same disk as the node-scoped one beside it.
        assert_eq!(parsed[3].1, parsed[6].1);
    }
}
