use super::*;

use crate::host::fake::FakeHost;
use crate::runtime::Resource as _;
use crate::storage::wait::not_yet;

const OURS: &str = "11111111-2222-3333-4444-555555555555";
const THEIRS: &str = "99999999-8888-7777-6666-555555555555";

fn put(path: &str, bytes: &[u8]) {
    let p = crate::storage::on_machine(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn link(path: &str, target: &str) {
    let p = crate::storage::on_machine(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, p).unwrap();
}

fn fsid_json(fsid: &str) -> String {
    format!(r#"{{"fsid":"{fsid}"}}"#)
}

fn ours_host() -> FakeHost {
    FakeHost::new().ok("ceph fsid", &fsid_json(OURS))
}

fn listed(dev: &str, id: i64, fsid: &str) -> String {
    json!({ id.to_string(): [{"devices": [dev], "tags": {"ceph.cluster_fsid": fsid}}] }).to_string()
}

fn disk_on(device: &str, ownership: Ownership) -> Disk {
    Disk {
        device: device.into(),
        model: String::new(),
        size_bytes: 1_000_000_000,
        is_loop: false,
        is_virtual: false,
        ownership,
        has_partitions: false,
        mounted: false,
        osd_id: None,
        progress: None,
    }
}

fn ctx(host: FakeHost, node: &str) -> crate::runtime::Ctx {
    crate::runtime::Ctx {
        node: node.into(),
        tools: crate::tools::Tools {
            host,
            kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::client(),
        },
    }
}

fn published(host: &FakeHost, node: &str) -> Value {
    let prefix = format!("ceph config-key set yolab/disk-status/{node} ");
    let raw = host
        .calls()
        .into_iter()
        .rev()
        .find_map(|c| c.strip_prefix(&prefix).map(str::to_string))
        .expect("the disk status was published");
    serde_json::from_str(&raw).unwrap()
}

const LSBLK: &str = r#"{"blockdevices":[
    {"name":"sdd","type":"disk","mountpoints":[null]},
    {"name":"sdb","type":"disk","mountpoints":[null]},
    {"name":"sda","type":"disk","mountpoints":[null],"children":[{"name":"sda1","type":"part","mountpoints":["/"]}]},
    {"name":"sdc","type":"disk","mountpoints":"/mnt"},
    {"name":"sr0","type":"rom"},
    {"name":"","type":"disk"},
    {"name":"zram0","type":"disk"}
]}"#;

fn a_machine_with_a_system_lv_a_usb_disk_and_a_virtual_disk() {
    put("/dev/sdb", &[0u8; 4096]);
    put("/sys/block/sdb/size", b"2048\n");
    put("/sys/block/sdb/device/model", b"Disk  \n");
    link("/dev/disk/by-id/ata-SAMSUNG", "../../sdb");
    link("/dev/disk/by-id/wwn-0xabc", "../../sdb");
    link("/dev/disk/by-id/scsi-0xabc", "../../sdb");
    link("/dev/disk/by-id/ata-SAMSUNG-part1", "../../sdb1");
    link("/dev/disk/by-id/dm-name-pool", "../../sdb");
    link("/dev/disk/by-id/usb-other", "../../sdc");
    put("/dev/dm-0", &[0u8; 4096]);
    link("/dev/mapper/pool-ceph", "../dm-0");
    put("/sys/block/dm-0/size", b"4096\n");
    put("/dev/mapper/yolab_vdisk_3-osd", &[0u8; 4096]);
    std::fs::create_dir_all(crate::storage::on_machine("/run/yolab/vdisks/3")).unwrap();
    std::fs::create_dir_all(crate::storage::on_machine("/run/yolab/vdisks/7")).unwrap();
    std::fs::create_dir_all(crate::storage::on_machine("/run/yolab/vdisks/notes")).unwrap();
}

fn a_cluster_that_answers(host: FakeHost) -> FakeHost {
    host.ok("lsblk -J -o NAME,TYPE,MOUNTPOINTS", LSBLK)
        .ok("ceph fsid", &fsid_json(OURS))
        .ok(
            "ceph-volume lvm list",
            &listed("/dev/mapper/pool-ceph", 0, OURS),
        )
        .ok("ceph config-key set", "")
        .ok("ceph -s", "")
        .ok("ceph osd ls", "[0]")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@0.service",
            "active",
        )
        .ok(
            "ceph osd df tree",
            r#"{"nodes":[{"id":0,"type":"osd","status":"up","crush_weight":0.5,"reweight":1.0,"kb":1000}]}"#,
        )
        .fail(
            "ceph config-key get yolab/storage-policy",
            "Error ENOENT: key 'yolab/storage-policy' doesn't exist",
        )
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":0,"up":1,"in":1,"up_from":3}],"pools":[]}"#,
        )
}

#[tokio::test]
async fn the_disks_controller_runs_on_every_node_once_ceph_is_up_and_waits_out_a_restore() {
    let c = DisksController;
    assert_eq!(c.name(), "disks");
    assert_eq!(c.scope(), crate::runtime::Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(60));
    assert_eq!(c.depends_on(), &[crate::runtime::CEPH]);
    assert_eq!(c.pauses_during(), &[crate::runtime::Activity::Restore]);
}

#[tokio::test]
async fn a_node_without_a_name_is_an_error_before_any_disk_is_looked_at() {
    let host = FakeHost::new();
    let err = DisksController
        .converge(&ctx(host.clone(), ""))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "cannot determine this node's name");
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_tick_publishes_every_disk_this_machine_has_with_its_size_model_and_osd() {
    a_machine_with_a_system_lv_a_usb_disk_and_a_virtual_disk();
    let host = a_cluster_that_answers(FakeHost::new()).ok("ceph config-key dump yolab/disks/", "{}");

    let tick = DisksController
        .converge(&ctx(host.clone(), "node-pub"))
        .await
        .unwrap();

    assert_eq!(tick, crate::runtime::Tick::Done);
    let disks = &published(&host, "node-pub")["disks"];
    assert_eq!(
        disks["serial-wwn-0xabc"],
        json!({
            "device": "sdb", "model": "Disk", "size_bytes": 2048 * 512,
            "is_our_osd": false, "foreign_ceph": false, "ownership": "blank",
            "has_partitions": false, "mounted": false, "osd_id": null,
            "phase": "removable", "message": "Not in use. Safe to unplug.", "attempts": 0,
        })
    );
    assert_eq!(disks["dev-sdd"]["ownership"], json!("unknown"));
    assert_eq!(disks["dev-sdd"]["size_bytes"], json!(0));
    assert_eq!(disks["system"]["osd_id"], json!(0));
    assert_eq!(disks["system"]["is_loop"], json!(true));
    assert_eq!(disks["system"]["size_bytes"], json!(4096 * 512));
    assert_eq!(disks["vdisk-3"]["is_virtual"], json!(true));
    assert_eq!(disks["vdisk-3"]["model"], json!("Virtual disk"));
    assert_eq!(disks["vdisk-3"]["size_bytes"], json!(0));
    assert_eq!(disks.get("vdisk-7"), None);
    assert_eq!(disks.as_object().unwrap().len(), 4);
    assert!(!host.ran("ceph config-key set yolab/disks/"));
}

#[tokio::test]
async fn a_store_that_cannot_list_its_claims_registers_new_disks_in_the_cluster_settings() {
    a_machine_with_a_system_lv_a_usb_disk_and_a_virtual_disk();
    crate::store::locked().corrupt_disk_claim("node0--dev-gone");
    let host = a_cluster_that_answers(FakeHost::new())
        .ok("ceph config-key dump yolab/disks/", "{}")
        .fail("ceph config-key dump yolab/disks/", "timed out");

    DisksController
        .converge(&ctx(host.clone(), "node-reg"))
        .await
        .unwrap();

    assert!(host.ran("ceph config-key set yolab/disks/serial-wwn-0xabc OFF"));
    assert!(host.ran("ceph config-key set yolab/disks/node-reg--dev-sdd OFF"));
    assert!(host.ran("ceph config-key set yolab/disks/node-reg--vdisk-3 OFF"));
    assert_eq!(
        progress_of("serial-wwn-0xabc").message,
        "Not in use. Safe to unplug."
    );
}

#[tokio::test]
async fn unreadable_disk_settings_mark_every_disk_unknown_and_change_nothing() {
    put("/dev/sdq", &[0u8; 4096]);
    let host = FakeHost::new()
        .ok(
            "lsblk -J -o NAME,TYPE,MOUNTPOINTS",
            r#"{"blockdevices":[{"name":"sdq","type":"disk","mountpoints":[null]}]}"#,
        )
        .fail("ceph fsid", "error connecting to the cluster")
        .fail("ceph config-key dump yolab/disks/", "error connecting to the cluster")
        .ok("ceph config-key set", "");

    publish_local(&host, "node-unk").await.unwrap();

    assert_eq!(progress_of("dev-sdq").phase, Phase::Unknown);
    assert_eq!(
        progress_of("dev-sdq").message,
        "Cannot reach this machine's settings right now. Nothing will be changed until it can."
    );
    assert!(!host.ran("ceph-volume"));
    assert_eq!(
        published(&host, "node-unk")["disks"]["dev-sdq"]["phase"],
        json!("unknown")
    );
}

#[tokio::test]
async fn a_disk_list_that_cannot_be_read_skips_the_tick() {
    let refused = FakeHost::new().unstartable("lsblk");
    assert!(publish_local(&refused, "n").await.is_err());

    let failed = FakeHost::new().fail("lsblk", "lsblk: not found");
    let err = scan_devices(&failed).await.unwrap_err();
    assert!(err.to_string().starts_with("lsblk failed: "), "{err}");

    let garbled = FakeHost::new().ok("lsblk", "not json");
    assert_eq!(
        scan_devices(&garbled).await.unwrap_err().to_string(),
        "parse lsblk -J"
    );

    let empty = FakeHost::new().ok("lsblk", "{}");
    assert_eq!(
        scan_devices(&empty).await.unwrap_err().to_string(),
        "lsblk -J has no blockdevices list"
    );
}

#[test]
fn a_virtual_disk_says_so_on_the_wire() {
    let mut d = disk_on("/dev/mapper/yolab_vdisk_1-osd", Ownership::Blank);
    d.is_virtual = true;
    assert_eq!(d.to_value()["is_virtual"], json!(true));
    assert_eq!(d.to_value().get("is_loop"), None);
}

#[test]
fn a_blocked_disk_and_one_that_failed_three_times_are_stuck() {
    set_phase("stuck-blocked", Phase::Blocked, "mounted elsewhere");
    set_phase("stuck-retrying", Phase::Retrying, "zap failed");
    progress_table()
        .get_mut("stuck-retrying")
        .unwrap()
        .attempts = 3;
    set_phase("stuck-not-yet", Phase::Retrying, "first failure");

    let stuck: Vec<(String, String)> = stuck_disks()
        .into_iter()
        .filter(|(d, _)| d.starts_with("stuck-"))
        .collect();

    assert_eq!(
        stuck,
        vec![
            ("stuck-blocked".to_string(), "mounted elsewhere".to_string()),
            ("stuck-retrying".to_string(), "zap failed".to_string()),
        ]
    );
}

#[test]
fn a_bare_device_name_is_read_under_dev() {
    put("/dev/sdx", &[0u8; 4096]);
    assert_eq!(bluestore_fsid("sdx").unwrap(), None);
}

#[test]
fn a_label_without_an_fsid_key_or_with_a_truncated_or_binary_fsid_has_no_fsid() {
    let mut no_key = [0u8; 4096];
    no_key[..BLUESTORE_MAGIC.len()].copy_from_slice(BLUESTORE_MAGIC);
    assert_eq!(fsid_in_label(&no_key), None);

    let mut at_the_end = no_key;
    let at = 4096 - CEPH_FSID_KEY.len() - 10;
    at_the_end[at..at + CEPH_FSID_KEY.len()].copy_from_slice(CEPH_FSID_KEY);
    assert_eq!(fsid_in_label(&at_the_end), None);

    let mut binary = no_key;
    let at = 512;
    binary[at..at + CEPH_FSID_KEY.len()].copy_from_slice(CEPH_FSID_KEY);
    let vs = at + CEPH_FSID_KEY.len();
    binary[vs..vs + 4].copy_from_slice(&36u32.to_le_bytes());
    binary[vs + 4..vs + 40].fill(0xff);
    assert_eq!(fsid_in_label(&binary), None);
}

#[test]
fn only_physical_disks_count_as_user_disks() {
    assert!(is_user_disk("sda"));
    assert!(is_user_disk("nvme0n1"));
    for virtual_disk in ["rbd0", "loop1", "zram0", "zd16", "md127", "dm-3"] {
        assert!(!is_user_disk(virtual_disk), "{virtual_disk}");
    }
}

#[tokio::test]
async fn the_system_lv_attempt_needs_the_lv_on_this_machine() {
    let host = FakeHost::new();
    let err = system_osd_attempt(&host).await.unwrap_err();
    assert!(
        err.to_string()
            .starts_with("/dev/mapper/pool-ceph does not exist"),
        "{err}"
    );
}

#[tokio::test]
async fn an_lv_attempt_whose_osd_list_cannot_be_read_is_an_error() {
    let dev = "/dev/lv-list-fails";
    put(dev, &[0u8; 4096]);
    let host = ours_host().fail("ceph-volume lvm list", "timed out");
    assert!(lv_osd_attempt(&host, dev).await.is_err());
}

#[tokio::test]
async fn an_lv_attempt_whose_erase_fails_is_an_error() {
    let dev = "/dev/lv-zap-fails";
    put(dev, &[0u8; 4096]);
    let host = ours_host()
        .ok("ceph-volume lvm list", &listed(dev, 3, OURS))
        .ok("ceph osd ls", "[]")
        .fail("ceph-volume lvm zap", "device busy");
    assert!(lv_osd_attempt(&host, dev).await.is_err());
    assert!(!host.ran("lvm create"));
}

#[tokio::test]
async fn an_lv_attempt_that_cannot_list_osds_after_creating_is_an_error() {
    let dev = "/dev/lv-relist-fails";
    put(dev, &[0u8; 4096]);
    let host = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .ok("ceph-volume lvm list", "{}")
        .ok("ceph-volume lvm list", "{}")
        .fail("ceph-volume lvm list", "timed out")
        .ok("wipefs --all", "")
        .ok("ceph-volume lvm create", "");
    assert!(lv_osd_attempt(&host, dev).await.is_err());
    assert!(host.ran("lvm create"));
}

#[tokio::test]
async fn the_mon_is_asked_when_ceph_volume_is_busy_and_an_empty_or_silent_mon_is_unknown() {
    let meta = HashMap::from([
        ("disk-mon-a".to_string(), disk_on("sdb", Ownership::Blank)),
        ("disk-mon-gone".to_string(), disk_on("", Ownership::Blank)),
    ]);

    let empty = FakeHost::new()
        .fail("ceph-volume lvm list", "busy")
        .ok("ceph osd metadata", "[]");
    assert!(fetch_disk_to_osd(&empty, "node1", &meta).await.is_none());

    let silent = FakeHost::new()
        .fail("ceph-volume lvm list", "busy")
        .fail("ceph osd metadata", "timed out");
    assert!(fetch_disk_to_osd(&silent, "node1", &meta).await.is_none());

    let elsewhere = ours_host().ok("ceph-volume lvm list", &listed("/dev/sdz", 4, OURS));
    let (map, source) = fetch_disk_to_osd(&elsewhere, "node1", &meta).await.unwrap();
    assert_eq!(map, HashMap::new());
    assert_eq!(source, OsdMapSource::CephVolume);
}

#[test]
fn the_mons_metadata_gives_the_bluestore_node_and_every_listed_device() {
    let raw = json!([
        {"id": 1, "hostname": "node1", "bluestore_bdev_dev_node": "/dev/sdb"},
        {"id": 2, "hostname": "node1", "bluestore_bdev_dev_node": "unknown", "devices": "sdc, sdd,"},
        {"id": 3, "hostname": "node2", "devices": "sde"},
        {"hostname": "node1", "devices": "sdf"},
    ]);
    assert_eq!(
        parse_osd_metadata(&raw, "node1"),
        vec![
            ("/dev/sdb".to_string(), 1),
            ("sdc".to_string(), 2),
            ("sdd".to_string(), 2),
        ]
    );
    assert_eq!(parse_osd_metadata(&json!({}), "node1"), vec![]);
}

#[tokio::test]
async fn the_local_osd_list_needs_the_clusters_id() {
    let host = FakeHost::new()
        .ok("ceph-volume lvm list", "{}")
        .fail("ceph fsid", "error connecting to the cluster");
    assert!(local_osds(&host).await.is_err());
}

#[test]
fn an_osd_without_an_id_has_no_weight_in_the_view() {
    let view = ClusterView::new(
        vec![
            json!({"type": "osd", "status": "up", "crush_weight": 1.0}),
            json!({"id": 4, "type": "osd", "status": "down", "crush_weight": 0.5, "reweight": 0.0, "kb": 7}),
        ],
        std::collections::HashSet::new(),
        None,
        "osd".into(),
    );
    assert_eq!(view.weights, HashMap::from([(4, (0.5, 0.0, 7))]));
    assert!(view.up.is_empty());
}

#[tokio::test]
async fn the_view_carries_the_chosen_policy_and_the_osds_that_never_booted() {
    let host = FakeHost::new()
        .ok("ceph osd df tree", r#"{"nodes":[]}"#)
        .ok(
            "ceph config-key get yolab/storage-policy",
            r#"{"size":2,"failure_domain":"host"}"#,
        )
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":5,"up":0,"in":1,"up_from":0},{"osd":6,"up":1,"in":1,"up_from":9}],"pools":[]}"#,
        );
    let view = observe_cluster(&host).await.unwrap();
    assert_eq!(view.want_copies, Some(2));
    assert_eq!(view.failure_domain, "host");
    assert_eq!(view.never_booted, std::collections::HashSet::from([5]));
}

#[tokio::test]
async fn a_mark_in_or_out_the_cluster_refuses_is_not_applied() {
    let steer = |mark_in, mark_out| Steer {
        set_weight: None,
        mark_in,
        mark_out,
        needs_purge: false,
        phase: None,
        message: None,
    };
    let host = FakeHost::new()
        .fail("ceph osd in", "EPERM")
        .fail("ceph osd out", "EPERM");
    assert!(!apply_steer(&host, "d", 1, &steer(true, false), 0.0).await);
    assert!(!apply_steer(&host, "d", 1, &steer(false, true), 1.0).await);
    assert!(apply_steer(&host, "d", 1, &steer(false, false), 1.0).await);
}

fn empty_view() -> ClusterView {
    ClusterView::new(vec![], std::collections::HashSet::new(), None, "osd".into())
}

const SETTLED: &str =
    r#"{"pgmap":{"num_pgs":12,"pgs_by_state":[{"state_name":"active+clean","count":12}]}}"#;

#[tokio::test]
async fn an_osd_that_stops_being_safe_once_stopped_is_started_again() {
    let host = FakeHost::new()
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[41]}"#)
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[]}"#)
        .ok("ceph status", SETTLED)
        .ok("systemctl stop yolab-ceph-osd@41.service", "")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@41.service",
            "inactive",
        )
        .ok("systemctl start yolab-ceph-osd@41.service", "");

    remove_drained(&host, "disk-recheck", &disk_on("sdb", Ownership::Ours), 41, &empty_view()).await;

    assert!(!is_stopped_for_removal(41));
    assert!(host.position("systemctl stop") < host.position("systemctl start"));
    assert!(!host.ran("purge"));
}

#[tokio::test]
async fn an_osd_is_never_purged_while_data_elsewhere_cannot_be_rebuilt() {
    let host = FakeHost::new()
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[42]}"#)
        .ok("ceph status", SETTLED)
        .ok("systemctl stop yolab-ceph-osd@42.service", "")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@42.service",
            "inactive",
        )
        .ok("systemctl start yolab-ceph-osd@42.service", "")
        .ok(
            "ceph osd dump",
            r#"{"osds":[],"pools":[{"pool":1,"pool_name":"data","size":1}]}"#,
        )
        .ok(
            "ceph pg dump pgs_brief",
            r#"[{"pgid":"1.0","state":"stale+active+clean"},{"pgid":"1.1","state":"active+clean"}]"#,
        );

    remove_drained(&host, "disk-at-risk", &disk_on("sdb", Ownership::Ours), 42, &empty_view()).await;

    assert!(!host.ran("purge-actual"));
    assert!(host.ran("systemctl start yolab-ceph-osd@42.service"));
    assert_eq!(
        progress_of("disk-at-risk").message,
        "Not removing this disk: data elsewhere in the cluster is unreadable and \
         cannot be rebuilt, and this disk may hold the only copy of it."
    );
}

#[tokio::test]
async fn a_purged_osd_without_a_device_is_removable_without_a_wipe() {
    let host = FakeHost::new()
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[43]}"#)
        .ok("ceph status", SETTLED)
        .ok("systemctl stop yolab-ceph-osd@43.service", "")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@43.service",
            "inactive",
        )
        .fail("ceph pg dump pgs_brief", "timed out")
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":43,"up":0,"in":0,"up_from":5}],"pools":[]}"#,
        )
        .ok("ceph osd purge-actual", "purged osd.43")
        .ok("ceph osd ls", "[]");

    remove_drained(&host, "disk-no-dev", &disk_on("", Ownership::Ours), 43, &empty_view()).await;

    assert!(!host.ran("lvm zap"));
    assert_eq!(progress_of("disk-no-dev").phase, Phase::Removable);
}

#[tokio::test]
async fn an_unknown_osd_map_marks_every_disk_unknown_and_changes_nothing() {
    let host = FakeHost::new().ok("ceph -s", "");
    let meta = HashMap::from([("disk-no-map".to_string(), disk_on("sdb", Ownership::Blank))]);

    reconcile_local_osds(&host, "node1", &meta, &HashMap::new(), None, true).await;

    assert_eq!(progress_of("disk-no-map").phase, Phase::Unknown);
    assert_eq!(host.calls(), vec!["ceph -s".to_string()]);
}

fn a_reachable_cluster_with_no_osds() -> FakeHost {
    ours_host()
        .ok("ceph -s", "")
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .ok("ceph osd df tree", r#"{"nodes":[]}"#)
        .fail("ceph config-key get yolab/storage-policy", "ENOENT")
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
}

#[tokio::test]
async fn a_switched_on_disk_is_blocked_waited_on_or_created_as_its_state_says() {
    let host = a_reachable_cluster_with_no_osds();
    let meta = HashMap::from([
        ("disk-plan-blocked".to_string(), disk_on("sdb", Ownership::Ours)),
        ("disk-plan-wait".to_string(), disk_on("sdc", Ownership::Blank)),
        ("disk-plan-off".to_string(), disk_on("sde", Ownership::Blank)),
    ]);
    {
        let mut p = progress_table();
        let e = p.entry("disk-plan-wait".into()).or_default();
        e.attempts = 1;
        e.last_attempt = Some(std::time::Instant::now());
    }
    let desired = HashMap::from([
        (record_key("node1", "disk-plan-blocked"), "ON".to_string()),
        (record_key("node1", "disk-plan-wait"), "ON".to_string()),
        (record_key("node1", "disk-plan-off"), "OFF".to_string()),
    ]);

    reconcile_local_osds(&host, "node1", &meta, &desired, Some(&HashMap::new()), true).await;

    assert_eq!(progress_of("disk-plan-blocked").phase, Phase::Blocked);
    assert_eq!(progress_of("disk-plan-wait").attempts, 1);
    assert_eq!(progress_of("disk-plan-off").phase, Phase::Removable);
    assert!(!host.ran("lvm create"));
}

#[tokio::test]
async fn a_switched_on_blank_disk_is_set_up_in_the_background() {
    let host = a_reachable_cluster_with_no_osds();
    let meta = HashMap::from([("disk-plan-create".to_string(), disk_on("sdk", Ownership::Blank))]);
    let desired = HashMap::from([(record_key("node1", "disk-plan-create"), "ON".to_string())]);

    reconcile_local_osds(&host, "node1", &meta, &desired, Some(&HashMap::new()), true).await;

    assert_eq!(progress_of("disk-plan-create").attempts, 1);
    for _ in 0..100 {
        if !creating().contains("disk-plan-create") {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!creating().contains("disk-plan-create"));
    assert!(host.ran("ceph-volume lvm create --bluestore --data /dev/sdk"));
}

#[tokio::test]
async fn a_switched_on_disk_with_an_osd_reports_how_it_is_doing() {
    let host = ours_host()
        .ok("ceph -s", "")
        .ok("ceph osd ls", "[8]")
        .ok("ceph-volume lvm list", "{}")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@8.service",
            "active",
        )
        .ok(
            "ceph osd df tree",
            r#"{"nodes":[{"id":8,"type":"osd","status":"down","crush_weight":0.5,"reweight":1.0,"kb":0}]}"#,
        )
        .fail("ceph config-key get yolab/storage-policy", "ENOENT")
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":8,"up":0,"in":1,"up_from":0}],"pools":[]}"#,
        );
    let meta = HashMap::from([("disk-steer".to_string(), disk_on("sdb", Ownership::Ours))]);
    let desired = HashMap::from([(record_key("node1", "disk-steer"), "ON".to_string())]);
    let map = HashMap::from([("disk-steer".to_string(), 8)]);

    reconcile_local_osds(&host, "node1", &meta, &desired, Some(&map), true).await;

    assert_eq!(progress_of("disk-steer").phase, Phase::Creating);
    assert_eq!(progress_of("disk-steer").message, FIRST_START_MESSAGE);
}

#[tokio::test]
async fn a_create_already_running_is_not_started_twice_and_a_retry_counts_its_attempt() {
    creating().insert("disk-spawn-busy".into());
    spawn_create(FakeHost::new(), "disk-spawn-busy".into(), "/dev/sdm".into());
    assert_eq!(progress_of("disk-spawn-busy").attempts, 0);
    creating().remove("disk-spawn-busy");

    progress_table()
        .entry("disk-spawn-again".into())
        .or_default()
        .attempts = 1;
    let host = FakeHost::new();
    spawn_create(host.clone(), "disk-spawn-again".into(), "/dev/sdm".into());
    assert_eq!(
        progress_of("disk-spawn-again").message,
        "Still setting this disk up… (attempt 2)"
    );
    for _ in 0..100 {
        if !creating().contains("disk-spawn-again") {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(progress_of("disk-spawn-again").phase, Phase::Retrying);
}

#[tokio::test]
async fn a_disk_whose_cluster_id_cannot_be_read_is_not_checked_or_touched() {
    let host = FakeHost::new()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .fail("ceph fsid", "error connecting to the cluster");
    create_osd(&host, "disk-no-fsid", "/dev/sdn").await;
    assert_eq!(
        progress_of("disk-no-fsid").message,
        "Could not check this disk yet. YoLab will keep trying."
    );
    assert!(!host.ran("wipefs") && !host.ran("lvm create"));
}

#[tokio::test]
async fn a_foreign_disk_that_cannot_be_erased_is_left_alone() {
    let host = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", &listed("/dev/sdo", 1, THEIRS))
        .fail("ceph-volume lvm zap", "device busy");
    create_osd(&host, "disk-zap-fails", "/dev/sdo").await;
    assert_eq!(
        progress_of("disk-zap-fails").message,
        "Could not erase this disk yet. YoLab will keep trying."
    );
    assert!(!host.ran("wipefs") && !host.ran("lvm create"));
}

#[tokio::test]
async fn a_failed_or_unstartable_wipefs_still_lets_ceph_volume_try() {
    let failed = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .fail("wipefs --all /dev/sdp", "wipefs: probing initialization failed")
        .fail("ceph-volume lvm create", "Unable to create a new OSD id");
    create_osd(&failed, "disk-wipefs-fails", "/dev/sdp").await;
    assert!(failed.ran("lvm create"));

    let unstartable = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .unstartable("wipefs")
        .fail("ceph-volume lvm create", "Unable to create a new OSD id");
    create_osd(&unstartable, "disk-wipefs-missing", "/dev/sdp").await;
    assert!(unstartable.ran("lvm create"));
    assert_eq!(progress_of("disk-wipefs-missing").phase, Phase::Retrying);
}

#[tokio::test]
async fn a_stale_signature_that_cannot_be_erased_is_not_retried() {
    let host = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .ok("wipefs --all", "")
        .fail(
            "ceph-volume lvm create",
            "RuntimeError: Device /dev/sdr has bluestore signature.",
        )
        .fail("ceph-volume lvm zap", "device busy");
    create_osd(&host, "disk-stale-zap-fails", "/dev/sdr").await;
    let creates = host.calls().iter().filter(|c| c.contains("lvm create")).count();
    assert_eq!(creates, 1);
    assert_eq!(progress_of("disk-stale-zap-fails").phase, Phase::Retrying);
}

#[tokio::test]
async fn a_create_that_cannot_be_confirmed_is_still_creating() {
    let host = ours_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph-volume lvm list", "{}")
        .fail("ceph-volume lvm list", "timed out")
        .ok("wipefs --all", "")
        .ok("ceph-volume lvm create", "");
    create_osd(&host, "disk-unconfirmed", "/dev/sds").await;
    assert_eq!(
        progress_of("disk-unconfirmed").message,
        "Added. Checking it is working…"
    );
}

#[tokio::test]
async fn a_failed_create_reclaims_the_osd_id_it_allocated() {
    let host = ours_host()
        .ok("ceph osd ls", "[1]")
        .ok("ceph osd ls", "[1, 6]")
        .ok("ceph osd ls", "[1, 6]")
        .ok("ceph osd ls", "[1]")
        .ok("ceph-volume lvm list", "{}")
        .ok("wipefs --all", "")
        .fail("ceph-volume lvm create", "Unable to create a new OSD id")
        .ok("ceph osd safe-to-destroy osd.6", r#"{"safe_to_destroy":[6]}"#)
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .ok("ceph osd purge-actual osd.6", "purged osd.6");
    create_osd(&host, "disk-leaked", "/dev/sdt").await;
    assert!(host.ran("ceph osd purge-actual osd.6 --yes-i-really-mean-it"));
    assert_eq!(progress_of("disk-leaked").orphan_osd_id, None);
    assert_eq!(progress_of("disk-leaked").phase, Phase::Retrying);
}

#[tokio::test]
async fn a_failed_create_with_no_osd_list_before_it_reclaims_nothing() {
    let host = ours_host()
        .fail("ceph osd ls", "timed out")
        .ok("ceph-volume lvm list", "{}")
        .ok("wipefs --all", "")
        .fail("ceph-volume lvm create", "Unable to create a new OSD id");
    create_osd(&host, "disk-no-before", "/dev/sdu").await;
    assert_eq!(progress_of("disk-no-before").orphan_osd_id, None);
    assert!(!host.ran("safe-to-destroy"));
}

fn orphaned(disk_id: &str, osd: i64) {
    progress_table()
        .entry(disk_id.into())
        .or_default()
        .orphan_osd_id = Some(osd);
}

#[tokio::test]
async fn an_orphan_the_cluster_no_longer_has_is_forgotten() {
    orphaned("disk-orphan-gone", 20);
    let host = FakeHost::new().ok("ceph osd ls", "[1]");
    reclaim_orphan(&host, "disk-orphan-gone").await;
    assert_eq!(progress_of("disk-orphan-gone").orphan_osd_id, None);
}

#[tokio::test]
async fn an_orphan_is_kept_when_the_osd_list_or_the_safety_check_does_not_answer() {
    orphaned("disk-orphan-unlisted", 21);
    reclaim_orphan(&FakeHost::new(), "disk-orphan-unlisted").await;
    assert_eq!(progress_of("disk-orphan-unlisted").orphan_osd_id, Some(21));

    orphaned("disk-orphan-unsafe", 22);
    let unsafe_host = FakeHost::new()
        .ok("ceph osd ls", "[22]")
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[]}"#);
    reclaim_orphan(&unsafe_host, "disk-orphan-unsafe").await;
    assert_eq!(progress_of("disk-orphan-unsafe").orphan_osd_id, Some(22));

    orphaned("disk-orphan-silent", 23);
    let silent = FakeHost::new()
        .ok("ceph osd ls", "[23]")
        .fail("ceph osd safe-to-destroy", "timed out");
    reclaim_orphan(&silent, "disk-orphan-silent").await;
    assert_eq!(progress_of("disk-orphan-silent").orphan_osd_id, Some(23));
    assert!(!silent.ran("purge"));
}

#[tokio::test]
async fn an_orphan_whose_purge_is_unconfirmed_or_refused_is_kept() {
    orphaned("disk-orphan-listed", 24);
    let listed_after = FakeHost::new()
        .ok("ceph osd ls", "[24]")
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[24]}"#)
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .ok("ceph osd purge-actual", "");
    reclaim_orphan(&listed_after, "disk-orphan-listed").await;
    assert_eq!(progress_of("disk-orphan-listed").orphan_osd_id, Some(24));

    orphaned("disk-orphan-refused", 25);
    let refused = FakeHost::new()
        .ok("ceph osd ls", "[25]")
        .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[25]}"#)
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .fail("ceph osd purge-actual", "EBUSY");
    reclaim_orphan(&refused, "disk-orphan-refused").await;
    assert_eq!(progress_of("disk-orphan-refused").orphan_osd_id, Some(25));
}

#[test]
fn an_unreadable_osd_list_is_an_error_whichever_question_is_asked() {
    assert!(parse_lvm_list("garbage", OURS).is_err());
    assert!(foreign_osd_ids("garbage", OURS).is_err());
    assert!(foreign_osd_in_list("garbage", OURS, "/dev/sdb").is_err());
}

#[test]
fn a_list_with_both_clusters_osds_names_only_the_other_ones() {
    let raw = json!({
        "1": [{"devices": ["/dev/sdb"], "tags": {"ceph.cluster_fsid": OURS}}],
        "2": [{"devices": ["/dev/sdc"], "lv_path": "/dev/ceph-x/osd-2", "tags": {"ceph.cluster_fsid": THEIRS}}],
        "3": [{"devices": ["/dev/sdd"], "tags": {}}],
    })
    .to_string();
    assert_eq!(foreign_osd_ids(&raw, OURS).unwrap(), vec![2]);
    assert_eq!(foreign_osd_in_list(&raw, OURS, "/dev/sdc").unwrap(), Some(2));
    assert_eq!(foreign_osd_in_list(&raw, OURS, "/dev/sdb").unwrap(), None);
    assert_eq!(
        parse_lvm_list(&raw, OURS).unwrap(),
        vec![("/dev/sdb".to_string(), 1), ("/dev/sdd".to_string(), 3)]
    );
    assert_eq!(foreign_osd_ids(&raw, "").unwrap(), Vec::<i64>::new());
}

#[tokio::test]
async fn another_clusters_running_osd_units_are_stopped_and_idle_ones_left_alone() {
    let raw = json!({
        "2": [{"devices": ["/dev/sdc"], "tags": {"ceph.cluster_fsid": THEIRS}}],
        "3": [{"devices": ["/dev/sdd"], "tags": {"ceph.cluster_fsid": THEIRS}}],
        "4": [{"devices": ["/dev/sde"], "tags": {"ceph.cluster_fsid": THEIRS}}],
    })
    .to_string();
    let host = ours_host()
        .ok("ceph-volume lvm list", &raw)
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@2.service",
            "active",
        )
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@3.service",
            "inactive",
        )
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@4.service",
            "",
        )
        .ok("systemctl stop", "");
    stop_foreign_osd_units(&host).await.unwrap();
    assert!(host.ran("systemctl stop yolab-ceph-osd@2.service"));
    assert!(!host.ran("systemctl stop yolab-ceph-osd@3.service"));
    assert!(!host.ran("systemctl stop yolab-ceph-osd@4.service"));
}

#[tokio::test]
async fn foreign_osd_units_cannot_be_stopped_without_the_list_the_id_or_systemctl() {
    let no_fsid = FakeHost::new()
        .ok("ceph-volume lvm list", "{}")
        .fail("ceph fsid", "error connecting to the cluster");
    assert!(stop_foreign_osd_units(&no_fsid).await.is_err());

    let garbled = ours_host().ok("ceph-volume lvm list", "garbage");
    assert!(stop_foreign_osd_units(&garbled).await.is_err());

    let no_systemctl = ours_host()
        .ok("ceph-volume lvm list", &listed("/dev/sdc", 2, THEIRS))
        .unstartable("systemctl");
    assert!(stop_foreign_osd_units(&no_systemctl).await.is_err());
}

#[tokio::test]
async fn forgotten_osds_are_found_only_from_a_readable_list_and_never_mid_create() {
    let silent = FakeHost::new().fail("ceph osd ls", "timed out");
    let map = HashMap::from([("disk-forgot-a".to_string(), 30)]);
    assert!(erase_forgotten_osds(&silent, "node1", &HashMap::new(), &HashMap::new(), &map)
        .await
        .is_empty());

    creating().insert("disk-forgot-busy".into());
    let host = FakeHost::new()
        .ok("ceph osd ls", "[]")
        .ok("systemctl stop", "")
        .fail("ceph-volume lvm zap", "device busy");
    let map = HashMap::from([
        ("disk-forgot-busy".to_string(), 31),
        ("disk-forgot-unplugged".to_string(), 32),
        ("disk-forgot-stuck".to_string(), 33),
    ]);
    let meta = HashMap::from([("disk-forgot-stuck".to_string(), disk_on("sdv", Ownership::Ours))]);
    let desired = HashMap::from([
        (record_key("node1", "disk-forgot-unplugged"), "ON".to_string()),
        (record_key("node1", "disk-forgot-stuck"), "ON".to_string()),
    ]);

    let forgotten = erase_forgotten_osds(&host, "node1", &meta, &desired, &map).await;
    creating().remove("disk-forgot-busy");

    assert_eq!(
        forgotten,
        std::collections::HashSet::from([
            "disk-forgot-unplugged".to_string(),
            "disk-forgot-stuck".to_string()
        ])
    );
    assert!(!host.ran("yolab-ceph-osd@31"));
    assert!(host.ran("ceph-volume lvm zap --destroy /dev/sdv"));
    assert_eq!(
        progress_of("disk-forgot-stuck").message,
        "Could not erase this disk yet. YoLab will keep trying."
    );
}

#[tokio::test]
async fn an_osd_on_a_virtual_disk_waits_for_its_placement_before_it_starts() {
    link("/var/lib/ceph/osd/ceph-50/block", "/dev/yolab_vdisk_9/osd");
    let host = FakeHost::new()
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@50.service",
            "inactive",
        )
        .fail("ceph config get osd.50 crush_location", "timed out");
    ensure_osd_unit_running(&host, 50).await;
    assert!(!host.ran("systemctl start"));
}

#[tokio::test]
async fn a_start_that_fails_or_cannot_run_is_only_logged() {
    let failed = FakeHost::new().fail("systemctl start", "Job failed");
    start_osd_unit(&failed, 51).await;
    assert!(failed.ran("systemctl start yolab-ceph-osd@51.service"));

    let missing = FakeHost::new().unstartable("systemctl start");
    start_osd_unit(&missing, 52).await;
    assert_eq!(
        missing.calls(),
        vec!["systemctl start yolab-ceph-osd@52.service".to_string()]
    );
}

#[tokio::test(start_paused = true)]
async fn a_unit_that_will_not_stop_is_given_fifteen_seconds() {
    let host = FakeHost::new()
        .ok("systemctl stop", "")
        .ok(
            "systemctl show -p ActiveState --value yolab-ceph-osd@53.service",
            "deactivating",
        );
    let started = tokio::time::Instant::now();
    disable_osd_unit(&host, 53).await;
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    let polls = host
        .calls()
        .iter()
        .filter(|c| c.starts_with("systemctl show"))
        .count();
    assert_eq!(polls, 15);
}

async fn a_receipt_for(osd: i64) -> destructive::Purged {
    let host = FakeHost::new()
        .ok(
            "ceph osd safe-to-destroy",
            &format!(r#"{{"safe_to_destroy":[{osd}]}}"#),
        )
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .ok("ceph osd purge-actual", "")
        .ok("ceph osd ls", "[]");
    let proof = destructive::safe_to_destroy(&host, osd)
        .await
        .unwrap()
        .unwrap();
    destructive::purge_safe(&host, proof).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_bare_device_is_wiped_under_dev_and_a_failed_wipe_is_retried_next_tick() {
    let host = FakeHost::new().fail("ceph-volume lvm zap", "device busy");
    wipe_device(&host, "sdw", a_receipt_for(60).await).await;
    assert!(host.ran("ceph-volume lvm zap --destroy /dev/sdw"));
}

fn host_tree(node: &str, children: &[i64], osds: Value) -> Vec<Value> {
    let mut nodes = vec![json!({"type": "host", "name": node, "children": children})];
    nodes.extend(osds.as_array().unwrap().iter().cloned());
    nodes
}

#[tokio::test]
async fn only_this_nodes_unplugged_down_and_out_osds_are_purged() {
    let tree = host_tree(
        "node1",
        &[70, 71, 72, 73],
        json!([
            {"type": "osd", "status": "down", "reweight": 0.0},
            {"id": 70, "type": "osd", "status": "down", "reweight": 0.0},
            {"id": 71, "type": "osd", "status": "down", "reweight": 1.0},
            {"id": 72, "type": "osd", "status": "up", "reweight": 0.0},
            {"id": 73, "type": "osd", "status": "down", "reweight": 0.0},
            {"id": 79, "type": "osd", "status": "down", "reweight": 0.0},
        ]),
    );
    let host = FakeHost::new()
        .ok("systemctl stop", "")
        .ok("systemctl show", "inactive")
        .ok("ceph osd safe-to-destroy osd.73", r#"{"safe_to_destroy":[73]}"#)
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .ok("ceph osd purge-actual osd.73", "")
        .ok("ceph osd ls", "[]");
    let active = HashMap::from([("disk-active".to_string(), 70)]);

    purge_drained_osds(&host, "node1", &tree, &active, false).await;

    assert!(host.ran("ceph osd purge-actual osd.73"));
    for spared in [70, 71, 72, 79] {
        assert!(!host.ran(&format!("osd.{spared}")), "osd.{spared}: {:?}", host.calls());
    }

    let waiting = FakeHost::new();
    purge_drained_osds(&waiting, "node1", &tree, &active, true).await;
    assert_eq!(waiting.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_purge_of_an_unplugged_osd_waits_for_a_clear_yes_and_a_confirmed_removal() {
    let tree = host_tree(
        "node1",
        &[80, 81, 82, 83],
        json!([
            {"id": 80, "type": "osd", "status": "down", "reweight": 0.0},
            {"id": 81, "type": "osd", "status": "down", "reweight": 0.0},
            {"id": 82, "type": "osd", "status": "down", "reweight": 0.0},
            {"id": 83, "type": "osd", "status": "down", "reweight": 0.0},
        ]),
    );
    let host = FakeHost::new()
        .ok("systemctl stop", "")
        .ok("systemctl show", "inactive")
        .ok("ceph osd safe-to-destroy osd.80", r#"{"safe_to_destroy":[]}"#)
        .fail("ceph osd safe-to-destroy osd.81", "timed out")
        .ok("ceph osd safe-to-destroy osd.82", r#"{"safe_to_destroy":[82]}"#)
        .ok("ceph osd safe-to-destroy osd.83", r#"{"safe_to_destroy":[83]}"#)
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .ok("ceph osd purge-actual osd.82", "")
        .fail("ceph osd purge-actual osd.83", "EBUSY")
        .ok("ceph osd ls", "[82]");

    purge_drained_osds(&host, "node1", &tree, &HashMap::new(), false).await;

    assert!(!host.ran("purge-actual osd.80") && !host.ran("purge-actual osd.81"));
    assert!(host.ran("purge-actual osd.82") && host.ran("purge-actual osd.83"));
}

#[tokio::test]
async fn a_disk_record_the_cluster_refuses_is_still_counted_as_registered() {
    let host = FakeHost::new().fail("ceph config-key set", "EACCES");
    let meta = HashMap::from([("dev-sdy".to_string(), disk_on("sdy", Ownership::Blank))]);
    assert_eq!(
        auto_register_all_disks(&host, "node1", &meta, &HashMap::new()).await,
        1
    );
    assert!(host.ran("ceph config-key set yolab/disks/node1--dev-sdy OFF"));
}

#[test]
fn an_unreadable_store_entry_never_stops_the_other_disks_being_synced() {
    let mut store = crate::store::Store::new("node1");
    store.corrupt_disk_claim("node1--dev-bad");
    let meta = HashMap::from([
        ("dev-bad".to_string(), disk_on("sdb", Ownership::Blank)),
        (SYSTEM_OSD_ID.to_string(), disk_on("/dev/mapper/pool-ceph", Ownership::Ours)),
    ]);
    let mirror = HashMap::from([
        ("node1--dev-bad".to_string(), "ON".to_string()),
        ("node1--dev-good".to_string(), "ON".to_string()),
    ]);

    let synced = sync_store(&mut store, "node1", &meta, Some(&mirror));

    assert_eq!(synced.claims, None);
    assert!(synced.seeded);
    assert!(synced.changed);
}

#[tokio::test]
async fn a_store_that_cannot_be_saved_still_answers_from_what_it_holds() {
    let path = crate::store::default_path();
    std::fs::create_dir_all(&path).unwrap();
    let host = FakeHost::new().ok(
        "ceph config-key dump yolab/disks/",
        r#"{"yolab/disks/node1--dev-save":"ON"}"#,
    );
    let meta = HashMap::from([("dev-save".to_string(), disk_on("sdb", Ownership::Blank))]);

    let desired = read_desired(&host, "node1", &meta).await.unwrap();

    assert_eq!(desired.get("node1--dev-save").map(String::as_str), Some("ON"));
    assert!(path.is_dir());
}

#[test]
fn an_lv_disk_without_a_mapper_link_has_no_size() {
    assert_eq!(lv_size_bytes("/dev/mapper/not-here"), 0);
    put("/dev/mapper/plain-file", b"");
    assert_eq!(lv_size_bytes("/dev/mapper/plain-file"), 0);
}

#[tokio::test]
async fn an_lv_attempt_on_a_bare_lv_name_refuses_a_disk_whose_label_cannot_be_read() {
    let dev = "/dev/lv-unknown";
    put(dev, &[1u8; 16]);
    let host = ours_host().ok("ceph-volume lvm list", "{}");
    let why = not_yet(lv_osd_attempt(&host, dev).await.unwrap());
    assert!(why.starts_with("/dev/lv-unknown: "), "{why}");
}
