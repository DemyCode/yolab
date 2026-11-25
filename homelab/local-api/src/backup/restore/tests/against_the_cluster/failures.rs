use super::*;

const FOLDERS: &str = "restic snapshots --no-lock --json --tag yolab-folders";

fn why<T>(result: anyhow::Result<T>) -> String {
    format!("{:#}", result.err().unwrap())
}

fn dated(files: Vec<(&'static str, String)>) -> FakeHost {
    FakeHost::new()
        .ok("restic unlock", "")
        .restic_restores(files)
        .ok(
            "restic snapshots --no-lock abc123 --json",
            r#"[{"id":"abc123","time":"2026-11-20T00:00:00Z"}]"#,
        )
}

fn unreadable(name: &'static str) -> impl Fn(&str, &[&str]) + Send + Sync + 'static {
    move |_: &str, args: &[&str]| {
        let wanted = format!("**/{name}");
        if args.contains(&wanted.as_str()) {
            let target = args[args.iter().position(|a| *a == "--target").unwrap() + 1];
            std::fs::create_dir_all(target).unwrap();
            std::os::unix::fs::symlink("/proc/self/mem", format!("{target}/{name}")).unwrap();
        }
    }
}

fn notes_catalog() -> (&'static str, String) {
    ("catalog.json", catalog(json!([]), Value::Null))
}

fn nameless_volume_catalog() -> (&'static str, String) {
    (
        "catalog.json",
        catalog(json!([{ "capacity": "1Gi" }]), Value::Null),
    )
}

fn folder_catalog() -> (&'static str, String) {
    (
        "catalog.json",
        catalog(
            json!([{ "name": "data", "capacity": "1Gi", "store": "services/yolab-notes", "folder": "notes" }]),
            Value::Null,
        ),
    )
}

fn notes_yaml() -> (&'static str, String) {
    ("yolab-notes.yaml", settings())
}

fn garbled_yaml() -> (&'static str, String) {
    ("yolab-notes.yaml", "not json".to_string())
}

async fn backed_up(host: FakeHost) -> (MockServer, Backend<FakeHost>) {
    let (server, kube) = api_server().await;
    enabled(&server).await;
    (server, Backend { kube, host })
}

async fn refused(server: &MockServer, at: &str) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(500).set_body_json(status(500, "InternalError")))
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn backups_whose_config_cannot_be_read_are_errors_everywhere() {
    let (server, kube) = api_server().await;
    refused(&server, CONFIG_PATH).await;
    let b = Backend {
        kube,
        host: FakeHost::new(),
    };
    assert!(start(&b, "yolab-notes", None, Default::default())
        .await
        .is_err());
    assert!(backup_versions(&b).await.is_err());
    assert!(definition_from_backup(&b, "yolab-notes", "abc123")
        .await
        .is_err());
    assert!(kept_from_backup(&b, "yolab-notes", "abc123").await.is_err());
    assert!(restore_plan(&b, "yolab-notes", "abc123").await.is_err());
    assert!(
        backup_payload(&b, "yolab-notes", "abc123", Default::default())
            .await
            .is_err()
    );
    assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
}

#[tokio::test]
async fn a_service_outside_the_apps_is_never_read_from_a_backup() {
    let (_server, b) = backed_up(FakeHost::new()).await;
    assert!(
        why(backup_payload(&b, "kube-system", "abc123", Default::default()).await)
            .contains("not an app namespace")
    );
    assert!(
        why(kept_from_backup(&b, "kube-system", "abc123").await).contains("not an app namespace")
    );
    assert!(why(restore_plan(&b, "kube-system", "abc123").await).contains("not an app namespace"));
    assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
}

#[tokio::test]
async fn a_start_that_cannot_find_its_snapshot_or_stop_the_service_or_queue_is_an_error() {
    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .fail("restic snapshots", "Fatal: wrong password"),
    )
    .await;
    assert!(
        why(start(&b, "yolab-notes", None, Default::default()).await).contains("wrong password")
    );

    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .ok("restic snapshots", "[]")
            .fail("restic find", "Fatal: wrong password"),
    )
    .await;
    assert!(
        why(start(&b, "yolab-notes", None, Default::default()).await).contains("wrong password")
    );

    let (server, b) = backed_up(FakeHost::new()).await;
    refused(&server, "/apis/apps/v1/namespaces/yolab-notes/deployments").await;
    assert!(
        start(&b, "yolab-notes", Some("abc123".into()), Default::default())
            .await
            .is_err()
    );

    let (server, b) = backed_up(FakeHost::new()).await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list("Deployment", vec![])))
        .mount(&server)
        .await;
    refused(&server, &crate::records::testing::path_of("yolab-restores")).await;
    assert!(
        start(&b, "yolab-notes", Some("abc123".into()), Default::default())
            .await
            .is_err()
    );
}

async fn attempted(record: Value, config: u16) -> Value {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path(CONFIG_PATH))
        .respond_with(ResponseTemplate::new(config).set_body_json(status(config, "Refused")))
        .mount(&server)
        .await;
    scaled("web", 3).mount(&server).await;
    daemonsets(&[]).mount(&server).await;
    crate::records::testing::records(&server, "yolab-restores", json!([record])).await;
    crate::records::testing::accept_writes(&server).await;
    let b = Backend {
        kube,
        host: FakeHost::new(),
    };
    assert_eq!(drive_next(&b).await.unwrap(), Tick::Done);
    let written = crate::records::testing::written(&server, "yolab-restores").await;
    written.last().unwrap()[0].clone()
}

#[tokio::test]
async fn a_restore_without_a_snapshot_or_a_backup_config_fails_and_says_why() {
    let done = attempted(queued_record(json!({ "snapshot_id": null })), 404).await;
    assert_eq!(done["state"], FAILED);
    assert_eq!(done["error"], "restore rs-1 names no snapshot");

    let done = attempted(queued_record(json!({})), 404).await;
    assert_eq!(done["state"], FAILED);
    assert_eq!(done["error"], "backup not configured");

    let done = attempted(queued_record(json!({})), 500).await;
    assert_eq!(done["state"], FAILED);
}

async fn quiet_service(volumes: u16) -> (MockServer, Client) {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list("Deployment", vec![])))
        .mount(&server)
        .await;
    daemonsets(&[]).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/persistentvolumes"))
        .respond_with(
            ResponseTemplate::new(volumes).set_body_json(list("PersistentVolume", vec![])),
        )
        .mount(&server)
        .await;
    (server, kube)
}

async fn restored_in_place(kube: Client, host: FakeHost) -> anyhow::Result<bool> {
    let b = Backend { kube, host };
    restore_inner(&b, "yolab-notes", "abc123", &cfg(), &rf::Choices::default()).await
}

#[tokio::test]
async fn a_restore_that_cannot_stop_a_deployment_or_hold_a_daemonset_touches_no_backup() {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(list("Deployment", vec![deployment("web", 2)])),
        )
        .mount(&server)
        .await;
    let host = FakeHost::new();
    assert!(restored_in_place(kube, host.clone()).await.is_err());
    assert!(host.calls().is_empty(), "{:?}", host.calls());

    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list("Deployment", vec![])))
        .mount(&server)
        .await;
    refused(&server, "/apis/apps/v1/namespaces/yolab-notes/daemonsets").await;
    let host = FakeHost::new();
    assert!(restored_in_place(kube, host.clone()).await.is_err());
    assert!(host.calls().is_empty(), "{:?}", host.calls());
}

#[tokio::test]
async fn a_restore_whose_settings_or_snapshot_time_cannot_be_read_goes_no_further() {
    let (_server, kube) = quiet_service(200).await;
    let host = FakeHost::new()
        .ok("restic unlock", "")
        .ok("restic restore", "")
        .fail("restic restore", "Fatal: no such snapshot");
    assert!(why(restored_in_place(kube, host).await).contains("no such snapshot"));

    let (_server, kube) = quiet_service(200).await;
    let host = FakeHost::new()
        .ok("restic unlock", "")
        .ok("restic restore", "")
        .fail("restic snapshots", "Fatal: wrong password");
    assert!(why(restored_in_place(kube, host).await).contains("wrong password"));
}

#[tokio::test]
async fn a_restore_with_a_broken_volume_list_or_no_view_of_the_volumes_is_an_error() {
    let (_server, kube) = quiet_service(200).await;
    let host = dated(vec![nameless_volume_catalog()]);
    assert!(why(restored_in_place(kube, host).await).contains("yolab-notes"));

    let (_server, kube) = quiet_service(500).await;
    assert!(restored_in_place(kube, dated(vec![notes_catalog()]))
        .await
        .is_err());
}

#[tokio::test]
async fn a_restore_whose_folder_backups_cannot_be_listed_is_an_error() {
    let (_server, kube) = quiet_service(200).await;
    let host = dated(vec![folder_catalog()]).fail(FOLDERS, "Fatal: wrong password");
    assert!(why(restored_in_place(kube, host).await).contains("wrong password"));
}

#[tokio::test]
async fn saved_settings_that_cannot_be_applied_fail_the_restore() {
    let (_server, kube) = quiet_service(200).await;
    let host = dated(vec![notes_catalog(), notes_yaml()]);
    assert!(why(restored_in_place(kube, host).await).starts_with("apply yolab-notes.yaml: "));
}

#[tokio::test]
async fn a_backup_without_readable_settings_restores_without_them() {
    let (server, kube) = quiet_service(200).await;
    assert!(
        !restored_in_place(kube.clone(), dated(vec![notes_catalog()]))
            .await
            .unwrap()
    );
    let host =
        dated(vec![notes_catalog()]).effect("restic restore", unreadable("yolab-notes.yaml"));
    assert!(!restored_in_place(kube, host).await.unwrap());
    assert!(crate::k8s::testing::patched(&server).await.is_empty());
}

#[tokio::test]
async fn a_daemonset_that_cannot_be_released_fails_the_restore() {
    let (server, kube) = api_server().await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list("Deployment", vec![])))
        .mount(&server)
        .await;
    daemonsets(&["engine"]).mount(&server).await;
    held("engine", true).expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/persistentvolumes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list("PersistentVolume", vec![])))
        .mount(&server)
        .await;
    assert!(restored_in_place(kube, dated(vec![notes_catalog()]))
        .await
        .is_err());
}

#[tokio::test]
async fn a_backup_payload_that_cannot_read_its_backup_is_an_error() {
    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .fail("restic restore", "Fatal: no such snapshot"),
    )
    .await;
    let payload = |b: Backend<FakeHost>| async move {
        why(backup_payload(&b, "yolab-notes", "abc123", Default::default()).await)
    };
    assert!(payload(b).await.contains("no such snapshot"));

    let host = dated(vec![notes_catalog()]).fail("restic restore", "Fatal: no such file");
    let (_server, b) = backed_up(host).await;
    assert!(payload(b).await.contains("no such file"));

    let host =
        dated(vec![notes_catalog()]).effect("restic restore", unreadable("yolab-notes.yaml"));
    let (_server, b) = backed_up(host).await;
    assert!(!payload(b).await.is_empty());

    let (_server, b) = backed_up(dated(vec![notes_catalog(), garbled_yaml()])).await;
    assert!(!payload(b).await.is_empty());

    let host = FakeHost::new()
        .ok("restic unlock", "")
        .restic_restores(vec![notes_catalog(), notes_yaml()])
        .fail("restic snapshots", "Fatal: wrong password");
    let (_server, b) = backed_up(host).await;
    assert!(payload(b).await.contains("wrong password"));

    let (_server, b) = backed_up(dated(vec![nameless_volume_catalog(), notes_yaml()])).await;
    assert!(payload(b).await.contains("yolab-notes"));

    let host = dated(vec![folder_catalog(), notes_yaml()]).fail(FOLDERS, "Fatal: wrong password");
    let (_server, b) = backed_up(host).await;
    assert!(payload(b).await.contains("wrong password"));
}

#[tokio::test]
async fn a_folder_store_never_written_means_the_chart_starts_that_volume_empty() {
    let host = dated(vec![folder_catalog(), notes_yaml()]).fail(
        FOLDERS,
        "Fatal: unable to open config file: Stat: The specified key does not exist.",
    );
    let (_server, b) = backed_up(host).await;
    let payload = backup_payload(&b, "yolab-notes", "abc123", Default::default())
        .await
        .unwrap();
    assert!(payload.volumes.is_empty());
}

#[tokio::test]
async fn a_restore_plan_that_cannot_read_its_backup_is_an_error() {
    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .fail("restic restore", "Fatal: no such snapshot"),
    )
    .await;
    assert!(why(restore_plan(&b, "yolab-notes", "abc123").await).contains("no such snapshot"));

    let (_server, b) = backed_up(dated(vec![nameless_volume_catalog()])).await;
    assert!(why(restore_plan(&b, "yolab-notes", "abc123").await).contains("yolab-notes"));

    let host = FakeHost::new()
        .ok("restic unlock", "")
        .restic_restores(vec![notes_catalog()])
        .fail("restic snapshots", "Fatal: wrong password");
    let (_server, b) = backed_up(host).await;
    assert!(why(restore_plan(&b, "yolab-notes", "abc123").await).contains("wrong password"));

    let (_server, b) = backed_up(dated(vec![folder_catalog()]).unstartable(FOLDERS)).await;
    assert!(restore_plan(&b, "yolab-notes", "abc123").await.is_err());

    let (_server, b) = backed_up(dated(vec![folder_catalog()]).ok(FOLDERS, "{")).await;
    assert!(restore_plan(&b, "yolab-notes", "abc123").await.is_err());
}

#[tokio::test]
async fn saved_settings_that_cannot_be_read_are_errors_not_empty_settings() {
    let host = FakeHost::new()
        .ok("restic unlock", "")
        .restic_restores(vec![notes_catalog()])
        .fail("restic restore", "Fatal: no such file");
    let (_server, b) = backed_up(host).await;
    assert!(
        why(definition_from_backup(&b, "yolab-notes", "abc123").await).contains("no such file")
    );

    let host =
        dated(vec![notes_catalog()]).effect("restic restore", unreadable("yolab-notes.yaml"));
    let (_server, b) = backed_up(host).await;
    assert!(definition_from_backup(&b, "yolab-notes", "abc123")
        .await
        .is_err());

    let (_server, b) = backed_up(dated(vec![notes_catalog(), garbled_yaml()])).await;
    assert!(definition_from_backup(&b, "yolab-notes", "abc123")
        .await
        .is_err());
}

#[tokio::test]
async fn a_kept_chart_that_cannot_be_read_is_an_error_not_no_chart() {
    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .fail("restic restore", "Fatal: no such snapshot"),
    )
    .await;
    assert!(why(kept_from_backup(&b, "yolab-notes", "abc123").await).contains("no such snapshot"));

    let host = dated(vec![]).effect("restic restore", unreadable("yolab-notes.yaml"));
    let (_server, b) = backed_up(host).await;
    assert!(kept_from_backup(&b, "yolab-notes", "abc123").await.is_err());

    let (_server, b) = backed_up(dated(vec![garbled_yaml()])).await;
    assert!(kept_from_backup(&b, "yolab-notes", "abc123").await.is_err());
}

#[tokio::test]
async fn older_copies_of_a_backup_that_cannot_be_read_are_an_error() {
    let host = FakeHost::new().fail("restic restore", "Fatal: no such snapshot");
    assert!(older_volume_copies(&host, &cfg(), "abc123", "yolab-notes")
        .await
        .is_err());
    let host = dated(vec![nameless_volume_catalog()]);
    assert!(older_volume_copies(&host, &cfg(), "abc123", "yolab-notes")
        .await
        .is_err());
}

#[tokio::test]
async fn versions_that_cannot_be_searched_or_listed_are_errors() {
    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .ok("restic snapshots", "[]")
            .fail("restic find", "Fatal: wrong password"),
    )
    .await;
    assert!(why(backup_versions(&b).await).ends_with("Fatal: wrong password"));

    let (_server, b) = backed_up(
        FakeHost::new()
            .ok("restic unlock", "")
            .unstartable("restic snapshots"),
    )
    .await;
    assert!(backup_versions(&b).await.is_err());
}

#[tokio::test]
async fn a_snapshot_time_that_cannot_be_asked_or_read_is_an_error() {
    let host = FakeHost::new().unstartable("restic snapshots");
    assert!(snapshot_time(&host, "repo", &cfg(), "snap-1")
        .await
        .is_err());
    let host = FakeHost::new().ok("restic snapshots", "{");
    assert!(why(snapshot_time(&host, "repo", &cfg(), "snap-1").await)
        .contains("restic's answer about snapshot snap-1"));
}

#[tokio::test]
async fn a_saved_file_that_cannot_be_fetched_read_or_parsed_is_an_error() {
    let host = FakeHost::new().unstartable("restic restore");
    assert!(
        extract_json_file(&host, "repo", &cfg(), "abc123", "catalog.json")
            .await
            .is_err()
    );
    let host = FakeHost::new()
        .ok("restic restore", "")
        .effect("restic restore", unreadable("catalog.json"));
    assert!(
        why(extract_json_file(&host, "repo", &cfg(), "abc123", "catalog.json").await)
            .starts_with("read catalog.json: ")
    );
    let host = FakeHost::new().restic_restores(vec![("catalog.json", "not json".to_string())]);
    assert!(
        why(extract_json_file(&host, "repo", &cfg(), "abc123", "catalog.json").await)
            .starts_with("parse catalog.json: ")
    );
}

fn payload_with(volumes: Vec<(CatalogPvc, Result<Restorable, String>)>) -> BackupPayload {
    BackupPayload {
        cfg: cfg(),
        snapshot_id: "abc123".into(),
        source_namespace: "yolab-notes".into(),
        app_id: "notes".into(),
        objects: json!({ "items": [{
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": { "name": "settings", "namespace": "yolab-notes2" }
        }] }),
        volumes,
        choices: rf::Choices::default(),
    }
}

#[tokio::test]
async fn an_install_that_cannot_record_its_fill_or_reapply_its_settings_is_an_error() {
    let (_server, kube) = api_server().await;
    let b = Backend {
        kube: kube.clone(),
        host: FakeHost::new(),
    };
    let payload = payload_with(vec![]);
    assert!(payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[])
        .await
        .is_err());
    assert!(why(payload.reapply(&kube).await).starts_with("apply yolab-notes.yaml: "));
}

#[tokio::test]
async fn an_install_whose_new_volume_cannot_be_created_fails_and_records_it() {
    let (server, kube) = api_server().await;
    let kept = crate::records::testing::kept(&server, "yolab-restores").await;
    let b = Backend {
        kube,
        host: FakeHost::new(),
    };
    let data = CatalogPvc {
        name: "data".into(),
        capacity: "1Gi".into(),
        snapshot: None,
        store: Some("services/yolab-notes".into()),
        folder: Some("notes".into()),
    };
    let from = Restorable {
        snapshot: "f1".into(),
        path: "/data/notes".into(),
    };
    let folder = crate::tree::Planned {
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
    };
    let payload = payload_with(vec![(data, Ok(from))]);
    assert!(payload
        .fill_volumes(&b, "yolab-notes2", "notes2", &[folder])
        .await
        .is_err());
    assert_eq!(kept.data("sets")[0]["state"], FAILED);
}

#[tokio::test]
async fn an_outcome_for_a_restore_no_longer_listed_or_unreachable_is_dropped_quietly() {
    let (server, kube) = api_server().await;
    crate::records::testing::records(&server, "yolab-restores", json!([])).await;
    crate::records::testing::accept_writes(&server).await;
    record_done(&kube, "rs-gone", &Ok(true)).await;
    assert!(crate::records::testing::written(&server, "yolab-restores")
        .await
        .is_empty());
    record_done(&crate::k8s::testing::unreachable(), "rs-gone", &Ok(true)).await;
    assert!(
        crate::backup::restore::list(&crate::k8s::testing::unreachable())
            .await
            .is_err()
    );
}

async fn driven_over(first: Value, then: ResponseTemplate) -> anyhow::Result<Tick> {
    let (server, kube) = api_server().await;
    let at = crate::records::testing::path_of("yolab-restores");
    Mock::given(method("GET"))
        .and(path(at.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::records::testing::stored(
                "yolab-restores",
                &json!([first]),
                "1",
            )),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(then)
        .mount(&server)
        .await;
    crate::records::testing::accept_writes(&server).await;
    let b = Backend {
        kube,
        host: FakeHost::new(),
    };
    let tick = drive_next(&b).await;
    assert!(crate::records::testing::written(&server, "yolab-restores")
        .await
        .is_empty());
    tick
}

#[tokio::test]
async fn a_restore_another_machine_is_running_is_left_to_it() {
    let running = json!({
        "id": "rs-busy-on-another-machine", "namespace": "yolab-notes",
        "started_at": "2026-09-28T00:00:00Z", "state": RUNNING,
        "owner": "node9", "heartbeat": Utc::now().to_rfc3339()
    });
    let tick = driven_over(running, ResponseTemplate::new(500))
        .await
        .unwrap();
    assert_eq!(
        tick,
        Tick::Idle("restore rs-busy-on-another-machine is running".into())
    );
}

#[tokio::test]
async fn a_restore_taken_by_another_machine_first_is_not_run_twice() {
    let queued = queued_record(json!({ "id": "rs-taken-elsewhere" }));
    let finished = queued_record(json!({ "id": "rs-taken-elsewhere", "state": SUCCEEDED }));
    let then = ResponseTemplate::new(200).set_body_json(crate::records::testing::stored(
        "yolab-restores",
        &json!([finished]),
        "2",
    ));
    assert_eq!(driven_over(queued, then).await.unwrap(), Tick::Done);
}

#[tokio::test]
async fn a_restore_that_cannot_be_taken_is_an_error() {
    let queued = queued_record(json!({ "id": "rs-untakeable" }));
    let then = ResponseTemplate::new(500).set_body_json(status(500, "InternalError"));
    assert!(driven_over(queued, then).await.is_err());
}

#[tokio::test]
async fn the_restores_controller_without_a_cluster_is_an_error() {
    let ctx = Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools::real(),
    };
    assert!(RestoresController.converge(&ctx).await.is_err());
}

#[test]
fn snapshots_without_an_id_or_a_readable_time_and_finds_without_a_snapshot_are_skipped() {
    let snapshots = json!([
        {"time": "2026-09-10T02:00:00Z"},
        {"id": "late", "time": "yesterday"},
        {"id": "aaaa1111", "time": "2026-09-10T02:00:00Z"},
    ]);
    let found = json!([
        {"matches": [{"path": "/s/yolab-a.yaml"}]},
        {"snapshot": "", "matches": [{"path": "/s/yolab-a.yaml"}]},
        {"snapshot": "aaaa", "matches": [{"path": "/s/yolab-a.yaml"}]},
    ]);
    let v = versions_by_app(&snapshots, &found);
    assert_eq!(v.keys().collect::<Vec<_>>(), ["yolab-a"]);
    assert_eq!(
        v["yolab-a"],
        vec![AppVersion {
            snapshot_id: "aaaa1111".into(),
            time: "2026-09-10T02:00:00Z".into(),
        }]
    );
}

#[test]
fn objects_without_an_items_list_hold_no_config_and_nothing_to_reapply() {
    assert_eq!(saved_config(&json!({})), None);
    let no_file = json!({"items": [
        {"kind": "Secret", "metadata": {"name": "yolab-config"}, "data": {}}
    ]});
    assert_eq!(saved_config(&no_file), None);
    assert!(objects_to_reapply(&json!({})).is_none());
}

#[test]
fn a_service_listed_without_volumes_has_none() {
    let catalog = json!({"services": [{"namespace": "yolab-a"}]});
    assert!(catalog_pvcs(&catalog, "yolab-a").unwrap().is_empty());
}
