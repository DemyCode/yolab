use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

use crate::disks_reconciler::{is_globally_unique_id, record_key, SYSTEM_OSD_ID};
use crate::host::Host;
use crate::storage::settings;
use crate::store::DiskIntent;
use crate::AppState;

#[derive(Serialize, Debug)]
pub struct DiskInfo {
    pub id: String,
    pub device: String,
    pub model: String,
    pub size_bytes: u64,
    pub is_loop: bool,
    pub is_virtual: bool,
    pub is_our_osd: bool,
    pub foreign_ceph: bool,
    pub osd_id: Option<i64>,
    pub desired: String,
    pub connected: bool,
    pub has_partitions: bool,
    pub mounted: bool,
    pub phase: String,
    pub message: String,
    pub attempts: u32,
}

#[derive(Deserialize)]
pub struct SetState {
    pub desired: String,
}

fn split_record_key(key: &str) -> (Option<&str>, &str) {
    if is_globally_unique_id(key) {
        return (None, key);
    }
    match key.split_once("--") {
        Some((node, id)) => (Some(node), id),
        None => (None, key),
    }
}

type Inventory = HashMap<String, HashMap<String, Value>>;

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
        is_virtual: flag("is_virtual"),
        is_our_osd: flag("is_our_osd"),
        foreign_ceph: flag("foreign_ceph"),
        osd_id: meta.and_then(|v| v["osd_id"].as_i64()),
    }
}

fn disk_list(
    desired: &HashMap<String, String>,
    live: &Inventory,
) -> HashMap<String, Vec<DiskInfo>> {
    let mut result: HashMap<String, Vec<DiskInfo>> = HashMap::new();
    let mut listed: std::collections::HashSet<(String, String)> = Default::default();

    for (key, setting) in desired {
        let (scoped_node, disk_id) = split_record_key(key);
        let node = match scoped_node {
            Some(n) => n.to_string(),
            None => live
                .iter()
                .find(|(_, disks)| disks.contains_key(disk_id))
                .map(|(n, _)| n.clone())
                .unwrap_or_default(),
        };
        let meta = live.get(&node).and_then(|m| m.get(disk_id));
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

pub async fn list_disks(
    State(s): State<AppState>,
) -> crate::error::Result<Json<HashMap<String, Vec<DiskInfo>>>> {
    let host = s.host;
    let desired: HashMap<String, String> = settings::dump(&host, settings::DISKS)
        .await?
        .into_iter()
        .collect();
    let live = parse_inventory(&settings::dump(&host, settings::DISK_STATUS).await?);
    Ok(Json(disk_list(&desired, &live)))
}

pub async fn movement(State(s): State<AppState>) -> Json<crate::storage::movement::Movement> {
    Json(crate::storage::movement::assess_via(&s.host).await)
}

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

pub(crate) async fn record_switch<H: Host>(
    host: &H,
    node: &str,
    disk_id: &str,
    desired: &str,
) -> Result<(), String> {
    if let Some(error) = refuse_state_change(disk_id, desired) {
        return Err(error.to_string());
    }
    let key = format!("{}{}", settings::DISKS, record_key(node, disk_id));
    settings::set(host, &key, desired)
        .await
        .map_err(|e| format!("the setting could not be saved: {e}"))?;
    record_intent(node, disk_id, desired);
    crate::runtime::wake("disks");
    Ok(())
}

pub(crate) async fn forget_disk<H: Host>(
    host: &H,
    node: &str,
    disk_id: &str,
) -> Result<(), String> {
    let key = format!("{}{}", settings::DISKS, record_key(node, disk_id));
    match host.ceph(&["config-key", "rm", &key]).await {
        Ok(_) => {}
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(format!("the disk record could not be removed: {e}")),
    }
    store_intent(node, disk_id, DiskIntent::Forgotten);
    Ok(())
}

fn record_intent(node: &str, disk_id: &str, desired: &str) {
    let intent = if desired == "ON" {
        DiskIntent::On
    } else {
        DiskIntent::Off
    };
    store_intent(node, disk_id, intent);
}

fn store_intent(node: &str, disk_id: &str, intent: DiskIntent) {
    let mut store = crate::store::locked();
    if let Err(e) = store.set_disk_intent(node, disk_id, intent) {
        tracing::warn!(
            "{node}--{disk_id}: the switch was saved but not recorded as a choice ({e}) — the \
             next reconcile will take it from the cluster settings"
        );
        return;
    }
    if let Err(e) = store.persist(&crate::store::default_path()) {
        tracing::warn!("{node}--{disk_id}: the choice could not be saved to disk ({e})");
    }
}

pub async fn set_disk_state(
    Path((node, id)): Path<(String, String)>,
    State(s): State<AppState>,
    Json(body): Json<SetState>,
) -> Json<Value> {
    match record_switch(&s.host, &node, &id, &body.desired).await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(error) => Json(serde_json::json!({"ok": false, "error": error})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn switching_the_system_disk_off_is_refused_before_anything_is_written() {
        let host = FakeHost::new();
        let refused = record_switch(&host, "node1", SYSTEM_OSD_ID, "OFF").await;
        assert!(refused.unwrap_err().contains("system disk"));
        assert!(host.calls().is_empty(), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_switch_that_cannot_be_saved_says_so() {
        let host = FakeHost::new().fail("ceph config-key set", "mon unreachable");
        let failed = record_switch(&host, "node1", "dev-sdb", "ON").await;
        assert!(
            failed.unwrap_err().contains("could not be saved"),
            "{:?}",
            host.calls()
        );
        assert!(host.ran("ceph config-key set"));
    }

    #[tokio::test]
    async fn a_switch_is_saved_in_the_cluster_settings_under_its_machine() {
        let host = FakeHost::new().ok("ceph config-key set", "");
        record_switch(&host, "node1", "dev-sdb", "ON")
            .await
            .unwrap();
        record_switch(&host, "node1", "dev-sdc", "OFF")
            .await
            .unwrap();
        assert!(host.ran("ceph config-key set yolab/disks/node1--dev-sdb ON"));
        assert!(host.ran("ceph config-key set yolab/disks/node1--dev-sdc OFF"));
    }

    #[tokio::test]
    async fn only_on_and_off_are_switch_positions() {
        let host = FakeHost::new();
        let refused = record_switch(&host, "node1", "dev-sdb", "maybe").await;
        assert_eq!(refused.unwrap_err(), "desired must be ON or OFF");
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_disk_removes_its_record_and_an_absent_one_is_already_forgotten() {
        let host = FakeHost::new().ok("ceph config-key rm", "");
        forget_disk(&host, "node1", "dev-sdb").await.unwrap();
        assert!(host.ran("ceph config-key rm yolab/disks/node1--dev-sdb"));

        let gone = FakeHost::new().fail("ceph config-key rm", "Error ENOENT: no such key");
        forget_disk(&gone, "node1", "dev-sdb").await.unwrap();

        let down = FakeHost::new().fail("ceph config-key rm", "Error EACCES: access denied");
        let err = forget_disk(&down, "node1", "dev-sdb").await.unwrap_err();
        assert!(err.contains("could not be removed"), "{err}");
    }

    mod over_http {
        use super::*;
        use crate::testkit::TestApi;

        async fn signed_in(host: FakeHost) -> TestApi {
            TestApi::with_kube_and_host(crate::k8s::testing::unreachable(), host)
                .login()
                .await
        }

        #[tokio::test]
        async fn the_disks_page_lists_each_machine_s_disks_with_their_switch() {
            let desired = serde_json::json!({ "yolab/disks/node1--dev-sdb": "ON" }).to_string();
            let status = serde_json::json!({
                "yolab/disk-status/node1": serde_json::json!({
                    "disks": {
                        "dev-sdb": { "device": "/dev/sdb", "size_bytes": 1000, "phase": "Ready" },
                        "dev-sdc": { "device": "/dev/sdc", "size_bytes": 2000 }
                    }
                }).to_string(),
                "yolab/disk-status/node2": "not json"
            })
            .to_string();
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/disks/", &desired)
                .ok("ceph config-key dump yolab/disk-status/", &status);
            let api = signed_in(host).await;

            let res = api.get("/api/disks").await;
            let node1 = res.json()["node1"].clone();
            assert_eq!(node1.as_array().map(Vec::len), Some(2), "{}", res.body);
            let sdb = node1
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["id"] == "dev-sdb")
                .cloned()
                .unwrap();
            assert_eq!(sdb["desired"], "ON");
            assert_eq!(sdb["phase"], "Ready");
            assert!(res.json().get("node2").is_none());
        }

        #[tokio::test]
        async fn the_disks_page_is_an_error_while_ceph_cannot_be_read() {
            let api = signed_in(FakeHost::new().fail("ceph config-key dump", "timed out")).await;
            assert!(!api.get("/api/disks").await.status.is_success());
        }

        #[tokio::test]
        async fn a_switch_over_http_answers_ok_or_the_reason() {
            let api = signed_in(FakeHost::new().ok("ceph config-key set", "")).await;
            let res = api
                .send(
                    "PUT",
                    "/api/disks/node1/dev-sdb",
                    Some(r#"{"desired":"OFF"}"#),
                )
                .await;
            assert_eq!(
                res.json(),
                serde_json::json!({ "ok": true }),
                "{}",
                res.body
            );
            let res = api
                .send(
                    "PUT",
                    "/api/disks/node1/system",
                    Some(r#"{"desired":"OFF"}"#),
                )
                .await;
            assert_eq!(res.json()["ok"], false);
            assert!(res.json()["error"]
                .as_str()
                .unwrap()
                .contains("system disk"));
        }
    }

    #[test]
    fn a_node_scoped_key_splits_into_node_and_disk() {
        assert_eq!(
            split_record_key("node1--dev-sda"),
            (Some("node1"), "dev-sda")
        );
        assert_eq!(split_record_key("node1--system"), (Some("node1"), "system"));
    }

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

    #[test]
    fn an_unrecognised_key_is_surfaced_rather_than_dropped() {
        assert_eq!(split_record_key("weird"), (None, "weird"));
        assert_eq!(split_record_key(""), (None, ""));
    }

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
        assert_eq!(parsed[6], (None, "serial-wwn-0x50014ee214caf529"));
        assert_eq!(parsed[3].1, parsed[6].1);
    }

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
