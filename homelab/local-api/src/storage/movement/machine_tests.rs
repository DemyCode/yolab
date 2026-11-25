use super::*;
use crate::host::fake::FakeHost;
use serde_json::json;

fn moving_status() -> Value {
    json!({
        "pgmap": {
            "pgs_by_state": [
                {"state_name": "active+remapped+backfilling", "count": 4},
                {"state_name": "active+clean", "count": 4},
            ],
            "num_pgs": 8,
            "num_objects": 100,
            "data_bytes": 1000u64,
            "misplaced_objects": 50,
        },
        "osdmap": {"num_osds": 3, "num_up_osds": 2},
    })
}

fn settled_status() -> Value {
    json!({
        "pgmap": {
            "pgs_by_state": [{"state_name": "active+clean", "count": 8}],
            "num_pgs": 8,
        },
        "osdmap": {"num_osds": 2, "num_up_osds": 2},
    })
}

fn status(host: FakeHost, status: &Value) -> FakeHost {
    host.ok("ceph status", &status.to_string())
        .ok("ceph osd dump", r#"{"flags":"noout,sortbitwise"}"#)
        .fail("ceph progress json", "mgr is starting")
}

#[test]
fn pg_states_without_a_name_or_a_count_are_skipped() {
    let s = snapshot(
        &json!({"pgmap": {"pgs_by_state": [
            {"count": 3},
            {"state_name": "peering"},
            {"state_name": "peering", "count": 2},
        ]}}),
        "",
    );
    assert_eq!(s.inactive_pgs, 2);
}

#[test]
fn a_clone_is_found_by_its_reference_and_its_progress_read_from_its_message() {
    let p = json!({"events": [
        {"id": "other", "refs": ["pool"], "message": "1 ongoing clones"},
        {"id": "x", "refs": ["clone"], "message": "2 ongoing clones - average progress is 40%"},
    ]});
    assert_eq!(
        clones(&p),
        Some(Clones {
            count: 2,
            progress: 0.4
        })
    );
}

#[test]
fn a_clone_without_any_progress_is_not_reported() {
    assert_eq!(clones(&json!({})), None);
    assert_eq!(clones(&json!({"events": []})), None);
    let silent =
        json!({"events": [{"id": "mgr-vol-ongoing-clones", "message": "1 ongoing clones"}]});
    assert_eq!(clones(&silent), None);
    let garbled = json!({"events": [{
        "id": "mgr-vol-ongoing-clones",
        "message": "1 ongoing clones - average progress is 1.2.3%",
    }]});
    assert_eq!(clones(&garbled), None);
}

#[test]
fn nothing_is_filling_when_the_cluster_reports_no_average_use() {
    assert_eq!(filling_osds(&json!({"summary": {}})), Vec::<i64>::new());
}

#[test]
fn a_tracker_without_recent_samples_has_no_eta() {
    let t = Tracker {
        peak: 100,
        samples: VecDeque::from([(0, 100)]),
    };
    assert_eq!(t.estimate(WINDOW_MS * 2, 50).eta_secs, None);
}

#[test]
fn draining_disks_are_listed_by_machine_then_name() {
    let published = BTreeMap::from([
        (
            "node2".to_string(),
            json!({"disks": {"a": {"phase": "draining", "model": "Zed"}}}).to_string(),
        ),
        (
            "node1".to_string(),
            json!({"disks": {
                "b": {"phase": "removing", "model": "Beta"},
                "c": {"phase": "draining", "model": "Alpha"},
            }})
            .to_string(),
        ),
    ]);
    let names: Vec<(String, String)> = draining_disks(&published)
        .into_iter()
        .map(|d| (d.node, d.name))
        .collect();
    assert_eq!(
        names,
        [
            ("node1".to_string(), "Alpha".to_string()),
            ("node1".to_string(), "Beta".to_string()),
            ("node2".to_string(), "Zed".to_string()),
        ]
    );
}

#[tokio::test]
async fn nothing_is_sampled_while_ceph_does_not_answer() {
    let host = FakeHost::new();
    sample(&host).await;
    assert!(!host.ran("ceph config-key"));
}

#[tokio::test]
async fn a_settled_cluster_with_no_record_writes_none() {
    let host = status(FakeHost::new(), &settled_status())
        .fail("ceph config-key get", "Error ENOENT: no such key");
    sample(&host).await;
    assert!(!host.ran("ceph config-key set"));
}

#[tokio::test]
async fn data_on_the_move_is_sampled_into_the_record() {
    let host = status(FakeHost::new(), &moving_status())
        .fail("ceph config-key get", "Error ENOENT: no such key")
        .fail("ceph config-key set", "timed out");
    sample(&host).await;
    assert!(host.ran("ceph config-key set yolab/movement {\"jobs\":{\"move\""));
}

#[tokio::test]
async fn an_unreadable_record_is_left_as_it_is() {
    let host = status(FakeHost::new(), &moving_status()).ok("ceph config-key get", "not json");
    sample(&host).await;
    assert!(!host.ran("ceph config-key set"));
}

#[tokio::test]
async fn the_movement_is_unknown_while_ceph_does_not_answer() {
    assert_eq!(assess_via(&FakeHost::new()).await.state, State::Unknown);
}

#[tokio::test]
async fn moving_data_names_the_machines_with_down_disks_and_the_disks_that_fill() {
    let host = status(FakeHost::new(), &moving_status())
        .fail("ceph config-key get yolab/movement", "timed out")
        .ok(
            "ceph osd tree",
            &json!({"nodes": [
                {"type": "host", "name": "node2", "children": [1]},
                {"type": "osd", "id": 1, "status": "down"},
            ]})
            .to_string(),
        )
        .ok(
            "ceph config-key dump yolab/disk-status/",
            &json!({"yolab/disk-status/node1": json!({"disks": {
                "d1": {"osd_id": 0, "model": "Fresh"},
            }}).to_string()})
            .to_string(),
        )
        .ok(
            "ceph osd df",
            &json!({
                "summary": {"average_utilization": 40.0},
                "nodes": [{"id": 0, "reweight": 1.0, "crush_weight": 1.0, "utilization": 1.0}],
            })
            .to_string(),
        )
        .ok(
            "ceph config-key get yolab/storage-policy",
            r#"{"size":2,"failure_domain":"host"}"#,
        );
    let m = assess_via(&host).await;
    assert_eq!(m.waiting_for, ["node2"]);
    assert_eq!(m.move_reason, Some(MoveReason::Filling));
    assert_eq!(
        m.filling,
        [DiskRef {
            node: "node1".into(),
            name: "Fresh".into()
        }]
    );
    assert_eq!(m.copies, Some(2));
    assert_eq!(m.state, State::Restarting);
    assert!(!host.ran("ceph osd pool ls detail"));
}

#[tokio::test]
async fn moving_data_with_no_disk_filling_or_draining_is_pools_resizing_or_balancing() {
    let resizing = status(FakeHost::new(), &moving_status())
        .fail("ceph osd tree", "timed out")
        .fail("ceph config-key dump", "timed out")
        .fail("ceph osd df", "timed out")
        .ok(
            "ceph osd pool ls detail",
            r#"[{"pg_num":32,"pg_num_target":64}]"#,
        );
    assert_eq!(
        assess_via(&resizing).await.move_reason,
        Some(MoveReason::Resizing)
    );

    let balancing =
        status(FakeHost::new(), &moving_status()).fail("ceph osd pool ls detail", "timed out");
    assert_eq!(
        assess_via(&balancing).await.move_reason,
        Some(MoveReason::Balancing)
    );
}

#[tokio::test]
async fn draining_disks_are_the_reason_without_looking_for_filling_ones() {
    let host = status(FakeHost::new(), &moving_status()).ok(
        "ceph config-key dump yolab/disk-status/",
        &json!({"yolab/disk-status/node1": json!({"disks": {
            "d1": {"phase": "draining", "model": "Old"},
        }}).to_string()})
        .to_string(),
    );
    let m = assess_via(&host).await;
    assert_eq!(m.move_reason, Some(MoveReason::Draining));
    assert!(!host.ran("ceph osd df"));
}

#[tokio::test]
async fn a_settled_cluster_has_no_move_reason() {
    let host = status(FakeHost::new(), &settled_status());
    let m = assess_via(&host).await;
    assert_eq!(m.move_reason, None);
    assert_eq!(m.state, State::Settled);
    assert_eq!(m.waiting_for, Vec::<String>::new());
}

fn cloning(status: &Value) -> FakeHost {
    FakeHost::new()
        .ok("ceph status", &status.to_string())
        .ok("ceph osd dump", r#"{"flags":"sortbitwise"}"#)
        .ok(
            "ceph progress json",
            &json!({"events": [
                {"id": "x", "refs": ["clone"], "message": "2 ongoing clones - average progress is 40%"},
            ]})
            .to_string(),
        )
}

#[tokio::test]
async fn ongoing_clones_are_sampled_and_the_record_is_saved() {
    let host = cloning(&settled_status())
        .fail("ceph config-key get", "Error ENOENT: no such key")
        .ok("ceph config-key set", "");
    sample(&host).await;
    assert!(host.ran("ceph config-key set yolab/movement"));
}

#[tokio::test]
async fn ongoing_clones_are_counted_in_the_movement() {
    let host = cloning(&settled_status());
    assert_eq!(assess_via(&host).await.clones, 2);
}
