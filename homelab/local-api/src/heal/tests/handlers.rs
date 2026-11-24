use super::*;
use crate::testkit::TestApi;

const FSID: &str = "11111111-2222-3333-4444-555555555555";

fn on_machine(path: &str, body: &str) {
    let p = crate::storage::on_machine(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn booted(id: &str) {
    on_machine("/proc/sys/kernel/random/boot_id", &format!("{id}\n"));
}

fn reset_state(heal_id: &str, phase: Value) {
    let mut state = json!({
        "heal_id": heal_id,
        "driver": "node1",
        "fsid": FSID,
        "server_addr": "",
    });
    state
        .as_object_mut()
        .unwrap()
        .extend(phase.as_object().unwrap().clone());
    on_machine("/var/lib/yolab/reset/state.json", &state.to_string());
}

fn api(host: FakeHost) -> TestApi {
    TestApi::with_kube_and_host(crate::k8s::testing::unreachable(), host).with_peer_token()
}

fn prepare_body(heal_id: &str) -> String {
    json!({ "heal_id": heal_id, "driver": "node1", "fsid": FSID, "server_addr": "" }).to_string()
}

#[tokio::test]
async fn the_status_shows_the_survey_with_this_machines_uptime() {
    on_machine("/proc/uptime", "123.45 6.7\n");
    let res = api(FakeHost::new()).get("/api/heal").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let body = res.json();
    assert_eq!(body["survey"]["uptime_secs"], 123);
    assert_eq!(body["heal"], Value::Null);
}

#[tokio::test]
async fn an_unreadable_uptime_counts_as_just_booted() {
    on_machine("/proc/uptime", "");
    let body = api(FakeHost::new()).get("/api/heal").await.json();
    assert_eq!(body["survey"]["uptime_secs"], 0);
}

#[tokio::test]
async fn an_unreadable_heal_record_makes_the_status_unavailable() {
    on_machine("/var/lib/yolab/heal.json", "not json");
    let res = api(FakeHost::new()).get("/api/heal").await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(res.json()["error"]
        .as_str()
        .unwrap()
        .contains("heal.json is unreadable"));
}

fn monitor_of(me: &str) -> FakeHost {
    on_machine("/etc/hostname", &format!("{me}\n"));
    FakeHost::new().ok(
        &format!("ceph daemon mon.{me} mon_status"),
        &mon_status_json("probing", &[(me, "::1")]),
    )
}

#[tokio::test]
async fn a_heal_is_started_on_what_the_page_showed() {
    let res = api(monitor_of("node1"))
        .post(
            "/api/heal",
            &json!({ "keep_machines": ["node1"], "remove_machines": [] }).to_string(),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let body = res.json();
    assert_eq!(body["members"], json!(["node1"]));
    assert_eq!(body["step"], "prepare");
    assert!(crate::storage::on_machine("/var/lib/yolab/heal.json").exists());
}

#[tokio::test]
async fn a_heal_that_cannot_start_is_a_conflict() {
    let res = api(FakeHost::new())
        .post(
            "/api/heal",
            &json!({ "keep_machines": [], "remove_machines": [] }).to_string(),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert!(res.json()["error"]
        .as_str()
        .unwrap()
        .starts_with("no list of this cluster's machines can be read"));
}

#[tokio::test]
async fn the_alert_view_lists_problems_and_the_machines_that_answer() {
    let host = monitor_of("node1");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "").unwrap();
    let view = current_view(
        &host,
        &crate::k8s::Kube::from_environment(),
        &crate::config::Config::for_test(&file),
    )
    .await;
    assert_eq!(view.problems, ["ceph_no_quorum"]);
    assert_eq!(view.answering, ["node1"]);
    assert_eq!(view.peer_addrs, Vec::<String>::new());
}

#[tokio::test]
async fn a_peer_answers_with_its_name_address_and_part_in_a_heal() {
    on_machine("/etc/hostname", "node2\n");
    booted("b1");
    let res = api(FakeHost::new()).get("/api/heal/peer").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.json(),
        json!({ "name": "node2", "addr": "::1", "reset": null })
    );

    reset_state("ab12", json!({ "phase": "prepared" }));
    let res = api(FakeHost::new()).get("/api/heal/peer").await;
    assert_eq!(res.json()["reset"]["phase"], "prepared");
}

#[tokio::test]
async fn a_peer_without_a_boot_id_or_with_a_garbled_reset_cannot_answer() {
    let res = api(FakeHost::new()).get("/api/heal/peer").await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(res.json()["error"]
        .as_str()
        .unwrap()
        .starts_with("read this boot's id: "));

    booted("");
    let res = api(FakeHost::new()).get("/api/heal/peer").await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        res.json()["error"],
        "this boot has no id — a heal cannot tell whether a restart happened"
    );

    booted("b1");
    on_machine("/var/lib/yolab/reset/state.json", "not json");
    let res = api(FakeHost::new()).get("/api/heal/peer").await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_peer_cannot_prepare_without_a_boot_id() {
    let res = api(FakeHost::new())
        .post("/api/heal/peer/prepare", &prepare_body("ab12"))
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_peer_already_prepared_for_this_heal_says_so() {
    booted("b1");
    reset_state("ab12", json!({ "phase": "prepared" }));
    let res = api(FakeHost::new())
        .post("/api/heal/peer/prepare", &prepare_body("ab12"))
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json()["phase"], "prepared");
}

#[tokio::test]
async fn a_peer_does_not_prepare_while_an_update_rebuilds_it() {
    booted("b1");
    let _update = crate::routers::update::exclusive().unwrap();
    let res = api(FakeHost::new())
        .post("/api/heal/peer/prepare", &prepare_body("ab12"))
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        res.json()["error"],
        "an update is rebuilding this machine's system"
    );
}

#[tokio::test]
async fn a_peer_refuses_a_prepare_it_cannot_accept() {
    booted("b1");
    let res = api(FakeHost::new())
        .post("/api/heal/peer/prepare", &prepare_body("not-hex"))
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        res.json()["error"],
        "heal id \"not-hex\" is not hexadecimal"
    );
}

#[tokio::test]
async fn a_peer_prepares_in_the_background_and_reports_how_it_went() {
    booted("b1");
    let api = api(FakeHost::new());
    let res = api
        .post("/api/heal/peer/prepare", &prepare_body("ab12"))
        .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.json(), json!({ "status": "preparing" }));
    let mut phase = Value::Null;
    for _ in 0..1000 {
        phase = api.get("/api/heal/peer").await.json()["reset"]["phase"].clone();
        if phase == "failed" {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(phase, "failed");
    assert!(crate::routers::update::exclusive().is_some());
}

#[tokio::test]
async fn a_peer_is_armed_only_for_the_heal_it_prepared() {
    let res = api(FakeHost::new())
        .post("/api/heal/peer/arm", r#"{"heal_id":"ab12"}"#)
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);

    booted("b1");
    let res = api(FakeHost::new())
        .post("/api/heal/peer/arm", r#"{"heal_id":"ab12"}"#)
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        res.json()["error"],
        "this machine was not prepared for heal ab12"
    );

    reset_state("ab12", json!({ "phase": "armed", "boot_id": "b1" }));
    let res = api(FakeHost::new())
        .post("/api/heal/peer/arm", r#"{"heal_id":"ab12"}"#)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json(), json!({ "status": "ok" }));
}

#[tokio::test]
async fn undoing_a_heal_this_peer_never_had_is_done() {
    booted("b1");
    let res = api(FakeHost::new())
        .post("/api/heal/peer/undo", r#"{"heal_id":"ab12"}"#)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn the_heal_controller_runs_on_every_machine_every_ten_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "").unwrap();
    let c = HealController {
        config: crate::config::Config::for_test(&file),
    };
    assert_eq!(c.name(), NAME);
    assert_eq!(c.scope(), Scope::Node);
    assert_eq!(c.interval(), TICK);
    let ctx = Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools::unreachable(),
    };
    assert_eq!(
        c.converge(&ctx).await.unwrap_err().to_string(),
        "read this boot's id"
    );
    booted("b1");
    assert_eq!(
        c.converge(&ctx).await.unwrap(),
        Tick::Idle("no heal is driven from this machine".into())
    );
}
