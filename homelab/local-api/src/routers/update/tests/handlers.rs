use super::*;
use crate::testkit::TestApi;
use axum::http::StatusCode;

fn put(path: &str) {
    let p = crate::storage::on_machine(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "").unwrap();
}

fn healing() {
    put("/var/lib/yolab/reset/config.toml.before");
}

fn api() -> TestApi {
    TestApi::provisioned().with_peer_token()
}

async fn until_logged(needle: &str) -> String {
    let log = crate::storage::on_machine("/var/log/yolab/rebuild.log");
    for _ in 0..1000 {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        tokio::task::yield_now().await;
    }
    std::fs::read_to_string(&log).unwrap_or_default()
}

#[tokio::test]
async fn the_channel_set_is_the_channel_read() {
    let api = api();
    let ch = r#"{"url":"github:someone/fork","ref":"dev"}"#;
    let res = api.send("PUT", "/api/update/channel", Some(ch)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        api.get("/api/update/channel").await.json(),
        serde_json::json!({ "url": "github:someone/fork", "ref": "dev" })
    );
}

#[tokio::test]
async fn a_channel_that_cannot_be_saved_is_a_server_error() {
    put("/var/lib/yolab/built");
    let res = api()
        .send(
            "PUT",
            "/api/update/channel",
            Some(r#"{"url":"github:a/b","ref":"main"}"#),
        )
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn an_update_streams_the_rebuild_it_launches() {
    let res = api().post("/api/update", "").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("data: [INFO] building"), "{}", res.body);
    assert!(
        res.body
            .contains("data: [ERROR] could not launch nixos-rebuild"),
        "{}",
        res.body
    );
    assert!(exclusive().is_some(), "the update let go of its lock");
}

#[tokio::test]
async fn an_update_waits_for_a_heal_and_for_another_update() {
    healing();
    let res = api().post("/api/update", "").await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert!(res.json()["error"]
        .as_str()
        .unwrap()
        .starts_with("a FORCE HEAL has prepared this machine"));

    std::fs::remove_file(crate::storage::on_machine(
        "/var/lib/yolab/reset/config.toml.before",
    ))
    .unwrap();
    let _running = exclusive().unwrap();
    let res = api().post("/api/update", "").await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(res.json()["error"], "Update already in progress");
}

#[tokio::test]
async fn a_triggered_update_writes_its_progress_into_the_rebuild_log() {
    let res = api().post("/api/update/trigger", "").await;
    assert_eq!(res.json(), serde_json::json!({ "status": "started" }));
    let log = until_logged("could not launch nixos-rebuild").await;
    assert!(log.contains("[trigger] [INFO] building"), "{log}");
    assert!(
        log.contains("[trigger] [ERROR] could not launch nixos-rebuild"),
        "{log}"
    );
}

#[tokio::test]
async fn a_trigger_waits_for_a_heal_and_for_another_update() {
    healing();
    let res = api().post("/api/update/trigger", "").await;
    assert!(res.json()["error"]
        .as_str()
        .unwrap()
        .starts_with("a FORCE HEAL has prepared this machine"));

    std::fs::remove_file(crate::storage::on_machine(
        "/var/lib/yolab/reset/config.toml.before",
    ))
    .unwrap();
    let _running = exclusive().unwrap();
    let res = api().post("/api/update/trigger", "").await;
    assert_eq!(
        res.json(),
        serde_json::json!({ "error": "already updating" })
    );
}

#[tokio::test]
async fn a_trigger_whose_log_cannot_be_written_still_updates() {
    put("/var/log/yolab");
    let res = api().post("/api/update/trigger", "").await;
    assert_eq!(res.json(), serde_json::json!({ "status": "started" }));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(exclusive().is_some());
}

#[tokio::test]
async fn updating_every_machine_with_none_to_reach_updates_this_one() {
    let res = TestApi::without_cluster()
        .with_peer_token()
        .post("/api/update/all", "")
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("data: [INFO] building"), "{}", res.body);

    let res = api().post("/api/update/all", "").await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn a_machine_that_cannot_be_updated_stops_the_roll_before_this_one() {
    let (server, kube) = crate::k8s::testing::api_server().await;
    crate::k8s::testing::serve(
        &server,
        "/api/v1/nodes",
        200,
        crate::k8s::testing::list(
            "Node",
            vec![serde_json::json!({
                "metadata": { "name": "node2" },
                "status": { "addresses": [{ "type": "InternalIP", "address": "fd00::9" }] },
            })],
        ),
    )
    .await;
    let res = TestApi::with_kube(kube)
        .with_peer_token()
        .post("/api/update/all", "")
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let body = res.json();
    assert_eq!(body["status"], "stopped");
    assert_eq!(body["stopped_at"], "fd00::9");
    assert!(exclusive().is_some(), "this machine was not updated");
}
