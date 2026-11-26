use super::*;

fn jam_saves(record: &LocalRecord) {
    std::fs::create_dir_all(record.path.with_extension("tmp")).unwrap();
}

async fn at(step: Step) -> (tempfile::TempDir, LocalRecord, FakeNetwork, Heal) {
    let (host, net) = broken_cluster();
    let (dir, record) = local();
    let mut heal = started(&host, &net, &record).await;
    heal.step = step;
    record.save(&heal).unwrap();
    (dir, record, net, heal)
}

fn lost_app_data() -> FakeHost {
    FakeHost::new()
        .ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":0,"up":0,"in":1}],"pools":[{"pool":3,"pool_name":"yolab-fs-data0"}]}"#,
        )
        .ok(
            "ceph pg dump pgs_brief",
            r#"[{"pgid":"3.1","state":"down","up":[0],"acting":[0]}]"#,
        )
}

#[test]
fn a_heal_whose_record_lost_its_driver_cannot_tell_members_where_to_join() {
    let heal = Heal {
        id: "ab12".into(),
        driver: "node9".into(),
        started_at: NOW,
        finished_at: None,
        step: Step::Prepare,
        fsid: new_fsid(),
        members: vec![Member {
            name: "node3".into(),
            addr: "fd00::3".into(),
        }],
        gone: vec![],
        restart_boot_id: None,
        waiting: None,
        failed: None,
    };
    assert_eq!(
        heal.request_for(&heal.members[0]).unwrap_err().to_string(),
        "the heal's record does not list the machine driving it"
    );
}

#[test]
fn a_garbled_or_unreadable_heal_record_is_an_error_not_no_heal() {
    let (_d, record) = local();
    std::fs::create_dir_all(record.path.parent().unwrap()).unwrap();
    std::fs::write(&record.path, "not json").unwrap();
    assert!(record
        .load()
        .unwrap_err()
        .to_string()
        .ends_with("heal.json is unreadable"));

    let (_d, folder) = local();
    std::fs::create_dir_all(&folder.path).unwrap();
    assert!(folder.load().unwrap_err().to_string().starts_with("read "));
}

#[tokio::test]
async fn backups_stop_when_a_disk_holding_app_data_is_lost() {
    assert_eq!(
        backups_blocked(&lost_app_data()).await.as_deref(),
        Some("a disk holding app data does not answer — reconnect it, or use FORCE HEAL")
    );
    let unknown = FakeHost::new()
        .ok("ceph osd dump", r#"{"osds":[],"pools":[]}"#)
        .fail("ceph pg dump pgs_brief", "timed out");
    assert_eq!(backups_blocked(&unknown).await, None);
}

#[test]
fn the_platform_address_is_read_from_the_tunnel_settings() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    let read = |body: &str| {
        std::fs::write(&file, body).unwrap();
        RealNetwork::from_config(
            &crate::config::Config::for_test(&file),
            crate::k8s::Kube::from_environment(),
        )
        .platform_url
    };
    assert_eq!(
        read("[tunnel]\nplatform_api_url = \"https://api.example/\"\n"),
        "https://api.example"
    );
    assert_eq!(read("[tunnel]\naccount_token = \"t\"\n"), "");
    assert_eq!(read("[homelab]\n"), "");
    assert_eq!(read("not = = toml"), "");
}

#[test]
fn a_mon_status_that_is_not_what_ceph_sends_is_an_error() {
    let err = |raw: &str| parse_mon_status(raw).unwrap_err().to_string();
    assert_eq!(err("nope"), "mon_status is not JSON");
    assert_eq!(err(r#"{"state":"leader"}"#), "mon_status has no monmap");
    assert_eq!(
        err(r#"{"state":"leader","monmap":{"mons":[{"public_addrs":{}}]}}"#),
        "a mon without a name"
    );
}

#[test]
fn a_name_that_is_not_an_address_is_kept_as_it_is() {
    assert_eq!(normalize(" node1 "), "node1");
}

#[test]
fn kubernetes_nodes_are_named_with_their_ipv6_internal_address() {
    let nodes = named_addresses(&[
        json!({"metadata":{"name":"node1"},"status":{"addresses":[
            {"type":"InternalIP","address":"10.0.0.1"},
            {"type":"Hostname","address":"fd00::9"},
            {"type":"InternalIP","address":"fd00::1"},
        ]}}),
        json!({"metadata":{"name":"node2"},"status":{}}),
        json!({"metadata":{}}),
    ]);
    assert_eq!(
        nodes,
        vec![
            ("node1".to_string(), Some("fd00::1".to_string())),
            ("node2".to_string(), None),
        ]
    );
}

#[tokio::test]
async fn kubernetes_names_a_machine_no_other_list_knows() {
    let (host, net) = broken_cluster();
    *net.kubernetes.lock().unwrap() = Some(vec![
        ("node1".into(), Some("fd00::1".into())),
        ("node4".into(), Some("fd00::4".into())),
        ("node5".into(), None),
    ]);
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    let gone: Vec<String> = s.gone().map(MachineState::label).collect();
    assert_eq!(gone, ["node2", "node4"]);
    assert!(s.kubernetes);
}

#[tokio::test]
async fn kubernetes_that_errors_is_reported_unreadable() {
    let (host, net) = broken_cluster();
    let net = FakeNetwork {
        kubernetes_error: true,
        ..net
    };
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(
        s.unreadable,
        ["Kubernetes' nodes: the API server timed out"]
    );
    assert!(s.kubernetes);
}

#[tokio::test]
async fn data_out_of_reach_is_a_problem_and_unknown_placement_is_not_counted() {
    let host = lost_app_data().ok(
        MON_STATUS,
        &mon_status_json("leader", &[("node1", "fd00::1")]),
    );
    let net = FakeNetwork::with(&[("node1", "fd00::1")]);
    net.kubernetes_up(&["node1"]);
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(s.lost_groups, Some(1));
    assert_eq!(s.problems(), ["data_unreachable"]);

    let blind = FakeHost::new()
        .ok(
            MON_STATUS,
            &mon_status_json("leader", &[("node1", "fd00::1")]),
        )
        .fail("ceph osd dump", "timed out")
        .ok("ceph pg dump pgs_brief", "[]");
    let s = survey(&blind, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(s.lost_groups, None);
}

#[tokio::test]
async fn this_machines_own_replaceable_heal_does_not_block_a_new_one() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    let mut heal = started(&host, &net, &record).await;
    heal.step = Step::Rebuild;
    net.set_phase("fd00::3", "ab12", PhaseView::Armed, None);
    net.machines
        .lock()
        .unwrap()
        .get_mut("fd00::3")
        .unwrap()
        .reset
        .as_mut()
        .unwrap()
        .driver = "node3".into();
    let s = survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert_eq!(s.refusal(Some(&heal)), None);
    heal.id = "cd34".into();
    assert_eq!(
        s.refusal(Some(&heal)).as_deref(),
        Some("node3 is already healing the cluster")
    );
}

#[tokio::test]
async fn a_heal_does_not_start_over_an_unreadable_record_or_a_bad_address() {
    let (host, net) = broken_cluster();
    let (_d, record) = local();
    std::fs::create_dir_all(&record.path).unwrap();
    let start = |record: LocalRecord, addr: &'static str| {
        let (host, net) = (host.clone(), &net);
        async move {
            start_heal(
                &host,
                net,
                &record,
                "node1",
                addr,
                &request(),
                5_000,
                "ab12".into(),
                NOW,
            )
            .await
        }
    };
    assert!(start(record, "fd00::1").await.is_err());

    let (_d, record) = local();
    assert_eq!(
        start(record.clone(), "node1.lan")
            .await
            .unwrap_err()
            .to_string(),
        "this machine's cluster address \"node1.lan\" is not IPv6"
    );
    assert_eq!(record.load().unwrap(), None);

    let (_d, record) = local();
    jam_saves(&record);
    assert!(start(record.clone(), "fd00::1").await.is_err());
}

#[tokio::test]
async fn a_replaceable_heal_is_replaced_by_a_new_one() {
    let (_d, record, _net, _heal) = at(Step::Undo).await;
    let (host, net) = broken_cluster();
    let heal = start_heal(
        &host,
        &net,
        &record,
        "node1",
        "fd00::1",
        &request(),
        5_000,
        "cd34".into(),
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(record.load().unwrap().unwrap().id, heal.id);
}

#[test]
fn an_empty_list_reads_none() {
    assert_eq!(list(&BTreeSet::new()), "none");
    assert_eq!(list(&set(&["a", "b"])), "a, b");
}

#[tokio::test]
async fn a_tick_without_a_heal_is_idle_and_an_unreadable_record_is_an_error() {
    let (_d, record) = local();
    assert_eq!(
        tick(&FakeHost::new(), &FakeNetwork::default(), &record, "b", NOW)
            .await
            .unwrap(),
        Tick::Idle("no heal is driven from this machine".into())
    );
    std::fs::create_dir_all(&record.path).unwrap();
    assert!(
        tick(&FakeHost::new(), &FakeNetwork::default(), &record, "b", NOW)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_finished_heal_is_idle() {
    let (_d, record, net, mut heal) = at(Step::Rebuild).await;
    heal.finished_at = Some(NOW);
    record.save(&heal).unwrap();
    assert_eq!(
        tick(&FakeHost::new(), &net, &record, "b", NOW)
            .await
            .unwrap(),
        Tick::Idle("heal ab12 finished".into())
    );
}

#[tokio::test]
async fn a_step_that_errors_is_recorded_as_what_the_heal_waits_on() {
    let (_d, record, net, mut heal) = at(Step::Rebuild).await;
    heal.restart_boot_id = Some("b".into());
    record.save(&heal).unwrap();
    let host = FakeHost::new().unstartable("systemctl reboot");
    let err = tick(&host, &net, &record, "b", NOW).await.unwrap_err();
    assert_eq!(
        record.load().unwrap().unwrap().waiting,
        Some(format!("{err:#}"))
    );

    jam_saves(&record);
    assert!(tick(&host, &net, &record, "b", NOW).await.is_err());
}

#[tokio::test]
async fn every_outcome_of_a_step_must_be_saved() {
    let (_d, record, net, _) = at(Step::Prepare).await;
    jam_saves(&record);
    assert!(tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .is_err());

    let (_d, record, net, _) = at(Step::Prepare).await;
    net.machines
        .lock()
        .unwrap()
        .get_mut("fd00::3")
        .unwrap()
        .name = "node7".into();
    jam_saves(&record);
    assert!(tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .is_err());

    let (_d, record, net, _) = at(Step::Undo).await;
    jam_saves(&record);
    assert!(tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .is_err());

    let (_d, record, net, _) = at(Step::Restart).await;
    jam_saves(&record);
    let host = FakeHost::new().ok("systemctl reboot", "");
    assert!(tick(&host, &net, &record, "b", NOW).await.is_err());
    assert!(!host.ran("systemctl reboot"));
}

#[tokio::test]
async fn a_wait_already_recorded_is_not_saved_again() {
    let (_d, record, net, _) = at(Step::Prepare).await;
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    jam_saves(&record);
    assert_eq!(
        tick(&FakeHost::new(), &net, &record, "b", NOW)
            .await
            .unwrap(),
        Tick::Done
    );
}

#[tokio::test]
async fn a_restart_this_machine_refuses_is_an_error() {
    let (_d, record, net, _) = at(Step::Restart).await;
    let host = FakeHost::new().fail("systemctl reboot", "access denied");
    let err = tick(&host, &net, &record, "b", NOW).await.unwrap_err();
    assert!(err.to_string().starts_with("systemctl reboot: "), "{err}");

    let (_d, record, net, _) = at(Step::Restart).await;
    let host = FakeHost::new().unstartable("systemctl reboot");
    assert!(tick(&host, &net, &record, "b", NOW).await.is_err());
}

#[tokio::test]
async fn arming_fails_on_a_machine_that_is_silent_or_not_prepared() {
    let (_d, record, net, _) = at(Step::Arm).await;
    net.disconnect("fd00::3");
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    let failed = record.load().unwrap().unwrap().failed.unwrap();
    assert_eq!(failed, "node3 stopped answering: connection timed out");

    let (_d, record, net, _) = at(Step::Arm).await;
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    let failed = record.load().unwrap().unwrap().failed.unwrap();
    assert_eq!(
        failed,
        "node3 is no longer prepared (it has nothing for this heal)"
    );

    let (_d, record, net, _) = at(Step::Arm).await;
    net.set_phase("fd00::3", "ab12", PhaseView::Restarted, None);
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    let failed = record.load().unwrap().unwrap().failed.unwrap();
    assert_eq!(failed, "node3 is no longer prepared (restarted)");
}

#[tokio::test]
async fn a_machine_already_armed_is_not_armed_again() {
    let (_d, record, net, _) = at(Step::Arm).await;
    net.set_phase("fd00::3", "ab12", PhaseView::Armed, None);
    net.set_phase("fd00::1", "ab12", PhaseView::Prepared, None);
    let host = FakeHost::new().ok("systemctl reboot", "");
    tick(&host, &net, &record, "b", NOW).await.unwrap();
    let calls = net.calls();
    assert!(!calls.contains(&"arm fd00::3".to_string()), "{calls:?}");
    assert!(calls.contains(&"arm fd00::1".to_string()), "{calls:?}");
}

#[tokio::test]
async fn preparing_waits_for_silent_machines_and_retries_a_refused_prepare() {
    let (_d, record, net, _) = at(Step::Prepare).await;
    net.disconnect("fd00::3");
    net.refuse("fd00::1", "busy");
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    assert_eq!(
        record.load().unwrap().unwrap().waiting.unwrap(),
        "node3 does not answer (connection timed out); node1 did not start preparing (busy)"
    );
}

#[tokio::test]
async fn preparing_fails_on_a_machine_that_changed_name_or_walked_away() {
    let (_d, record, net, _) = at(Step::Prepare).await;
    net.machines
        .lock()
        .unwrap()
        .get_mut("fd00::3")
        .unwrap()
        .name = "node7".into();
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    assert_eq!(
        record.load().unwrap().unwrap().failed.unwrap(),
        "fd00::3 now answers as node7, not node3"
    );

    let (_d, record, net, _) = at(Step::Prepare).await;
    net.set_phase("fd00::3", "ab12", PhaseView::Undone, None);
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    assert_eq!(
        record.load().unwrap().unwrap().failed.unwrap(),
        "node3 abandoned its part in the heal"
    );
}

#[tokio::test]
async fn preparing_a_heal_whose_record_lost_its_driver_is_an_error() {
    let (_d, record, net, mut heal) = at(Step::Prepare).await;
    heal.driver = "node9".into();
    record.save(&heal).unwrap();
    assert!(tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .is_err());
    assert!(net.calls().is_empty());
}

#[tokio::test]
async fn rebuilding_skips_silent_machines_and_waits_on_kubernetes_that_errors() {
    let (_d, record, net, _) = at(Step::Rebuild).await;
    net.disconnect("fd00::3");
    let net = FakeNetwork {
        kubernetes_error: true,
        ..net
    };
    assert_eq!(
        tick(&FakeHost::new(), &net, &record, "b", NOW)
            .await
            .unwrap_err()
            .to_string(),
        "the API server timed out"
    );
    assert!(!net.calls().iter().any(|c| c.starts_with("reboot")));
}

#[tokio::test]
async fn a_machine_the_platform_never_listed_is_not_deleted_from_it() {
    let (_d, record, net, mut heal) = at(Step::Rebuild).await;
    heal.gone[0].platform_id = None;
    record.save(&heal).unwrap();
    net.kubernetes_up(&["node1", "node3"]);
    tick(&FakeHost::new(), &net, &record, "b", NOW)
        .await
        .unwrap();
    assert!(!record.load().unwrap().unwrap().running());
    assert!(!net
        .calls()
        .iter()
        .any(|c| c.starts_with("delete platform node")));
}

#[test]
fn the_page_sees_the_heal_by_its_member_and_machine_names() {
    let heal = Heal {
        id: "ab12".into(),
        driver: "node1".into(),
        started_at: NOW,
        finished_at: Some(NOW + 5),
        step: Step::Rebuild,
        fsid: "f".into(),
        members: vec![Member {
            name: "node1".into(),
            addr: "fd00::1".into(),
        }],
        gone: vec![Gone {
            label: "node2".into(),
            addr: "fd00::2".into(),
            platform_id: None,
        }],
        restart_boot_id: None,
        waiting: Some("w".into()),
        failed: None,
    };
    assert_eq!(
        heal_json(&heal),
        json!({
            "id": "ab12",
            "driver": "node1",
            "running": false,
            "failed": null,
            "started_at": NOW,
            "finished_at": NOW + 5,
            "step": "rebuild",
            "steps": ["prepare", "arm", "restart", "rebuild"],
            "members": ["node1"],
            "removed_machines": ["node2"],
            "waiting": "w",
        })
    );
}

#[test]
fn a_heal_walks_prepare_arm_restart_rebuild_and_stops_after_rebuild_or_undo() {
    assert_eq!(Step::Prepare.next(), Some(Step::Arm));
    assert_eq!(Step::Arm.next(), Some(Step::Restart));
    assert_eq!(Step::Restart.next(), Some(Step::Rebuild));
    assert_eq!(Step::Rebuild.next(), None);
    assert_eq!(Step::Undo.next(), None);
}

#[tokio::test]
async fn a_heal_runs_only_until_it_has_a_finish_time() {
    let (_d, _record, _net, mut heal) = at(Step::Rebuild).await;
    assert!(heal.running());
    heal.finished_at = Some(NOW);
    assert!(!heal.running());
}

#[tokio::test]
async fn a_silent_machine_is_logged_but_this_machine_is_not() {
    let (host, _) = broken_cluster();
    let net = FakeNetwork::with(&[("node3", "fd00::3")]).platform(&[
        (11, "fd00::1"),
        (12, "fd00::2"),
        (13, "fd00::3"),
    ]);
    let logs = crate::testkit::capture_logs();
    survey(&host, &net, "node1", "fd00::1", 5_000).await;
    assert!(
        logs.contain("heal: fd00::2 does not answer"),
        "{}",
        logs.text()
    );
    assert!(
        !logs.contain("heal: fd00::1 does not answer"),
        "{}",
        logs.text()
    );
}
