use super::*;
use crate::host::fake::FakeHost;
use crate::k8s::testing::{api_server, serve, status};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn key(kind: u8, salt: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([kind, 0, salt, 7, 7, 7, 7, 7, 7, 7, 7, 7])
}

fn aes(salt: u8) -> String {
    key(1, salt)
}

fn aes256k(salt: u8) -> String {
    key(2, salt)
}

fn entry(k: &str, pending: Option<&str>) -> String {
    let mut e = json!({"entity": "x", "key": k, "caps": {}});
    if let Some(p) = pending {
        e["pending_key"] = json!(p);
    }
    json!([e]).to_string()
}

const LEASE: &str = "/apis/coordination.k8s.io/v1/namespaces/rook-ceph/leases/yolab-ceph-key-restart";
const LEASES: &str = "/apis/coordination.k8s.io/v1/namespaces/rook-ceph/leases";

async fn lease_free(server: &MockServer) {
    serve(server, LEASE, 404, status(404, "NotFound")).await;
    Mock::given(method("POST"))
        .and(path(LEASES))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": {"name": RESTART_LEASE, "namespace": NS},
        })))
        .mount(server)
        .await;
}

async fn lease_held_by(server: &MockServer, holder: &str) {
    serve(
        server,
        LEASE,
        200,
        json!({
            "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": {"name": RESTART_LEASE, "namespace": NS, "resourceVersion": "5"},
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": RESTART_LEASE_SECS,
                "renewTime": Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
            },
        }),
    )
    .await;
}

fn pgs_active() -> &'static str {
    r#"{"pg_summary":{"num_pg_by_state":[{"name":"active+clean","num":81}],"num_pgs":81}}"#
}

fn daemon_keyring(root: &Path, daemon: &str, node: &str, entity: &str, k: &str) {
    let dir = root.join(format!("var/lib/ceph/{daemon}/ceph-{node}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keyring"), format!("[{entity}]\n\tkey = {k}\n")).unwrap();
}

fn base_host() -> FakeHost {
    FakeHost::new()
        .ok("ceph config-key set", "")
        .ok("chown", "")
        .fail("ceph auth get client.bootstrap-osd", "Error ENOENT: no such entity")
}

#[test]
fn the_key_type_is_the_first_byte_of_the_decoded_key() {
    assert_eq!(key_type(&aes(1)), KeyType::Aes);
    assert_eq!(key_type(&aes256k(1)), KeyType::Aes256k);
    assert_eq!(key_type("not base64!"), KeyType::Unknown);
    assert_eq!(key_type(""), KeyType::Unknown);
}

#[test]
fn only_kernels_from_7_0_authenticate_with_aes256k_keys() {
    assert!(!kernel_speaks_aes256k("6.18.55"));
    assert!(kernel_speaks_aes256k("7.2.9"));
    assert!(kernel_speaks_aes256k("7.0.0-rc1"));
    assert!(kernel_speaks_aes256k("10.1.0"));
    assert!(!kernel_speaks_aes256k(""));
}

#[test]
fn a_keyring_edit_touches_only_the_named_entity() {
    let mon_keyring = "[mon.]\n\tkey = OLD\n\tcaps mon = \"allow *\"\n[client.admin]\n\tkey = ADMIN\n";
    let edited = keyring_with_key(mon_keyring, "mon.", "NEW");
    assert_eq!(keyring_key(&edited, "mon."), Some("NEW".into()));
    assert_eq!(keyring_key(&edited, "client.admin"), Some("ADMIN".into()));
    assert!(edited.contains("caps mon = \"allow *\""));
    assert_eq!(edited.matches("key = ").count(), 2);
}

#[test]
fn a_keyring_without_the_entity_gains_it() {
    let edited = keyring_with_key("", "client.yolab-images", "K");
    assert_eq!(keyring_key(&edited, "client.yolab-images"), Some("K".into()));
    let keyless = keyring_with_key("[mgr.n1]\n\tcaps mon = \"x\"\n[other]\n", "mgr.n1", "K");
    assert_eq!(keyring_key(&keyless, "mgr.n1"), Some("K".into()));
    assert_eq!(keyring_key(&keyless, "other"), None);
}

#[test]
fn a_pending_key_is_what_the_daemon_must_adopt() {
    let parsed = parse_auth_entry(&serde_json::from_str(&entry("A", Some("P"))).unwrap()).unwrap();
    assert_eq!(parsed.target(), "P");
    let settled = parse_auth_entry(&serde_json::from_str(&entry("A", None)).unwrap()).unwrap();
    assert_eq!(settled.target(), "A");
}

#[test]
fn every_placement_group_must_be_active_before_a_restart() {
    let stat = |states: Value, total: u64| json!({"pg_summary": {"num_pg_by_state": states, "num_pgs": total}});
    assert!(all_pgs_active(&stat(json!([{"name": "active+clean", "num": 3}]), 3)));
    assert!(all_pgs_active(&stat(
        json!([{"name": "active+clean", "num": 2}, {"name": "active+undersized+degraded", "num": 1}]),
        3
    )));
    assert!(!all_pgs_active(&stat(
        json!([{"name": "active+clean", "num": 2}, {"name": "peering", "num": 1}]),
        3
    )));
    assert!(!all_pgs_active(&stat(json!([{"name": "inactive", "num": 3}]), 3)));
    assert!(!all_pgs_active(&json!({})));
}

#[test]
fn a_cluster_with_one_osd_or_single_copy_pools_never_waits_for_ok_to_stop() {
    assert!(redundancy_impossible(&json!([0]), &json!([{"size": 2}])));
    assert!(redundancy_impossible(&json!([0, 1]), &json!([{"size": 2}, {"size": 1}])));
    assert!(!redundancy_impossible(&json!([0, 1]), &json!([{"size": 2}])));
}

#[test]
fn a_restart_lease_is_taken_only_when_free_expired_or_already_ours() {
    let now = Utc::now();
    let held = |who: &str, ago: i64| {
        json!({"spec": {
            "holderIdentity": who,
            "leaseDurationSeconds": 600,
            "renewTime": (now - chrono::Duration::seconds(ago)).to_rfc3339(),
        }})
    };
    assert!(lease_free_for(&held("n1", 10), "n1", now));
    assert!(!lease_free_for(&held("n2", 10), "n1", now));
    assert!(lease_free_for(&held("n2", 601), "n1", now));
    assert!(lease_free_for(&json!({"spec": {}}), "n1", now));
}

#[test]
fn a_kernel_mapping_made_as_admin_is_seen() {
    let root = tempfile::tempdir().unwrap();
    let dev = root.path().join("sys/bus/rbd/devices/0");
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::write(
        dev.join("config_info"),
        "[fd00:cafe::9]:3300 name=admin,key=client.admin,osd_request_timeout=300 images node2 -",
    )
    .unwrap();
    assert!(admin_kernel_mapped(root.path()));
    std::fs::write(
        dev.join("config_info"),
        "[fd00:cafe::9]:3300 name=yolab-images,key=client.yolab-images images node2 -",
    )
    .unwrap();
    assert!(!admin_kernel_mapped(root.path()));
    assert!(!admin_kernel_mapped(tempfile::tempdir().unwrap().path()));
}

#[test]
fn every_machine_must_boot_a_kernel_that_speaks_aes256k() {
    let node = |name: &str, kernel: &str| json!({"metadata": {"name": name}, "status": {"nodeInfo": {"kernelVersion": kernel}}});
    assert!(every_kernel_speaks_aes256k(&[node("n1", "7.2.9"), node("n2", "7.2.9")]).is_ok());
    let err = every_kernel_speaks_aes256k(&[node("n1", "7.2.9"), node("n2", "6.18.55")]).unwrap_err();
    assert!(err.contains("n2"));
    assert!(every_kernel_speaks_aes256k(&[]).is_err());
}

#[tokio::test]
async fn a_monitor_keyring_follows_a_rotated_mon_key_without_any_restart() {
    let root = tempfile::tempdir().unwrap();
    daemon_keyring(root.path(), "mon", "n1", "mon.", &aes(1));
    let (server, kube) = api_server().await;
    serve(&server, LEASE, 404, status(404, "NotFound")).await;
    let host = base_host().ok("ceph auth get mon.", &entry(&aes256k(2), None));
    let b = Backend { kube, host };

    let tick = converge_node(&b, root.path(), "n1").await.unwrap();

    assert_eq!(tick, Tick::Done);
    let text = std::fs::read_to_string(root.path().join("var/lib/ceph/mon/ceph-n1/keyring")).unwrap();
    assert_eq!(keyring_key(&text, "mon."), Some(aes256k(2)));
    assert!(!b.host.ran("systemctl"));
}

#[tokio::test]
async fn a_manager_adopts_its_pending_key_by_restarting_under_the_lease() {
    let root = tempfile::tempdir().unwrap();
    daemon_keyring(root.path(), "mgr", "n1", "mgr.n1", &aes(1));
    let (server, kube) = api_server().await;
    lease_free(&server).await;
    let host = base_host()
        .ok("ceph auth get mgr.n1", &entry(&aes(1), Some(&aes256k(3))))
        .ok("ceph pg stat", pgs_active())
        .ok("systemctl restart ceph-mgr-n1.service", "");
    let b = Backend { kube, host };

    let tick = converge_node(&b, root.path(), "n1").await.unwrap();

    assert!(matches!(tick, Tick::RequeueAfter(_)));
    assert!(b.host.ran("systemctl restart ceph-mgr-n1.service"));
    let text = std::fs::read_to_string(root.path().join("var/lib/ceph/mgr/ceph-n1/keyring")).unwrap();
    assert_eq!(keyring_key(&text, "mgr.n1"), Some(aes256k(3)));
}

#[tokio::test]
async fn no_daemon_restarts_while_another_machine_holds_the_lease() {
    let root = tempfile::tempdir().unwrap();
    daemon_keyring(root.path(), "mgr", "n1", "mgr.n1", &aes(1));
    let (server, kube) = api_server().await;
    lease_held_by(&server, "n2").await;
    let host = base_host()
        .ok("ceph auth get mgr.n1", &entry(&aes(1), Some(&aes256k(3))))
        .ok("ceph pg stat", pgs_active());
    let b = Backend { kube, host };

    let tick = converge_node(&b, root.path(), "n1").await.unwrap();

    assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("another machine")));
    assert!(!b.host.ran("systemctl"));
    let text = std::fs::read_to_string(root.path().join("var/lib/ceph/mgr/ceph-n1/keyring")).unwrap();
    assert_eq!(keyring_key(&text, "mgr.n1"), Some(aes(1)), "the key on disk is untouched");
}

fn osd_on_disk(root: &Path, n: u32) {
    let dir = root.join(format!("var/lib/ceph/osd/ceph-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    let dev = root.join(format!("dev-osd-{n}"));
    std::fs::write(&dev, "").unwrap();
    std::os::unix::fs::symlink(&dev, dir.join("block")).unwrap();
}

fn label(k: &str) -> String {
    json!({"/dev/x": {"osd_key": k}}).to_string()
}

#[tokio::test]
async fn an_osd_waits_while_stopping_it_would_make_data_unavailable() {
    let root = tempfile::tempdir().unwrap();
    osd_on_disk(root.path(), 2);
    let (server, kube) = api_server().await;
    lease_free(&server).await;
    let host = base_host()
        .ok("ceph auth get osd.2", &entry(&aes(1), Some(&aes256k(4))))
        .ok("ceph-bluestore-tool show-label", &label(&aes(1)))
        .ok("ceph pg stat", pgs_active())
        .fail("ceph osd ok-to-stop 2", "Error EBUSY: would make pgs inactive")
        .ok("ceph osd ls", "[0,1,2]")
        .ok("ceph osd pool ls detail", r#"[{"size":2}]"#);
    let b = Backend { kube, host };

    let tick = converge_node(&b, root.path(), "n1").await.unwrap();

    assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("ok to stop")));
    assert!(!b.host.ran("systemctl stop"));
    assert!(!b.host.ran("set-label-key"));
}

#[tokio::test]
async fn an_osd_takes_its_new_key_through_its_bluestore_label() {
    let root = tempfile::tempdir().unwrap();
    osd_on_disk(root.path(), 2);
    let (server, kube) = api_server().await;
    lease_free(&server).await;
    let want = aes256k(4);
    let host = base_host()
        .ok("ceph auth get osd.2", &entry(&aes(1), Some(&want)))
        .ok("ceph-bluestore-tool show-label", &label(&aes(1)))
        .ok("ceph pg stat", pgs_active())
        .ok("ceph osd ok-to-stop 2", r#"{"ok_to_stop":true}"#)
        .ok("systemctl stop yolab-ceph-osd@2.service", "")
        .ok("ceph-bluestore-tool set-label-key", "")
        .ok("systemctl start yolab-ceph-osd@2.service", "");
    let b = Backend { kube, host };

    let tick = converge_node(&b, root.path(), "n1").await.unwrap();

    assert!(matches!(tick, Tick::RequeueAfter(_)));
    let calls = b.host.calls();
    let at = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap();
    assert!(at("systemctl stop") < at("set-label-key"));
    assert!(at("set-label-key") < at("systemctl start"));
    assert!(b.host.ran(&format!("-k osd_key -v {want}")));
    assert!(!stopped_marker(root.path(), 2).exists());
}

#[tokio::test]
async fn an_osd_stopped_by_an_interrupted_rotation_is_started_again() {
    let root = tempfile::tempdir().unwrap();
    let marker = stopped_marker(root.path(), 5);
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "").unwrap();
    let (server, kube) = api_server().await;
    serve(&server, LEASE, 404, status(404, "NotFound")).await;
    let host = base_host().ok("systemctl start yolab-ceph-osd@5.service", "");
    let b = Backend { kube, host };

    converge_node(&b, root.path(), "n1").await.unwrap();

    assert!(b.host.ran("systemctl start yolab-ceph-osd@5.service"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn an_old_kernel_keeps_mapping_the_images_disk_as_before() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(root.path().join("proc/sys/kernel/osrelease"), "6.18.55\n").unwrap();
    let host = FakeHost::new();
    assert_eq!(images_identity(&host, root.path(), "images").await, None);
    assert!(host.calls().is_empty());
}

#[tokio::test]
async fn a_new_kernel_maps_the_images_disk_with_its_own_aes256k_key() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(root.path().join("proc/sys/kernel/osrelease"), "7.2.9\n").unwrap();
    let k = aes256k(9);
    let host = FakeHost::new()
        .ok(
            "ceph auth get-or-create client.yolab-images",
            &format!("[client.yolab-images]\n\tkey = {k}\n"),
        )
        .ok("chown", "");

    let path = images_identity(&host, root.path(), "images").await.unwrap();

    assert!(host.ran("auth get-or-create client.yolab-images mon profile rbd osd profile rbd pool=images --key_type aes256k"));
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(keyring_key(&text, "client.yolab-images"), Some(k));
}

#[tokio::test]
async fn an_images_key_already_on_disk_is_reused_without_asking_ceph() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("proc/sys/kernel")).unwrap();
    std::fs::write(root.path().join("proc/sys/kernel/osrelease"), "7.2.9\n").unwrap();
    std::fs::create_dir_all(root.path().join("etc/ceph")).unwrap();
    std::fs::write(
        root.path().join("etc/ceph/ceph.client.yolab-images.keyring"),
        format!("[client.yolab-images]\n\tkey = {}\n", aes256k(9)),
    )
    .unwrap();
    let host = FakeHost::new();
    assert!(images_identity(&host, root.path(), "images").await.is_some());
    assert!(host.calls().is_empty());
}

#[tokio::test]
async fn an_accepted_admin_key_is_left_alone() {
    let host = FakeHost::new().ok("ceph --connect-timeout 20 -s", "HEALTH_OK");
    let env = StorageEnv::from_lookup(|_| None);
    let tick = heal_admin_keyring(&host, Path::new("/nonexistent"), &env, &["fd00::2".into()])
        .await
        .unwrap();
    assert_eq!(tick, Tick::Done);
}

#[tokio::test]
async fn a_refused_admin_key_without_a_cluster_token_is_reported_not_guessed() {
    let host = FakeHost::new().fail(
        "ceph --connect-timeout 20 -s",
        "handle_auth_bad_method failed to auth with my available methods: (13) Permission denied",
    );
    let env = StorageEnv::from_lookup(|_| None);
    let tick = heal_admin_keyring(&host, Path::new("/nonexistent"), &env, &[]).await.unwrap();
    assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("cluster token")));
}

#[test]
fn an_unreachable_cluster_is_not_mistaken_for_a_refused_key() {
    assert!(!denied("RADOS timed out (error connecting to the cluster)"));
    assert!(denied("[errno 13] RADOS permission denied (error connecting to the cluster)"));
}

fn monmap(preferred: &str, service: &str, allowed: &[&str]) -> String {
    json!({
        "features": {"persistent": ["tentacle", "cephx_auth_aes256k"]},
        "auth_preferred_cipher": {"name": preferred},
        "auth_service_cipher": {"name": service},
        "auth_allowed_ciphers": allowed.iter().map(|a| json!({"name": a})).collect::<Vec<_>>(),
        "mons": [{"name": "n1"}],
    })
    .to_string()
}

fn auth_ls(entries: &[(&str, String)]) -> String {
    json!({"auth_dump": entries.iter().map(|(e, k)| json!({"entity": e, "key": k})).collect::<Vec<_>>()}).to_string()
}

async fn k8s_nodes(server: &MockServer, kernel: &str) {
    serve(
        server,
        "/api/v1/nodes",
        200,
        json!({"kind": "NodeList", "apiVersion": "v1", "metadata": {}, "items": [{
            "metadata": {"name": "n1"},
            "status": {
                "nodeInfo": {"kernelVersion": kernel},
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        }]}),
    )
    .await;
}

#[tokio::test]
async fn new_keys_become_aes256k_before_anything_is_rotated() {
    let (_server, kube) = api_server().await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes", "aes", &["aes", "aes256k"]))
        .ok("ceph mon set auth_preferred_cipher aes256k", "");
    let b = Backend { kube, host };
    rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(b.host.ran("mon set auth_preferred_cipher aes256k"));
    assert!(!b.host.ran("auth get-or-create-pending"));
}

#[tokio::test]
async fn one_daemon_at_a_time_gets_a_pending_key() {
    let (server, kube) = api_server().await;
    k8s_nodes(&server, "6.18.55").await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes256k", "aes", &["aes", "aes256k"]))
        .ok("ceph auth ls", &auth_ls(&[("osd.0", aes(1)), ("mgr.n1", aes(2))]))
        .ok("ceph auth get osd.0", &entry(&aes(1), None))
        .ok("ceph auth get mgr.n1", &entry(&aes(2), None))
        .ok("ceph osd metadata 0", r#"{"hostname":"n1"}"#)
        .ok("ceph auth get-or-create-pending osd.0", "");
    let b = Backend { kube, host };
    rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(b.host.ran("auth get-or-create-pending osd.0"));
    assert!(!b.host.ran("auth get-or-create-pending mgr.n1"));
}

#[tokio::test]
async fn no_new_rotation_starts_while_a_daemon_is_still_taking_its_key() {
    let (server, kube) = api_server().await;
    k8s_nodes(&server, "6.18.55").await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes256k", "aes", &["aes", "aes256k"]))
        .ok("ceph auth ls", &auth_ls(&[("osd.0", aes(1)), ("mgr.n1", aes(2))]))
        .ok("ceph auth get osd.0", &entry(&aes(1), Some(&aes256k(1))))
        .ok("ceph auth get mgr.n1", &entry(&aes(2), None));
    let b = Backend { kube, host };
    let tick = rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("osd.0")));
    assert!(!b.host.ran("get-or-create-pending"));
}

#[tokio::test]
async fn the_mon_key_rotates_only_with_every_monitor_in_quorum() {
    let (server, kube) = api_server().await;
    k8s_nodes(&server, "6.18.55").await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes256k", "aes", &["aes", "aes256k"]))
        .ok("ceph auth ls", &auth_ls(&[("osd.0", aes256k(1))]))
        .ok("ceph auth get osd.0", &entry(&aes256k(1), None))
        .ok("ceph auth get mon.", &entry(&aes(5), None))
        .ok(
            "ceph quorum_status",
            r#"{"quorum_names":["n1"],"monmap":{"mons":[{"name":"n1"},{"name":"n2"}]}}"#,
        );
    let b = Backend { kube, host };
    let tick = rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("quorum")));
    assert!(!b.host.ran("auth rotate mon."));
}

#[tokio::test]
async fn service_tickets_stay_as_they_are_while_any_kernel_is_older_than_7_0() {
    let (server, kube) = api_server().await;
    k8s_nodes(&server, "6.18.55").await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes256k", "aes", &["aes", "aes256k"]))
        .ok("ceph auth ls", &auth_ls(&[("osd.0", aes256k(1))]))
        .ok("ceph auth get osd.0", &entry(&aes256k(1), None))
        .ok("ceph auth get mon.", &entry(&aes256k(5), None));
    let b = Backend { kube, host };
    let tick = rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(matches!(tick, Tick::Idle(ref why) if why.contains("6.18.55")));
    assert!(!b.host.ran("auth_service_cipher"));
    assert!(!b.host.ran("auth rotate"));
}

#[tokio::test]
async fn service_tickets_stay_as_they_are_until_our_csi_driver_has_rolled_out() {
    let (server, kube) = api_server().await;
    k8s_nodes(&server, "7.2.9").await;
    serve(
        &server,
        "/apis/apps/v1/namespaces/rook-ceph/deployments/rook-ceph-operator",
        200,
        json!({"metadata": {"name": "rook-ceph-operator"}}),
    )
    .await;
    let host = FakeHost::new()
        .ok("ceph mon dump", &monmap("aes256k", "aes", &["aes", "aes256k"]))
        .ok("ceph auth ls", &auth_ls(&[("osd.0", aes256k(1))]))
        .ok("ceph auth get osd.0", &entry(&aes256k(1), None))
        .ok("ceph auth get mon.", &entry(&aes256k(5), None));
    let b = Backend { kube, host };
    let tick = rotation_step(&b, Path::new("/")).await.unwrap();
    assert!(matches!(tick, Tick::Idle(ref why) if why.contains("CSI")));
    assert!(!b.host.ran("auth_service_cipher"));
}
