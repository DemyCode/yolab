use super::*;
use crate::host::fake::FakeHost;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rec(id: i64, node: &str, provisioned: bool) -> VdiskRecord {
    let s = |v: &str| {
        if provisioned {
            v.to_string()
        } else {
            String::new()
        }
    };
    VdiskRecord {
        id,
        node: node.into(),
        box_type: "bx11".into(),
        size_gb: 1,
        host: s("u1.example"),
        username: s("u1"),
        password: s("pw"),
        osd_id: None,
    }
}

fn records(recs: &[VdiskRecord]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = recs
        .iter()
        .map(|r| {
            (
                format!("yolab/vdisks/{}", r.id),
                serde_json::Value::String(serde_json::to_string(r).unwrap()),
            )
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn tunnel(url: &str) -> Tunnel {
    Tunnel {
        enabled: true,
        platform_api_url: url.into(),
        account_token: "tok".into(),
        tunnel_id: "t".into(),
        host: String::new(),
    }
}

fn switches(pairs: &[(&str, &str)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .iter()
        .map(|(k, v)| (format!("yolab/disks/{k}"), serde_json::Value::from(*v)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn link_osd(root: &Path, osd: i64, target: &str) {
    let dir = root.join(format!("var/lib/ceph/osd/ceph-{osd}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(target, dir.join("block")).unwrap();
}

fn touch(path: PathBuf) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"").unwrap();
}

use std::path::PathBuf;

#[test]
fn a_block_target_without_an_lv_part_is_no_virtual_disk() {
    assert_eq!(vdisk_of_block_target("/dev/yolab_vdisk_4"), None);
}

#[test]
fn the_osd_folder_is_read_only_for_ceph_named_entries() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(osd_on(root.path(), 4), None);
    std::fs::create_dir_all(root.path().join("var/lib/ceph/osd/notes")).unwrap();
    use std::os::unix::ffi::OsStrExt as _;
    std::fs::create_dir_all(
        root.path()
            .join("var/lib/ceph/osd")
            .join(std::ffi::OsStr::from_bytes(b"ceph-\xff")),
    )
    .unwrap();
    link_osd(root.path(), 6, "/dev/yolab_vdisk_4/osd");
    assert_eq!(osd_on(root.path(), 5), None);
    assert_eq!(osd_on(root.path(), 4), Some(6));
}

fn placed(entry: &str) -> FakeHost {
    FakeHost::new().ok(
        "ceph osd dump",
        &format!(r#"{{"osds":[{entry}],"pools":[]}}"#),
    )
}

#[tokio::test]
async fn a_virtual_disk_osd_is_moved_under_its_machine_kept_off_reads_and_kept_in() {
    let host = placed(r#"{"osd":5,"up":1,"in":1,"primary_affinity":1.0,"state":["exists","up"]}"#)
        .ok("ceph config get osd.5 crush_location", "host=vdisk-1")
        .ok("ceph config rm", "")
        .ok("ceph osd primary-affinity", "")
        .ok("ceph osd add-noout", "");
    ensure_placement(&host, 5).await.unwrap();
    assert!(host.ran("ceph config rm osd.5 crush_location"));
    assert!(host.ran("ceph osd primary-affinity osd.5 0"));
    assert!(host.ran("ceph osd add-noout osd.5"));
}

#[tokio::test]
async fn a_placement_step_the_cluster_refuses_stops_the_placement() {
    let entry = r#"{"osd":5,"up":1,"in":1,"primary_affinity":1.0,"state":["exists","up"]}"#;
    let rm = placed(entry)
        .ok("ceph config get", "host=vdisk-1")
        .fail("ceph config rm", "EPERM");
    assert_eq!(
        ensure_placement(&rm, 5).await.unwrap_err().to_string(),
        "keep the OSD under the machine it is attached to"
    );

    let affinity = placed(entry)
        .fail("ceph config get", "Error ENOENT: no such option")
        .fail("ceph osd primary-affinity", "EPERM");
    assert_eq!(
        ensure_placement(&affinity, 5)
            .await
            .unwrap_err()
            .to_string(),
        "keep reads off the virtual disk"
    );

    let noout = placed(r#"{"osd":5,"up":1,"in":1,"primary_affinity":0.0,"state":["exists"]}"#)
        .fail("ceph config get", "Error ENOENT: no such option")
        .fail("ceph osd add-noout", "EPERM");
    assert_eq!(
        ensure_placement(&noout, 5).await.unwrap_err().to_string(),
        "stop an internet outage from moving data off the virtual disk"
    );
}

#[tokio::test]
async fn a_placement_needs_the_crush_location_and_the_osd_in_the_map() {
    let silent = FakeHost::new().fail("ceph config get", "timed out");
    assert_eq!(
        ensure_placement(&silent, 5).await.unwrap_err().to_string(),
        "read the OSD's CRUSH location"
    );

    let no_map = FakeHost::new()
        .ok("ceph config get", "")
        .fail("ceph osd dump", "timed out");
    assert_eq!(
        ensure_placement(&no_map, 5).await.unwrap_err().to_string(),
        "read the OSD map"
    );

    let missing = placed(r#"{"osd":9,"up":1,"in":1}"#).ok("ceph config get", "");
    assert_eq!(
        ensure_placement(&missing, 5).await.unwrap_err().to_string(),
        "osd.5 is not in the OSD map"
    );
}

#[tokio::test]
async fn a_placed_virtual_disk_osd_may_start() {
    let root = tempfile::tempdir().unwrap();
    link_osd(root.path(), 5, "/dev/yolab_vdisk_4/osd");
    let host = placed(r#"{"osd":5,"up":0,"in":1,"primary_affinity":0.0,"state":["noout"]}"#)
        .ok("ceph config get", "");
    assert!(placement_allows_start(&host, root.path(), 5).await);
}

#[tokio::test]
async fn detaching_everything_names_the_disks_that_stayed() {
    let root = tempfile::tempdir().unwrap();
    for id in [1, 2] {
        std::fs::create_dir_all(stack::run_dir(root.path(), id)).unwrap();
    }
    std::fs::write(stack::run_dir(root.path(), 2).join("loop"), "/dev/loop2").unwrap();
    std::fs::create_dir_all(root.path().join("sys/block/loop2/loop")).unwrap();
    let host = FakeHost::new().fail("losetup --detach", "busy");
    assert_eq!(
        detach_all(&host, root.path())
            .await
            .unwrap_err()
            .to_string(),
        "virtual disks [2] are still attached"
    );

    let fine = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(stack::run_dir(fine.path(), 1)).unwrap();
    detach_all(&FakeHost::new(), fine.path()).await.unwrap();

    let blocked = tempfile::tempdir().unwrap();
    touch(blocked.path().join("run/yolab/vdisks"));
    assert!(detach_all(&FakeHost::new(), blocked.path()).await.is_err());
}

fn controller(host: FakeHost, platform: &str) -> (tempfile::TempDir, VdisksController<FakeHost>) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        format!("[tunnel]\nplatform_api_url = \"{platform}\"\naccount_token = \"tok\"\n"),
    )
    .unwrap();
    let config = crate::config::Config::for_test(&file);
    (
        dir,
        VdisksController {
            config,
            host,
            http: crate::http::Client::new(),
        },
    )
}

fn ctx(node: &str) -> Ctx {
    Ctx {
        node: node.into(),
        tools: crate::tools::Tools::unreachable(),
    }
}

#[tokio::test]
async fn the_virtual_disks_controller_runs_on_every_node_every_half_minute_after_ceph() {
    let (_dir, c) = controller(FakeHost::new(), "http://[::1]:9");
    assert_eq!(c.name(), NAME);
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(30));
    assert_eq!(c.depends_on(), &[crate::runtime::CEPH]);
}

#[tokio::test]
async fn unreadable_records_are_an_error_and_no_records_is_idle() {
    let (_dir, down) = controller(
        FakeHost::new().fail("ceph config-key dump yolab/vdisks/", "timed out"),
        "http://[::1]:9",
    );
    assert_eq!(
        down.converge(&ctx("node1")).await.unwrap_err().to_string(),
        "read the virtual disk records"
    );

    let (_dir, none) = controller(
        FakeHost::new().ok(
            "ceph config-key dump yolab/vdisks/",
            &records(&[rec(1, "node2", true)]),
        ),
        "http://[::1]:9",
    );
    assert_eq!(
        none.converge(&ctx("node1")).await.unwrap(),
        Tick::Idle("no virtual disks on this machine".into())
    );
}

#[tokio::test]
async fn a_disk_attached_here_without_a_record_is_detached() {
    std::fs::create_dir_all(stack::run_dir(crate::storage::root(), 8)).unwrap();
    std::fs::create_dir_all(stack::run_dir(crate::storage::root(), 9)).unwrap();
    std::fs::write(
        stack::run_dir(crate::storage::root(), 9).join("loop"),
        "/dev/loop9",
    )
    .unwrap();
    std::fs::create_dir_all(crate::storage::on_machine("/sys/block/loop9/loop")).unwrap();
    let host = FakeHost::new()
        .ok("ceph config-key dump yolab/vdisks/", "{}")
        .fail("losetup --detach", "busy");
    let (_dir, c) = controller(host.clone(), "http://[::1]:9");
    c.converge(&ctx("node1")).await.unwrap();
    assert!(host.ran("losetup --detach /dev/loop9"));
}

#[tokio::test]
async fn a_disk_list_folder_that_cannot_be_read_is_an_error() {
    touch(crate::storage::on_machine("/run/yolab/vdisks"));
    let (_dir, c) = controller(
        FakeHost::new().ok("ceph config-key dump yolab/vdisks/", "{}"),
        "http://[::1]:9",
    );
    assert!(c.converge(&ctx("node1")).await.is_err());
}

#[tokio::test]
async fn a_machine_without_its_config_cannot_reach_the_platform_for_its_disks() {
    let host = FakeHost::new().ok(
        "ceph config-key dump yolab/vdisks/",
        &records(&[rec(1, "node1", true)]),
    );
    let (dir, c) = controller(host, "http://[::1]:9");
    std::fs::remove_file(dir.path().join("config.toml")).unwrap();
    assert!(c.converge(&ctx("node1")).await.is_err());
}

#[tokio::test]
async fn each_disks_status_is_published_only_when_it_changed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/volume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 1, "box_type": "bx11", "size_gb": 1, "status": "initializing" },
            { "id": 2, "box_type": "bx11", "size_gb": 1, "status": "initializing" },
        ])))
        .mount(&server)
        .await;
    let host = FakeHost::new()
        .ok(
            "ceph config-key dump yolab/vdisks/",
            &records(&[
                rec(1, "node1", false),
                rec(2, "node1", false),
                rec(3, "node1", false),
                rec(4, "node1", false),
            ]),
        )
        .ok(
            "ceph config-key dump yolab/disks/",
            &switches(&[("node1--vdisk-4", "OFF")]),
        )
        .ok(
            "ceph config-key dump yolab/vdisk-status/",
            &serde_json::json!({ "yolab/vdisk-status/2": PREPARING }).to_string(),
        )
        .ok("ceph config-key set", "")
        .ok("ceph config-key rm", "")
        .ok("ceph osd crush rm", "");
    Mock::given(method("DELETE"))
        .and(path("/storage/volume/4"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let (_dir, c) = controller(host.clone(), &server.uri());

    assert_eq!(c.converge(&ctx("node1")).await.unwrap(), Tick::Done);

    assert!(host.ran(&format!(
        "ceph config-key set yolab/vdisk-status/1 {PREPARING}"
    )));
    assert!(!host.ran("ceph config-key set yolab/vdisk-status/2"));
    assert!(host.ran(
        "ceph config-key set yolab/vdisk-status/3 Could not connect yet, retrying: the platform has no Storage Box for this virtual disk"
    ));
    assert!(!host.ran("ceph config-key set yolab/vdisk-status/4"));
    assert!(host.ran("ceph config-key rm yolab/vdisks/4"));
}

fn client() -> crate::http::Client {
    crate::http::Client::new()
}

#[tokio::test]
async fn a_disk_whose_switches_cannot_be_read_does_nothing() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new().fail("ceph config-key dump yolab/disks/", "timed out");
    let err = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "read the disk switches");
}

#[tokio::test]
async fn an_unreadable_store_falls_back_to_the_cluster_switch() {
    crate::store::locked().corrupt_disk_claim("node0--dev-x");
    let host = FakeHost::new().ok(
        "ceph config-key dump yolab/disks/",
        &switches(&[("node1--vdisk-1", "OFF")]),
    );
    assert!(switched_off(&host, &rec(1, "node1", true)).await.unwrap());
}

#[tokio::test]
async fn a_box_that_became_ready_is_recorded_with_its_login() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/volume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 1, "host": "u1.example", "username": "u1", "password": "pw",
              "box_type": "bx11", "size_gb": 1, "status": "active" },
        ])))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", "{}")
        .ok("ceph config-key set", "")
        .unstartable("mountpoint");
    let err = step(
        &host,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        rec(1, "node1", false),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "run mountpoint");
    assert!(host.ran(&format!(
        "ceph config-key set yolab/vdisks/1 {}",
        serde_json::to_string(&rec(1, "node1", true)).unwrap()
    )));

    let unsaved = FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", "{}")
        .fail("ceph config-key set", "EACCES");
    assert!(step(
        &unsaved,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        rec(1, "node1", false)
    )
    .await
    .is_err());
    assert!(!unsaved.ran("mountpoint"));
}

#[tokio::test]
async fn an_unprovisioned_disk_waits_on_a_platform_that_cannot_list_its_boxes() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new().ok("ceph config-key dump yolab/disks/", "{}");
    let err = step(
        &host,
        root.path(),
        &client(),
        &tunnel(&format!("http://[::1]:{}", crate::testkit::closed_port())),
        rec(1, "node1", false),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "reach the platform to list virtual disks");
}

fn switched_off_host() -> FakeHost {
    FakeHost::new().ok(
        "ceph config-key dump yolab/disks/",
        &switches(&[("node1--vdisk-1", "OFF")]),
    )
}

#[tokio::test]
async fn a_switched_off_disk_whose_osd_the_cluster_still_has_keeps_draining() {
    let root = tempfile::tempdir().unwrap();
    let host = switched_off_host()
        .ok("ceph osd ls", "[3]")
        .ok(
            "ceph osd tree",
            r#"{"nodes":[{"name":"vdisk-1","children":[3]}]}"#,
        )
        .fail("mountpoint", "");
    touch(root.path().join(record::osd_dev(1).trim_start_matches('/')));
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert_eq!(status, DRAINING);
    assert!(!host.ran("ceph osd crush rm"));
}

#[tokio::test]
async fn a_switched_off_disk_needs_the_osd_list_and_the_crush_tree() {
    let root = tempfile::tempdir().unwrap();
    let no_ids = switched_off_host().fail("ceph osd ls", "timed out");
    assert_eq!(
        step(
            &no_ids,
            root.path(),
            &client(),
            &tunnel("http://[::1]:9"),
            rec(1, "node1", true)
        )
        .await
        .unwrap_err()
        .to_string(),
        "list the cluster's OSDs"
    );
    let no_tree = switched_off_host()
        .ok("ceph osd ls", "[]")
        .fail("ceph osd tree", "timed out");
    assert_eq!(
        step(
            &no_tree,
            root.path(),
            &client(),
            &tunnel("http://[::1]:9"),
            rec(1, "node1", true)
        )
        .await
        .unwrap_err()
        .to_string(),
        "read the CRUSH tree"
    );
}

#[tokio::test]
async fn a_switched_off_disk_the_cluster_no_longer_uses_is_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/storage/volume/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(stack::run_dir(root.path(), 1)).unwrap();
    let host = switched_off_host()
        .ok("ceph osd ls", "[]")
        .ok("ceph osd tree", r#"{"nodes":[]}"#)
        .fail("ceph osd crush rm", "Error ENOENT: no such bucket")
        .ok("ceph config-key rm", "");
    let mut held = rec(1, "node1", true);
    held.osd_id = Some(7);
    let status = step(&host, root.path(), &client(), &tunnel(&server.uri()), held)
        .await
        .unwrap();
    assert_eq!(status, "");
    assert!(host.ran("ceph config-key rm yolab/vdisks/1"));
    assert!(!stack::run_dir(root.path(), 1).exists());
}

#[tokio::test]
async fn a_box_that_stopped_answering_is_detached_and_reported_offline() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(stack::run_dir(root.path(), 1).join("mnt")).unwrap();
    let host = FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", "{}")
        .ok("mountpoint", "")
        .fail("stat", "Host is down")
        .ok("umount", "");
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert_eq!(status, OFFLINE);
    assert!(host.ran("umount --force --lazy"));

    let stuck = FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", "{}")
        .ok("mountpoint", "")
        .fail("stat", "Host is down")
        .fail("umount", "busy");
    assert!(step(
        &stuck,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn a_disk_not_attached_yet_is_attached() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", "{}")
        .fail("mountpoint", "")
        .fail("ceph config-key get yolab/vdisk-formatted/1", "timed out");
    let err = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap_err();
    assert!(host.ran("ceph config-key get yolab/vdisk-formatted/1"));
    assert!(!host.ran("mount.cifs"), "{err}");
}

fn attached_with(root: &Path, switches: &str) -> FakeHost {
    touch(root.join(record::osd_dev(1).trim_start_matches('/')));
    FakeHost::new()
        .ok("ceph config-key dump yolab/disks/", switches)
        .fail("mountpoint", "")
}

fn connected(root: &Path) -> FakeHost {
    attached_with(root, "{}")
}

#[tokio::test]
async fn an_attached_disks_osd_is_placed_and_remembered() {
    let root = tempfile::tempdir().unwrap();
    link_osd(root.path(), 4, &record::osd_dev(1));
    let host = connected(root.path())
        .ok("ceph config get", "")
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":4,"up":1,"in":1,"primary_affinity":0.0,"state":["noout"]}],"pools":[]}"#,
        )
        .ok("ceph config-key set", "")
        .ok(
            "ceph osd tree",
            r#"{"nodes":[{"name":"vdisk-1","children":[]}]}"#,
        )
        .ok("ceph osd crush rm", "");
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert_eq!(status, CONNECTED);
    let mut remembered = rec(1, "node1", true);
    remembered.osd_id = Some(4);
    assert!(host.ran(&format!(
        "ceph config-key set yolab/vdisks/1 {}",
        serde_json::to_string(&remembered).unwrap()
    )));
    assert!(host.ran("ceph osd crush rm vdisk-1"));
}

#[tokio::test]
async fn an_osd_already_remembered_is_not_saved_again() {
    let root = tempfile::tempdir().unwrap();
    link_osd(root.path(), 4, &record::osd_dev(1));
    let host = connected(root.path())
        .ok("ceph config get", "")
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":4,"up":1,"in":1,"primary_affinity":0.0,"state":["noout"]}],"pools":[]}"#,
        )
        .ok("ceph osd tree", r#"{"nodes":[]}"#);
    let mut known = rec(1, "node1", true);
    known.osd_id = Some(4);
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        known,
    )
    .await
    .unwrap();
    assert_eq!(status, CONNECTED);
    assert!(!host.ran("ceph config-key set"));
}

#[tokio::test]
async fn an_attached_disk_stops_at_a_placement_a_save_or_a_crush_step_that_fails() {
    let dump =
        r#"{"osds":[{"osd":4,"up":1,"in":1,"primary_affinity":0.0,"state":["noout"]}],"pools":[]}"#;
    let root = tempfile::tempdir().unwrap();
    link_osd(root.path(), 4, &record::osd_dev(1));

    let unplaced = connected(root.path()).fail("ceph config get", "timed out");
    assert!(step(
        &unplaced,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true)
    )
    .await
    .is_err());

    let unsaved = connected(root.path())
        .ok("ceph config get", "")
        .ok("ceph osd dump", dump)
        .fail("ceph config-key set", "EACCES");
    assert!(step(
        &unsaved,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true)
    )
    .await
    .is_err());

    let mut known = rec(1, "node1", true);
    known.osd_id = Some(4);
    let no_tree = connected(root.path())
        .ok("ceph config get", "")
        .ok("ceph osd dump", dump)
        .fail("ceph osd tree", "timed out");
    assert_eq!(
        step(
            &no_tree,
            root.path(),
            &client(),
            &tunnel("http://[::1]:9"),
            known.clone()
        )
        .await
        .unwrap_err()
        .to_string(),
        "read the CRUSH tree"
    );

    let kept = connected(root.path())
        .ok("ceph config get", "")
        .ok("ceph osd dump", dump)
        .ok(
            "ceph osd tree",
            r#"{"nodes":[{"name":"vdisk-1","children":[]}]}"#,
        )
        .fail("ceph osd crush rm", "EBUSY");
    assert_eq!(
        step(
            &kept,
            root.path(),
            &client(),
            &tunnel("http://[::1]:9"),
            known
        )
        .await
        .unwrap_err()
        .to_string(),
        "remove the empty CRUSH host vdisk-1"
    );
}

#[tokio::test]
async fn a_switched_off_disk_still_holding_its_osd_is_placed_and_drained() {
    let root = tempfile::tempdir().unwrap();
    link_osd(root.path(), 4, &record::osd_dev(1));
    let host = attached_with(root.path(), &switches(&[("node1--vdisk-1", "OFF")]))
        .ok("ceph osd ls", "[4]")
        .ok("ceph osd tree", r#"{"nodes":[]}"#)
        .ok("ceph config get", "")
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":4,"up":1,"in":1,"primary_affinity":0.0,"state":["noout"]}],"pools":[]}"#,
        )
        .ok("ceph config-key set", "");
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert_eq!(status, DRAINING);
}

async fn platform_deletes(code: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/storage/volume/1"))
        .respond_with(ResponseTemplate::new(code))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_removal_stops_at_a_detach_or_a_platform_that_refuses() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(stack::run_dir(root.path(), 1)).unwrap();
    std::fs::create_dir_all(stack::run_dir(root.path(), 1).join("loop")).unwrap();
    let server = platform_deletes(200).await;
    assert!(finish_removal(
        &FakeHost::new(),
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        &rec(1, "node1", true)
    )
    .await
    .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());

    let root = tempfile::tempdir().unwrap();
    let refused = platform_deletes(503).await;
    let host = FakeHost::new();
    assert!(finish_removal(
        &host,
        root.path(),
        &client(),
        &tunnel(&refused.uri()),
        &rec(1, "node1", true)
    )
    .await
    .is_err());
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_removal_keeps_going_past_a_crush_host_that_will_not_go() {
    let root = tempfile::tempdir().unwrap();
    let server = platform_deletes(200).await;
    let host = FakeHost::new()
        .fail("ceph osd crush rm", "EBUSY")
        .ok("ceph config-key rm", "");
    finish_removal(
        &host,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        &rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert!(host.ran("ceph config-key rm yolab/vdisks/1"));
}

#[tokio::test]
async fn a_removal_stops_when_a_record_or_the_run_folder_cannot_be_removed() {
    let server = platform_deletes(200).await;

    let root = tempfile::tempdir().unwrap();
    let switch = FakeHost::new()
        .ok("ceph osd crush rm", "")
        .fail("ceph config-key rm yolab/disks/", "EACCES");
    let err = finish_removal(
        &switch,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        &rec(1, "node1", true),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("the disk record could not be removed: "),
        "{err}"
    );

    let record = FakeHost::new()
        .ok("ceph osd crush rm", "")
        .ok("ceph config-key rm", "")
        .fail("ceph config-key rm yolab/vdisk-keys/1", "EACCES");
    assert!(finish_removal(
        &record,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        &rec(1, "node1", true)
    )
    .await
    .is_err());

    use std::os::unix::fs::PermissionsExt as _;
    let folder = tempfile::tempdir().unwrap();
    let locked = stack::run_dir(folder.path(), 1).join("locked");
    touch(locked.join("file"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let host = FakeHost::new()
        .ok("ceph osd crush rm", "")
        .ok("ceph config-key rm", "");
    let err = finish_removal(
        &host,
        folder.path(),
        &client(),
        &tunnel(&server.uri()),
        &rec(1, "node1", true),
    )
    .await
    .unwrap_err();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(err.to_string(), "remove the virtual disk's run directory");
    assert!(host.ran("ceph config-key rm yolab/vdisks/1"));
}

#[tokio::test]
async fn a_disk_attached_here_with_its_record_is_kept_attached() {
    std::fs::create_dir_all(stack::run_dir(crate::storage::root(), 1)).unwrap();
    std::fs::write(
        stack::run_dir(crate::storage::root(), 1).join("loop"),
        "/dev/loop1",
    )
    .unwrap();
    std::fs::create_dir_all(crate::storage::on_machine("/sys/block/loop1/loop")).unwrap();
    let host = FakeHost::new()
        .ok(
            "ceph config-key dump yolab/vdisks/",
            &records(&[rec(1, "node1", false)]),
        )
        .ok("ceph config-key dump yolab/disks/", "{}")
        .ok("ceph config-key dump yolab/vdisk-status/", "{}")
        .ok("ceph config-key set", "");
    let (_dir, c) = controller(
        host.clone(),
        &format!("http://[::1]:{}", crate::testkit::closed_port()),
    );
    c.converge(&ctx("node1")).await.unwrap();
    assert!(!host.ran("losetup"));
    assert!(host.ran("ceph config-key set yolab/vdisk-status/1 Could not connect yet"));
}

#[tokio::test]
async fn a_switched_off_disk_the_platform_will_not_delete_stays_recorded() {
    let server = platform_deletes(503).await;
    let root = tempfile::tempdir().unwrap();
    let host = switched_off_host();
    assert!(step(
        &host,
        root.path(),
        &client(),
        &tunnel(&server.uri()),
        rec(1, "node1", false)
    )
    .await
    .is_err());
    assert!(!host.ran("ceph config-key rm"));
}

#[tokio::test]
async fn a_disk_that_attaches_wakes_the_disks_controller_and_reads_connected() {
    let root = tempfile::tempdir().unwrap();
    let host = stack::machine_tests::attaches(root.path(), false, "", "")
        .ok("ceph config-key dump yolab/disks/", "{}")
        .ok("ceph osd tree", r#"{"nodes":[]}"#);
    let status = step(
        &host,
        root.path(),
        &client(),
        &tunnel("http://[::1]:9"),
        rec(1, "node1", true),
    )
    .await
    .unwrap();
    assert_eq!(status, CONNECTED);
    assert!(host.ran("vgchange -ay"));
}
