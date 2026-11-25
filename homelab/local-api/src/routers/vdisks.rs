use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::notify::Tunnel;
use crate::storage::settings;
use crate::vdisk::{platform, record, record::VdiskRecord};
use crate::AppState;

pub const BOX_TYPES: [&str; 4] = ["bx11", "bx21", "bx31", "bx41"];

#[derive(Serialize, Debug, PartialEq)]
pub struct VdiskInfo {
    pub id: i64,
    pub disk_id: String,
    pub node: String,
    pub box_type: String,
    pub size_gb: u64,
    pub ready: bool,
    pub state: &'static str,
    pub status: String,
}

#[derive(Deserialize)]
pub struct AddVdisk {
    pub node: String,
    pub box_type: String,
}

fn info(rec: &VdiskRecord, status: Option<&String>) -> VdiskInfo {
    VdiskInfo {
        id: rec.id,
        disk_id: rec.disk_id(),
        node: rec.node.clone(),
        box_type: rec.box_type.clone(),
        size_gb: rec.size_gb,
        ready: rec.is_provisioned(),
        state: state_of(rec, status.map(String::as_str)),
        status: status.cloned().unwrap_or_default(),
    }
}

pub(crate) fn state_of(rec: &VdiskRecord, status: Option<&str>) -> &'static str {
    use crate::vdisk::{CONNECTED, DRAINING, OFFLINE, PREPARING};
    match status {
        _ if !rec.is_provisioned() => "provisioning",
        Some(s) if s == OFFLINE => "offline",
        Some(s) if s == CONNECTED => "connected",
        Some(s) if s == PREPARING => "provisioning",
        Some(s) if s == DRAINING => "removing",
        Some(s) if !s.is_empty() => "error",
        _ => "connecting",
    }
}

pub(crate) fn refuse_add(box_type: &str, node: &str, known_nodes: &[String]) -> Option<String> {
    if !BOX_TYPES.contains(&box_type) {
        return Some(format!("box_type must be one of {}", BOX_TYPES.join(", ")));
    }
    if !known_nodes.iter().any(|n| n == node) {
        return Some(format!(
            "{node:?} is not a machine that stores data in this cluster"
        ));
    }
    None
}

pub async fn list_vdisks(State(s): State<AppState>) -> Result<Json<Vec<VdiskInfo>>> {
    let records = record::list(&s.host).await?;
    let statuses = record::statuses(&s.host).await?;
    let mut out: Vec<VdiskInfo> = records
        .iter()
        .map(|r| info(r, statuses.get(&r.id.to_string())))
        .collect();
    out.sort_by_key(|v| v.id);
    Ok(Json(out))
}

pub async fn list_offers(State(s): State<AppState>) -> Result<Json<Vec<platform::Offer>>> {
    let tunnel = Tunnel::read(&s.config.config_path)?;
    Ok(Json(platform::offers(&s.http, &tunnel).await?))
}

pub async fn add_vdisk(
    State(s): State<AppState>,
    Json(body): Json<AddVdisk>,
) -> std::result::Result<Json<VdiskInfo>, (StatusCode, Json<serde_json::Value>)> {
    let bad = |code: StatusCode, msg: String| (code, Json(serde_json::json!({ "error": msg })));
    let known: Vec<String> = settings::dump(&s.host, settings::DISK_STATUS)
        .await
        .map_err(|e| {
            bad(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the storage cluster did not answer: {e}"),
            )
        })?
        .into_keys()
        .collect();
    if let Some(why) = refuse_add(&body.box_type, &body.node, &known) {
        return Err(bad(StatusCode::BAD_REQUEST, why));
    }
    let tunnel = Tunnel::read(&s.config.config_path)
        .map_err(|e| bad(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let volume = platform::create(&s.http, &tunnel, &body.box_type)
        .await
        .map_err(|e| bad(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let rec = VdiskRecord {
        id: volume.id,
        node: body.node.clone(),
        box_type: volume.box_type.clone(),
        size_gb: volume.size_gb,
        host: if volume.is_ready() {
            volume.host.clone()
        } else {
            String::new()
        },
        username: if volume.is_ready() {
            volume.username.clone()
        } else {
            String::new()
        },
        password: if volume.is_ready() {
            volume.password.clone()
        } else {
            String::new()
        },
        osd_id: None,
    };
    record::save(&s.host, &rec).await.map_err(|e| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the Storage Box {} was bought but could not be recorded in the cluster ({e}) — \
                 delete it from the YoLab console so it is not billed",
                volume.id
            ),
        )
    })?;
    crate::routers::disks::record_switch(&s.host, &rec.node, &rec.disk_id(), "ON")
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    crate::runtime::wake(crate::vdisk::NAME);
    Ok(Json(info(&rec, None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_sizes_on_known_machines_can_be_bought() {
        let nodes = vec!["node1".to_string()];
        assert_eq!(refuse_add("bx11", "node1", &nodes), None);
        assert!(refuse_add("bx99", "node1", &nodes).is_some());
        assert!(refuse_add("bx11", "node7", &nodes).is_some());
    }

    #[test]
    fn the_page_gets_a_state_from_the_controllers_own_messages() {
        let mut rec = VdiskRecord {
            id: 2,
            node: "node1".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: String::new(),
            username: String::new(),
            password: String::new(),
            osd_id: None,
        };
        assert_eq!(state_of(&rec, None), "provisioning");
        rec.host = "h".into();
        rec.username = "u".into();
        rec.password = "p".into();
        assert_eq!(state_of(&rec, None), "connecting");
        assert_eq!(state_of(&rec, Some(crate::vdisk::OFFLINE)), "offline");
        assert_eq!(state_of(&rec, Some(crate::vdisk::CONNECTED)), "connected");
        assert_eq!(
            state_of(&rec, Some("Could not connect yet, retrying: x")),
            "error"
        );
        assert_eq!(state_of(&rec, Some(crate::vdisk::DRAINING)), "removing");
        assert_eq!(
            state_of(&rec, Some(crate::vdisk::PREPARING)),
            "provisioning"
        );
    }

    #[test]
    fn the_listing_never_carries_the_box_password() {
        let rec = VdiskRecord {
            id: 2,
            node: "node1".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: "u2.your-storagebox.de".into(),
            username: "u2".into(),
            password: "hunter2".into(),
            osd_id: None,
        };
        let json = serde_json::to_string(&info(&rec, Some(&"Connected".to_string()))).unwrap();
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("u2.your-storagebox.de"));
        assert!(json.contains("\"disk_id\":\"vdisk-2\""));
    }

    mod over_http {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::testkit::TestApi;
        use serde_json::json;
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn machines() -> String {
            json!({ "yolab/disk-status/node1": "{}" }).to_string()
        }

        async fn signed_in(host: FakeHost) -> TestApi {
            TestApi::with_kube_and_host(crate::k8s::testing::unreachable(), host)
                .login()
                .await
        }

        #[tokio::test]
        async fn the_page_lists_each_virtual_disk_in_order_with_its_state() {
            let records = json!({
                "yolab/vdisks/7": json!({ "id": 7, "node": "node1", "box_type": "bx11", "size_gb": 1000 }).to_string(),
                "yolab/vdisks/3": json!({
                    "id": 3, "node": "node1", "box_type": "bx21", "size_gb": 5000,
                    "host": "u1.example", "username": "u1", "password": "pw"
                }).to_string()
            })
            .to_string();
            let statuses = json!({ "yolab/vdisk-status/3": crate::vdisk::CONNECTED }).to_string();
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/vdisks/", &records)
                .ok("ceph config-key dump yolab/vdisk-status/", &statuses);
            let api = signed_in(host).await;

            let res = api.get("/api/vdisks").await;
            let list = res.json();
            assert_eq!(list[0]["id"], 3, "{}", res.body);
            assert_eq!(list[0]["state"], "connected");
            assert_eq!(list[1]["id"], 7);
            assert_eq!(list[1]["state"], "provisioning");
            assert!(!res.body.contains("pw"));
        }

        #[tokio::test]
        async fn the_list_is_an_error_while_ceph_cannot_be_read() {
            let api = signed_in(FakeHost::new().fail("ceph config-key dump", "timed out")).await;
            assert!(!api.get("/api/vdisks").await.status.is_success());
        }

        #[tokio::test]
        async fn prices_come_from_the_platform_and_need_a_connection() {
            let platform = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/storage/volume/offers"))
                .and(header("authorization", "Bearer cluster-tok"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                    { "box_type": "bx11", "size_gb": 1000, "monthly_cents": 400 }
                ])))
                .mount(&platform)
                .await;
            let api = TestApi::with_platform(crate::k8s::testing::unreachable(), &platform.uri())
                .login()
                .await;
            let res = api.get("/api/vdisks/offers").await;
            assert_eq!(res.json()[0]["monthly_cents"], 400, "{}", res.body);

            let offline = TestApi::provisioned().login().await;
            let res = offline.get("/api/vdisks/offers").await;
            assert!(
                res.json()["error"]
                    .as_str()
                    .unwrap()
                    .contains("not connected"),
                "{}",
                res.body
            );
        }

        async fn buy(api: &TestApi, body: &str) -> crate::testkit::Res {
            api.post("/api/vdisks", body).await
        }

        #[tokio::test]
        async fn a_purchase_is_refused_for_an_unknown_size_or_machine_or_a_silent_cluster() {
            let api = signed_in(
                FakeHost::new().ok("ceph config-key dump yolab/disk-status/", &machines()),
            )
            .await;
            let res = buy(&api, r#"{"node":"node1","box_type":"bx99"}"#).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST);
            assert!(res.json()["error"]
                .as_str()
                .unwrap()
                .contains("box_type must be one of"));
            let res = buy(&api, r#"{"node":"node9","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST);

            let silent = signed_in(FakeHost::new().fail("ceph config-key dump", "timed out")).await;
            let res = buy(&silent, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(res.json()["error"]
                .as_str()
                .unwrap()
                .starts_with("the storage cluster did not answer"));
        }

        async fn platform_selling(volume: serde_json::Value) -> MockServer {
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/storage/volume"))
                .and(body_json(json!({ "box_type": "bx11" })))
                .respond_with(ResponseTemplate::new(200).set_body_json(volume))
                .mount(&platform)
                .await;
            platform
        }

        async fn at(platform: &MockServer, host: FakeHost) -> TestApi {
            TestApi::with_platform_and_host(
                crate::k8s::testing::unreachable(),
                &platform.uri(),
                host,
            )
            .login()
            .await
        }

        #[tokio::test]
        async fn a_bought_disk_is_recorded_and_switched_on() {
            let platform = platform_selling(json!({
                "id": 12, "box_type": "bx11", "size_gb": 1000, "status": "active",
                "host": "u12.example", "username": "u12", "password": "pw"
            }))
            .await;
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/disk-status/", &machines())
                .ok("ceph config-key set", "");
            let api = at(&platform, host).await;

            let res = buy(&api, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.body);
            assert_eq!(res.json()["disk_id"], "vdisk-12");
            assert_eq!(res.json()["ready"], true);
            assert!(api.host.ran("ceph config-key set yolab/vdisks/12"));
            assert!(api
                .host
                .ran("ceph config-key set yolab/disks/node1--vdisk-12 ON"));
        }

        #[tokio::test]
        async fn a_disk_still_being_made_is_recorded_without_its_login() {
            let platform = platform_selling(json!({
                "id": 13, "box_type": "bx11", "size_gb": 1000, "status": "pending",
                "host": "u13.example", "username": "u13", "password": "pw"
            }))
            .await;
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/disk-status/", &machines())
                .ok("ceph config-key set", "");
            let api = at(&platform, host).await;

            let res = buy(&api, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.json()["ready"], false, "{}", res.body);
            assert!(!api.host.calls().iter().any(|c| c.contains("u13.example")));
        }

        #[tokio::test]
        async fn a_purchase_the_platform_refuses_is_a_bad_gateway() {
            let platform = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(402).set_body_string("no credit"))
                .mount(&platform)
                .await;
            let host = FakeHost::new().ok("ceph config-key dump yolab/disk-status/", &machines());
            let api = at(&platform, host).await;
            let res = buy(&api, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::BAD_GATEWAY);
            assert!(res.json()["error"].as_str().unwrap().contains("no credit"));
        }

        #[tokio::test]
        async fn a_bought_disk_that_cannot_be_recorded_says_to_delete_it_so_it_is_not_billed() {
            let platform = platform_selling(json!({
                "id": 14, "box_type": "bx11", "size_gb": 1000, "status": "active",
                "host": "h", "username": "u", "password": "p"
            }))
            .await;
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/disk-status/", &machines())
                .fail("ceph config-key set", "Error EACCES: access denied");
            let api = at(&platform, host).await;
            let res = buy(&api, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(res.json()["error"]
                .as_str()
                .unwrap()
                .contains("so it is not billed"));
        }

        #[tokio::test]
        async fn a_recorded_disk_whose_switch_cannot_be_saved_is_an_error() {
            let platform = platform_selling(json!({
                "id": 15, "box_type": "bx11", "size_gb": 1000, "status": "active",
                "host": "h", "username": "u", "password": "p"
            }))
            .await;
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/disk-status/", &machines())
                .ok("ceph config-key set yolab/vdisks/", "")
                .fail(
                    "ceph config-key set yolab/disks/",
                    "Error EACCES: access denied",
                );
            let api = at(&platform, host).await;
            let res = buy(&api, r#"{"node":"node1","box_type":"bx11"}"#).await;
            assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(res.json()["error"]
                .as_str()
                .unwrap()
                .contains("could not be saved"));
        }

        #[tokio::test]
        async fn the_list_is_an_error_while_the_disk_states_cannot_be_read() {
            let host = FakeHost::new()
                .ok("ceph config-key dump yolab/vdisks/", "{}")
                .fail("ceph config-key dump yolab/vdisk-status/", "timed out");
            let api = signed_in(host).await;
            assert!(!api.get("/api/vdisks").await.status.is_success());
        }

        #[tokio::test]
        async fn prices_need_a_tunnel_section_in_the_config() {
            let api = TestApi::with_config("[homelab]\nhostname = \"yolab\"\n").over_loopback();
            let res = api.get("/api/vdisks/offers").await;
            assert!(!res.status.is_success(), "{}", res.body);
        }

        #[tokio::test]
        async fn a_purchase_without_a_tunnel_section_is_unavailable_before_asking_the_platform() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("config.toml");
            std::fs::write(&file, "[homelab]\nhostname = \"yolab\"\n").unwrap();
            let config = std::sync::Arc::new(crate::config::Config::for_test(&file));
            let host = FakeHost::new().ok("ceph config-key dump yolab/disk-status/", &machines());
            let state = crate::AppState {
                config: config.clone(),
                auth: crate::auth::AuthState::isolated(config),
                kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
                http: crate::http::Client::new(),
                proxy_http: crate::http::Client::new(),
                host,
            };
            let body = AddVdisk {
                node: "node1".into(),
                box_type: "bx11".into(),
            };
            let (code, Json(error)) = add_vdisk(State(state), Json(body)).await.unwrap_err();
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                error["error"].as_str().unwrap().contains("[tunnel]"),
                "{error}"
            );
        }
    }
}
