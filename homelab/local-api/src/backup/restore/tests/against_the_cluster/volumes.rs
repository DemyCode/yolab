use super::*;
use std::collections::BTreeMap;

fn in_tree(pvcs: Value) -> FakeHost {
    restic_holding(vec![
        ("catalog.json", catalog(pvcs, Value::Null)),
        ("yolab-notes.yaml", settings()),
    ])
}

fn data_and_kept() -> Value {
    json!([
        { "name": "data", "capacity": "1Gi", "store": "services/yolab-notes", "folder": "notes" },
        { "name": "kept", "capacity": "1Gi", "store": "services/yolab-notes", "folder": "kept" }
    ])
}

fn keep(claim: &str) -> rf::Choices {
    rf::Choices {
        data: BTreeMap::from([(claim.to_string(), rf::Choice::Use)]),
    }
}

async fn restored(
    volumes: Vec<Value>,
    host: FakeHost,
    choices: rf::Choices,
    last_words: &str,
) -> (MockServer, Value) {
    let (server, kube, kept) = running_service_with(volumes).await;
    crate::tree::testing::jobs_finish_saying(&server, last_words).await;
    let b = Backend { kube, host };
    start(&b, "yolab-notes", Some("abc123".into()), choices)
        .await
        .unwrap();
    assert_eq!(drive_next(&b).await.unwrap(), Tick::Done);
    let sets = kept.data("sets");
    (server, sets[0].clone())
}

fn jobs_started(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/jobs"))
        .count()
}

#[tokio::test]
async fn each_volume_comes_back_from_its_folder_backup_unless_it_is_kept() {
    let (server, set) = restored(
        vec![
            crate::tree::testing::volume("yolab-notes", "data", "notes"),
            crate::tree::testing::volume("yolab-notes", "kept", "kept"),
        ],
        in_tree(data_and_kept()),
        keep("kept"),
        "YOLAB_RESTORED\n",
    )
    .await;
    assert_eq!(set["state"], SUCCEEDED, "{set}");
    assert_eq!(jobs_started(&server.received_requests().await.unwrap()), 1);
}

#[tokio::test]
async fn a_folder_restore_that_does_not_finish_fails_the_restore() {
    let (_server, set) = restored(
        vec![crate::tree::testing::volume("yolab-notes", "data", "notes")],
        in_tree(data_and_kept()),
        keep("kept"),
        "half way\n",
    )
    .await;
    assert_eq!(set["state"], FAILED, "{set}");
}

#[tokio::test]
async fn a_volume_the_service_no_longer_has_fails_the_restore() {
    let (_server, set) = restored(vec![], in_tree(data_and_kept()), keep("kept"), "").await;
    assert_eq!(set["state"], FAILED, "{set}");
    assert!(set["error"]
        .as_str()
        .unwrap()
        .contains("the backup holds data, which this service no longer has"));
}

#[tokio::test]
async fn a_volume_never_backed_up_is_kept_as_it_is() {
    let host = in_tree(data_and_kept());
    host.now_answers(
        "restic snapshots --no-lock --json --tag yolab-folders",
        "[]",
    );
    let (server, set) = restored(vec![], host, Default::default(), "").await;
    assert_eq!(set["state"], SUCCEEDED, "{set}");
    assert_eq!(jobs_started(&server.received_requests().await.unwrap()), 0);
}

fn new_folder() -> crate::tree::Planned {
    crate::tree::Planned {
        claimed: crate::tree::Claimed {
            namespace: "yolab-notes2".into(),
            claim: "data".into(),
            folder: "notes2".into(),
        },
        volume: crate::tree::testing::volume("yolab-notes2", "data", "notes2"),
        request: json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": "data", "namespace": "yolab-notes2" },
        }),
    }
}

fn only_data() -> rf::Choices {
    rf::Choices {
        data: BTreeMap::from([
            ("cache".to_string(), rf::Choice::Empty),
            ("old".to_string(), rf::Choice::Empty),
        ]),
    }
}

async fn payload_cluster() -> (MockServer, Backend<FakeHost>, crate::records::testing::Kept) {
    let (server, kube) = api_server().await;
    enabled(&server).await;
    let kept = crate::records::testing::kept(&server, "yolab-restores").await;
    crate::k8s::testing::accept_patches(&server).await;
    crate::tree::testing::jobs_finish_saying(&server, "YOLAB_RESTORED\n").await;
    let pvcs = json!([
        { "name": "data", "capacity": "1Gi", "store": "services/yolab-notes", "folder": "notes" },
        { "name": "cache", "capacity": "1Gi", "store": "services/yolab-notes", "folder": "notes.cache",
          "snapshot_id": "gone", "snapshot_time": "t" },
        { "name": "old", "capacity": "1Gi", "snapshot_id": "o1", "snapshot_time": "t" }
    ]);
    let b = Backend {
        kube,
        host: in_tree(pvcs),
    };
    (server, b, kept)
}

#[tokio::test]
async fn an_install_from_a_backup_fills_its_new_folders_and_records_it() {
    let (server, b, kept) = payload_cluster().await;
    let payload = backup_payload(&b, "yolab-notes", "abc123", only_data())
        .await
        .unwrap();
    payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[new_folder()])
        .await
        .unwrap();
    assert_eq!(kept.data("sets")[0]["state"], SUCCEEDED);
    let patched = crate::k8s::testing::patched(&server).await;
    assert!(patched
        .iter()
        .any(|p| p["kind"] == "PersistentVolumeClaim"
            && p["metadata"]["namespace"] == "yolab-notes2"));
    assert_eq!(jobs_started(&server.received_requests().await.unwrap()), 1);
}

#[tokio::test]
async fn an_install_whose_backup_cannot_fill_a_volume_fails_and_says_why() {
    let (_server, b, kept) = payload_cluster().await;
    let payload = backup_payload(&b, "yolab-notes", "abc123", Default::default())
        .await
        .unwrap();
    let err = payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[new_folder()])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("has since been deleted"), "{err}");
    assert_eq!(kept.data("sets")[0]["state"], FAILED);

    let payload = backup_payload(&b, "yolab-notes", "abc123", only_data())
        .await
        .unwrap();
    let err = payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("which this service no longer has"), "{err}");
}

#[tokio::test]
async fn an_install_that_keeps_the_new_volumes_empty_starts_no_job() {
    let (server, b, _kept) = payload_cluster().await;
    let mut choices = only_data();
    choices.data.insert("data".into(), rf::Choice::Empty);
    let payload = backup_payload(&b, "yolab-notes", "abc123", choices)
        .await
        .unwrap();
    payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[new_folder()])
        .await
        .unwrap();
    assert_eq!(jobs_started(&server.received_requests().await.unwrap()), 0);
}

#[test]
fn a_new_services_volume_is_restored_and_never_replaced() {
    for chosen in [
        None,
        Some(rf::Choice::Replace),
        Some(rf::Choice::Restore),
        Some(rf::Choice::Use),
    ] {
        assert_eq!(
            rf::step(false, true, chosen, rf::Choice::Use),
            rf::Step::Restore
        );
    }
}
