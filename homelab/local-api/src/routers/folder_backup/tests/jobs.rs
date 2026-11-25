use super::*;
use crate::k8s::testing::api_server;
use crate::tree::testing::jobs_finish_saying;

fn cfg() -> BackupConfig {
    BackupConfig {
        access_key_id: "AKID".into(),
        secret_access_key: "SECRET".into(),
        bucket: "bucket-1".into(),
        endpoint: "https://s3.example".into(),
        restic_password: "pw".into(),
    }
}

fn into(to: &str) -> FolderRestore {
    FolderRestore {
        from: "/data/notes".into(),
        to: to.into(),
        replace: true,
    }
}

#[tokio::test]
async fn a_backup_job_answers_with_the_snapshot_it_saved_and_its_credentials_go() {
    let (server, kube) = api_server().await;
    jobs_finish_saying(&server, "copying\nYOLAB_SNAPSHOT abc123\n").await;
    let id = back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["notes".into()])
        .await
        .unwrap();
    assert_eq!(id, "abc123");
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .any(|r| r.method.as_str() == "DELETE" && r.url.path().contains("/secrets/yolab-backup-")));
}

#[tokio::test]
async fn a_backup_job_that_names_no_snapshot_is_an_error() {
    let (server, kube) = api_server().await;
    jobs_finish_saying(&server, "done\n").await;
    let err = back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["notes".into()])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        "back up the folders of yolab-notes: it finished but did not say which snapshot it saved (last words: done)"
    );
}

#[tokio::test]
async fn a_backup_needs_a_folder_and_credentials_the_cluster_takes() {
    let (_server, kube) = api_server().await;
    assert_eq!(
        back_up(&kube, &cfg(), "yolab-notes", "bk-1", &[])
            .await
            .unwrap_err()
            .to_string(),
        "yolab-notes has no folder in the file tree to back up"
    );
    assert!(
        back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["../etc".into()])
            .await
            .is_err()
    );
    assert!(
        back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["notes".into()])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_restore_job_must_say_everything_was_restored() {
    let (server, kube) = api_server().await;
    jobs_finish_saying(&server, "YOLAB_RESTORED\n").await;
    restore(&kube, &cfg(), "yolab-notes", "abc123", &[into("notes")])
        .await
        .unwrap();

    let (server, kube) = api_server().await;
    jobs_finish_saying(&server, "half way\n").await;
    assert_eq!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[into("notes")])
            .await
            .unwrap_err()
            .to_string(),
        "restore the folders of yolab-notes: it finished without saying everything was restored (last words: half way)"
    );
}

#[tokio::test]
async fn a_restore_of_nothing_bad_or_from_outside_a_backup_runs_no_job() {
    let (server, kube) = api_server().await;
    restore(&kube, &cfg(), "yolab-notes", "abc123", &[])
        .await
        .unwrap();
    assert_eq!(
        restore(&kube, &cfg(), "yolab-notes", "not-hex", &[into("notes")])
            .await
            .unwrap_err()
            .to_string(),
        "\"not-hex\" is not a snapshot id"
    );
    assert_eq!(
        restore(&kube, &cfg(), "yolab-notes", "", &[into("notes")])
            .await
            .unwrap_err()
            .to_string(),
        "\"\" is not a snapshot id"
    );
    assert!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[into("../etc")])
            .await
            .is_err()
    );
    let outside = FolderRestore {
        from: "/etc".into(),
        ..into("notes")
    };
    assert_eq!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[outside])
            .await
            .unwrap_err()
            .to_string(),
        "\"/etc\" is not a folder of a backup"
    );
    let two_lines = FolderRestore {
        from: "/data/a\nb".into(),
        ..into("notes")
    };
    assert!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[two_lines])
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn only_credentials_accepted(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("PATCH"))
        .and(wiremock::matchers::path_regex(
            "^/api/v1/namespaces/ceph-csi/secrets/[^/]+$",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "x" }
            })),
        )
        .mount(server)
        .await;
}

async fn pods_unlistable(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/v1/namespaces/ceph-csi/pods"))
        .respond_with(
            wiremock::ResponseTemplate::new(503)
                .set_body_json(crate::k8s::testing::status(503, "ServiceUnavailable")),
        )
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_job_the_cluster_will_not_run_fails_the_backup_and_the_restore() {
    let (server, kube) = api_server().await;
    only_credentials_accepted(&server).await;
    assert!(
        back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["notes".into()])
            .await
            .is_err()
    );
    assert!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[into("notes")])
            .await
            .is_err()
    );
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.method.as_str() == "POST"));
}

#[tokio::test]
async fn a_job_whose_last_words_cannot_be_read_fails_the_backup_and_the_restore() {
    let (server, kube) = api_server().await;
    pods_unlistable(&server).await;
    jobs_finish_saying(&server, "YOLAB_RESTORED\n").await;
    assert!(
        back_up(&kube, &cfg(), "yolab-notes", "bk-1", &["notes".into()])
            .await
            .is_err()
    );
    assert!(
        restore(&kube, &cfg(), "yolab-notes", "abc123", &[into("notes")])
            .await
            .is_err()
    );
}
