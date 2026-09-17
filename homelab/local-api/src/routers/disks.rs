use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

use crate::disks_reconciler::{is_globally_unique_id, record_key, SYSTEM_OSD_ID};
use crate::host::RealHost;
use crate::storage::settings;
use crate::AppState;

#[derive(Serialize, Debug)]
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
    /// Where the reconciler has got to with this disk: see disks_reconciler::Phase.
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

/// The inverse of `disks_reconciler::record_key`: the node a record is scoped to
/// (None for a hardware id, which belongs to the disk wherever it is plugged in)
/// and the disk id. A key in neither shape is still returned, never skipped — a
/// record nobody can parse still governs a disk.
fn split_record_key(key: &str) -> (Option<&str>, &str) {
    if is_globally_unique_id(key) {
        return (None, key);
    }
    match key.split_once("--") {
        Some((node, id)) => (Some(node), id),
        None => (None, key),
    }
}

/// node → disk id → the metadata that node last published.
type Inventory = HashMap<String, HashMap<String, Value>>;

/// Parses each node's published inventory. A node whose payload does not parse
/// is left out and logged: its disks show as not connected rather than wrong.
fn parse_inventory(published: &BTreeMap<String, String>) -> Inventory {
    published
        .iter()
        .filter_map(|(node, raw)| match serde_json::from_str::<Value>(raw) {
            Ok(v) => {
                let disks = v["disks"].as_object()?;
                Some((
                    node.clone(),
                    disks.iter().map(|(k, m)| (k.clone(), m.clone())).collect(),
                ))
            }
            Err(e) => {
                tracing::warn!("disk inventory published by {node} is unreadable: {e}");
                None
            }
        })
        .collect()
}

fn disk_info(disk_id: &str, desired: &str, meta: Option<&Value>) -> DiskInfo {
    let text = |field: &str| {
        meta.and_then(|v| v[field].as_str())
            .unwrap_or("")
            .to_string()
    };
    let flag = |field: &str| meta.and_then(|v| v[field].as_bool()).unwrap_or(false);
    DiskInfo {
        id: disk_id.to_string(),
        desired: desired.to_string(),
        connected: meta.is_some(),
        device: text("device"),
        model: text("model"),
        size_bytes: meta.and_then(|v| v["size_bytes"].as_u64()).unwrap_or(0),
        has_partitions: flag("has_partitions"),
        mounted: flag("mounted"),
        phase: text("phase"),
        message: text("message"),
        attempts: meta
            .and_then(|v| v["attempts"].as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        is_loop: flag("is_loop"),
        is_our_osd: flag("is_our_osd"),
        foreign_ceph: flag("foreign_ceph"),
        osd_id: meta.and_then(|v| v["osd_id"].as_i64()),
    }
}

/// Every disk the page should show, by node: every record (connected or not),
/// plus every connected disk that has no record yet — the system disk, which
/// never needs one, and a disk seen before its first registration.
fn disk_list(
    desired: &HashMap<String, String>,
    live: &Inventory,
) -> HashMap<String, Vec<DiskInfo>> {
    let mut result: HashMap<String, Vec<DiskInfo>> = HashMap::new();
    let mut listed: std::collections::HashSet<(String, String)> = Default::default();

    for (key, setting) in desired {
        let (scoped_node, disk_id) = split_record_key(key);
        // A hardware-id record is shown under whichever node sees the disk now;
        // a disk nobody sees stays listed, under no node ("").
        let node = match scoped_node {
            Some(n) => n.to_string(),
            None => live
                .iter()
                .find(|(_, disks)| disks.contains_key(disk_id))
                .map(|(n, _)| n.clone())
                .unwrap_or_default(),
        };
        let meta = live.get(&node).and_then(|m| m.get(disk_id));
        // The system disk is ON whatever a record says (`wants_on`).
        let setting = if disk_id == SYSTEM_OSD_ID {
            "ON"
        } else {
            setting
        };
        listed.insert((node.clone(), disk_id.to_string()));
        result
            .entry(node)
            .or_default()
            .push(disk_info(disk_id, setting, meta));
    }

    for (node, disks) in live {
        for (disk_id, meta) in disks {
            if listed.contains(&(node.clone(), disk_id.clone())) {
                continue;
            }
            let setting = if disk_id == SYSTEM_OSD_ID {
                "ON"
            } else {
                "OFF"
            };
            result
                .entry(node.clone())
                .or_default()
                .push(disk_info(disk_id, setting, Some(meta)));
        }
    }

    // Connected first, then the system disk, then by size, then by id so the
    // order never shuffles between refreshes.
    for disks in result.values_mut() {
        disks.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.is_loop.cmp(&a.is_loop))
                .then(b.size_bytes.cmp(&a.size_bytes))
                .then(a.id.cmp(&b.id))
        });
    }
    result
}

/// GET /api/disks. An unreadable settings store is an error, never an empty
/// page: "no disks" and "cannot tell" must not look the same.
pub async fn list_disks(
    State(_s): State<AppState>,
) -> crate::error::Result<Json<HashMap<String, Vec<DiskInfo>>>> {
    let desired: HashMap<String, String> = settings::dump(&RealHost, settings::DISKS)
        .await?
        .into_iter()
        .collect();
    let live = parse_inventory(&settings::dump(&RealHost, settings::DISK_STATUS).await?);
    Ok(Json(disk_list(&desired, &live)))
}

/// Why a requested ON/OFF must not be recorded, if it must not.
///
/// The system disk cannot be switched off: this machine's container images live
/// on it (see `storage::containerd_store`), so draining it would leave the node
/// unable to run anything. The reconciler treats it as ON regardless
/// (`disks_reconciler::wants_on`); refusing here keeps the page from showing a
/// switch that does nothing.
fn refuse_state_change(disk_id: &str, desired: &str) -> Option<&'static str> {
    match desired {
        "ON" => None,
        "OFF" if disk_id == SYSTEM_OSD_ID => Some(
            "The system disk holds this machine's container images and cannot be switched off.",
        ),
        "OFF" => None,
        _ => Some("desired must be ON or OFF"),
    }
}

/// Records the owner's switch for one disk and wakes the disk controller. Shared
/// by the Storage page's toggle and the Ceph page's per-OSD buttons, so there is
/// one writer and one set of rules.
pub(crate) async fn record_switch(node: &str, disk_id: &str, desired: &str) -> Result<(), String> {
    if let Some(error) = refuse_state_change(disk_id, desired) {
        return Err(error.to_string());
    }
    let key = format!("{}{}", settings::DISKS, record_key(node, disk_id));
    settings::set(&RealHost, &key, desired)
        .await
        .map_err(|e| format!("the setting could not be saved: {e}"))?;
    crate::runtime::wake("disks");
    Ok(())
}

pub async fn set_disk_state(
    Path((node, id)): Path<(String, String)>,
    State(_s): State<AppState>,
    Json(body): Json<SetState>,
) -> Json<Value> {
    match record_switch(&node, &id, &body.desired).await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(error) => Json(serde_json::json!({"ok": false, "error": error})),
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
        assert_eq!(
            split_record_key("node1--dev-sda"),
            (Some("node1"), "dev-sda")
        );
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

    /// The live record set at the moment the disk went missing from the page. Both
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

    // ── The page's list ───────────────────────────────────────────────────────

    fn inventory(node: &str, disks: Value) -> Inventory {
        parse_inventory(&BTreeMap::from([(
            node.to_string(),
            serde_json::json!({ "disks": disks }).to_string(),
        )]))
    }

    fn ids(list: &[DiskInfo]) -> Vec<(&str, &str, bool)> {
        list.iter()
            .map(|d| (d.id.as_str(), d.desired.as_str(), d.connected))
            .collect()
    }

    #[test]
    fn records_and_connected_disks_without_records_are_all_listed() {
        let desired = HashMap::from([
            ("node1--dev-sdb".to_string(), "ON".to_string()),
            // Unplugged, still switched on: listed, not connected.
            ("node1--dev-sdz".to_string(), "ON".to_string()),
        ]);
        let live = inventory(
            "node1",
            serde_json::json!({
                "system": {"device": "/dev/mapper/pool-ceph", "is_loop": true, "size_bytes": 10},
                "dev-sdb": {"device": "sdb", "size_bytes": 50},
                "dev-sdc": {"device": "sdc", "size_bytes": 20},
            }),
        );

        let list = disk_list(&desired, &live);

        assert_eq!(
            ids(&list["node1"]),
            vec![
                ("system", "ON", true),
                ("dev-sdb", "ON", true),
                ("dev-sdc", "OFF", true),
                ("dev-sdz", "ON", false),
            ]
        );
    }

    #[test]
    fn a_hardware_id_record_is_listed_under_the_node_that_sees_the_disk() {
        let desired = HashMap::from([("serial-wwn-0xabc".to_string(), "OFF".to_string())]);
        let live = inventory(
            "node3",
            serde_json::json!({"serial-wwn-0xabc": {"device": "sdb"}}),
        );

        let list = disk_list(&desired, &live);

        assert_eq!(ids(&list["node3"]), vec![("serial-wwn-0xabc", "OFF", true)]);
        assert!(!list.contains_key(""), "the disk is not listed twice");
    }

    #[test]
    fn the_system_disk_is_shown_on_whatever_a_record_says() {
        let desired = HashMap::from([("node1--system".to_string(), "OFF".to_string())]);
        let live = inventory("node1", serde_json::json!({"system": {"is_loop": true}}));
        assert_eq!(
            ids(&disk_list(&desired, &live)["node1"]),
            vec![("system", "ON", true)]
        );
    }

    #[test]
    fn an_unreadable_node_inventory_is_left_out_not_guessed() {
        let published = BTreeMap::from([
            ("node1".to_string(), "not json".to_string()),
            (
                "node2".to_string(),
                r#"{"disks":{"dev-sda":{}}}"#.to_string(),
            ),
        ]);
        let live = parse_inventory(&published);
        assert!(!live.contains_key("node1"));
        assert!(live["node2"].contains_key("dev-sda"));
    }

    #[test]
    fn metadata_fields_are_read_into_the_row() {
        let meta = serde_json::json!({
            "device": "sdb", "model": "WD", "size_bytes": 1000, "is_our_osd": true,
            "osd_id": 4, "phase": "active", "message": "In use", "attempts": 2,
        });
        let row = disk_info("dev-sdb", "ON", Some(&meta));
        assert_eq!((row.device.as_str(), row.model.as_str()), ("sdb", "WD"));
        assert_eq!(
            (row.size_bytes, row.osd_id, row.attempts),
            (1000, Some(4), 2)
        );
        assert!(row.is_our_osd && row.connected && !row.mounted);
        assert_eq!(
            (row.phase.as_str(), row.message.as_str()),
            ("active", "In use")
        );
    }

    #[test]
    fn the_system_disk_cannot_be_switched_off_and_only_on_or_off_is_accepted() {
        assert!(
            refuse_state_change("system", "OFF").is_some_and(|e| e.contains("container images"))
        );
        assert_eq!(refuse_state_change("system", "ON"), None);
        assert_eq!(refuse_state_change("dev-sdb", "OFF"), None);
        assert_eq!(refuse_state_change("dev-sdb", "ON"), None);
        assert!(refuse_state_change("dev-sdb", "USING").is_some());
    }
}
