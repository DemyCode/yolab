use super::*;

fn env() -> StorageEnv {
    StorageEnv::from_lookup(|_| None)
}

fn ctx_with(host: FakeHost, kube: crate::k8s::Kube) -> Ctx {
    Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools {
            host,
            kube,
            http: crate::http::client(),
        },
    }
}

fn ctx(host: FakeHost) -> Ctx {
    ctx_with(host, crate::k8s::Kube::from_environment())
}

fn reaching() -> crate::k8s::Kube {
    crate::k8s::Kube::with(crate::k8s::testing::unreachable())
}

fn put(path: &str) {
    let p = crate::storage::on_machine(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "").unwrap();
}

#[test]
fn a_ready_attempt_is_a_done_tick_and_a_wait_is_not_yet() {
    assert_eq!(tick_of(Attempt::Ready(())), Tick::Done);
    assert_eq!(
        tick_of(Attempt::NotYet("w".into())),
        Tick::NotYet("w".into())
    );
}

#[tokio::test]
async fn a_lock_folder_that_cannot_be_made_is_an_error() {
    put(lock::LOCK_DIR);
    assert!(locked("osd-activate", succeed).await.is_err());
}

#[tokio::test]
async fn every_node_storage_job_runs_on_each_node_under_its_lock() {
    let c = OsdActivateController { env: env() };
    assert_eq!(c.name(), "osd-activate");
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(120));
    assert_eq!(c.not_before_uptime(), Duration::ZERO);
    assert_eq!(c.depends_on(), &[crate::runtime::CEPH]);
    let grow = ImagesGrowController { env: env() };
    assert_eq!(grow.not_before_uptime(), Duration::from_secs(600));
}

#[tokio::test]
async fn osd_activation_fails_when_this_machines_osds_cannot_be_listed() {
    let err = OsdActivateController { env: env() }
        .converge(&ctx(FakeHost::new()))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "list this machine's OSDs");
}

#[tokio::test]
async fn a_system_osd_without_its_volume_is_an_error() {
    let err = SystemOsdController { env: env() }
        .converge(&ctx(FakeHost::new()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[tokio::test]
async fn the_image_store_waits_for_ceph_and_fails_on_what_ceph_refuses() {
    let c = ImagesRbdController { env: env() };
    assert!(matches!(
        c.converge(&ctx(FakeHost::new())).await.unwrap(),
        Tick::NotYet(_)
    ));
    let answering = FakeHost::new()
        .ok("ceph -s", "")
        .fail("ceph osd stat", "timed out");
    assert!(c.converge(&ctx(answering)).await.is_err());
}

#[tokio::test]
async fn jobs_that_need_ceph_do_nothing_until_it_answers() {
    assert_eq!(
        ImagesGrowController { env: env() }
            .converge(&ctx(FakeHost::new()))
            .await
            .unwrap(),
        Tick::Done
    );
    assert_eq!(
        DashboardController { env: env() }
            .converge(&ctx(FakeHost::new()))
            .await
            .unwrap(),
        Tick::Done
    );
    assert_eq!(
        MonMemberController { env: env() }
            .converge(&ctx(FakeHost::new()))
            .await
            .unwrap(),
        Tick::Done
    );
}

#[tokio::test]
async fn csi_secrets_are_a_cluster_job_that_needs_kubernetes() {
    let c = CsiSecretsController;
    assert_eq!(c.name(), "csi-secrets");
    assert_eq!(c.scope(), Scope::Cluster);
    assert_eq!(c.interval(), Duration::from_secs(120));
    assert_eq!(
        c.depends_on(),
        &[crate::runtime::CEPH, crate::runtime::KUBE_API]
    );
    assert!(c.converge(&ctx(FakeHost::new())).await.is_err());
    assert_eq!(
        c.converge(&ctx_with(FakeHost::new(), reaching()))
            .await
            .unwrap(),
        Tick::Done
    );
}

#[tokio::test]
async fn daemon_keys_are_minted_for_mgr_and_mds_and_each_failure_is_named() {
    let c = CephKeysController;
    assert_eq!(c.name(), "ceph-keys");
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(300));
    assert_eq!(c.depends_on(), &[crate::runtime::CEPH]);

    put("/var/lib/ceph/mgr/ceph-node1/keyring");
    put("/var/lib/ceph/mds");
    let host = FakeHost::new().ok("systemctl is-active", "");
    let err = c.converge(&ctx(host.clone())).await.unwrap_err();
    assert!(err.to_string().starts_with("mds: "), "{err}");
    assert!(!err.to_string().contains("mgr"), "{err}");

    std::fs::remove_file(crate::storage::on_machine("/var/lib/ceph/mds")).unwrap();
    put("/var/lib/ceph/mds/ceph-node1/keyring");
    assert_eq!(c.converge(&ctx(host)).await.unwrap(), Tick::Done);
}

#[tokio::test]
async fn a_machine_that_created_the_cluster_has_nothing_to_join() {
    let c = CephJoinController { env: env() };
    assert_eq!(c.name(), "ceph-join");
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(120));
    assert_eq!(c.not_before_uptime(), Duration::from_secs(60));
    assert_eq!(
        c.converge(&ctx(FakeHost::new())).await.unwrap(),
        Tick::Idle("this machine created the cluster".into())
    );
}

#[tokio::test]
async fn a_joined_machine_only_makes_sure_its_monitor_runs() {
    let mut joining = env();
    joining.join_seed_addr = "fd00::9".into();
    put("/var/lib/ceph/mon/ceph-node1/keyring");
    let host = FakeHost::new()
        .fail("systemctl is-active", "inactive")
        .fail("systemctl reset-failed", "no such unit")
        .fail("systemctl start", "no such unit");
    assert_eq!(
        CephJoinController { env: joining }
            .converge(&ctx(host.clone()))
            .await
            .unwrap(),
        Tick::Done
    );
    assert!(host.ran("systemctl start --no-block ceph-mon-node1.service"));
}

#[tokio::test]
async fn a_unit_systemctl_cannot_reach_is_only_warned_about() {
    let host = FakeHost::new()
        .fail("systemctl is-active", "inactive")
        .unstartable("systemctl start");
    ensure_started(&host, "ceph-mgr-n1.service").await;
    assert!(host.ran("systemctl start"));
}

#[tokio::test]
async fn csi_recovery_runs_once_per_boot_on_each_node_after_kubernetes() {
    let c = CsiRecoveryController;
    assert_eq!(c.name(), "csi-recovery");
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), Duration::from_secs(30));
    assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
    assert!(c.converge(&ctx(FakeHost::new())).await.is_err());
    assert!(c
        .converge(&ctx_with(FakeHost::new(), reaching()))
        .await
        .is_err());
    put(CSI_RECOVERED_MARKER);
    assert_eq!(
        c.converge(&ctx_with(FakeHost::new(), reaching()))
            .await
            .unwrap(),
        Tick::Idle("already done this boot".into())
    );
}

#[tokio::test]
async fn a_marker_that_cannot_be_written_leaves_the_recovery_to_run_again() {
    use crate::csi::testing::{plugin_pods_deleted, DAEMONSET};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("run"), "").unwrap();
    let marker = dir.path().join("run/marker");
    let (server, client) = crate::k8s::testing::api_server().await;
    crate::k8s::testing::serve(
        &server,
        DAEMONSET,
        200,
        serde_json::json!({ "metadata": { "name": "csi-cephfsplugin" } }),
    )
    .await;
    plugin_pods_deleted(&server, 200).await;
    assert!(once_per_boot(&marker, &client).await.is_err());
}

#[tokio::test]
async fn a_machine_that_cannot_join_the_cluster_is_a_tick_error() {
    let mut joining = env();
    joining.join_seed_addr = "fd00::9".into();
    let host = FakeHost::new();
    let err = CephJoinController { env: joining }
        .converge(&ctx(host.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no tunnel.account_token"), "{err}");
    assert!(!host.ran("systemctl"));
}
