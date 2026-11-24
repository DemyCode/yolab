use super::*;
use crate::k8s::testing::{api_server, serve, status};
use crate::testkit::TestApi;
use axum::http::StatusCode;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPOS: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-chart-repos";
const STATS: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-github-stats";

fn chart_with(root: &std::path::Path, repo: &str, name: &str, extra: &str) {
    let dir = root.join(repo).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Chart.yaml"),
        format!("apiVersion: v2\nname: {name}\nversion: 1.0.0\n{extra}"),
    )
    .unwrap();
}

async fn a_cluster_with_stars() -> (MockServer, kube::Client) {
    let (server, kube) = api_server().await;
    serve(&server, REPOS, 404, status(404, "NotFound")).await;
    serve(
        &server,
        STATS,
        200,
        json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": { "name": "yolab-github-stats" },
            "data": {
                "someone__notes": json!({"stars": 42, "pushed_at": "2026-11-01", "fetched_at": 1}).to_string(),
                "someone__gone": json!({"stars": 0, "archived": true, "fetched_at": 1}).to_string(),
            },
        }),
    )
    .await;
    (server, kube)
}

fn a_catalog_on_disk(root: &std::path::Path) {
    chart_with(
        root,
        "official",
        "notes",
        "annotations:\n  yolab.io/github: someone/notes\n",
    );
    chart_with(
        root,
        "official",
        "gone",
        "annotations:\n  yolab.io/github: someone/gone\n",
    );
    chart_with(root, "official", "yolab-common", "type: library\n");
    chart_with(
        root,
        "official",
        "hidden",
        "annotations:\n  yolab.io/disabled: \"broken upstream\"\n",
    );
    std::fs::create_dir_all(root.join("official").join("not-a-chart")).unwrap();
    std::fs::write(root.join("official").join("stray-file"), "").unwrap();
    chart_with(root, "custom", "notes", "");
    chart_with(root, "custom", "Zeta", "");
}

#[tokio::test]
async fn the_catalog_lists_each_chart_once_by_name_with_its_stars() {
    let cache = crate::charts::testing::cache_in_temp();
    a_catalog_on_disk(cache.dir.path());
    let (_server, kube) = a_cluster_with_stars().await;
    let res = TestApi::with_kube(kube)
        .with_peer_token()
        .get("/api/apps/catalog")
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let apps = res.json();
    let names: Vec<&str> = apps
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["gone", "notes", "Zeta"]);
    let notes = &apps[1];
    assert_eq!(notes["repo"], "custom");
    assert_eq!(apps[0]["stars"], Value::Null);
}

#[tokio::test]
async fn a_known_repository_carries_its_stars_and_last_push() {
    let cache = crate::charts::testing::cache_in_temp();
    chart_with(
        cache.dir.path(),
        "official",
        "notes",
        "annotations:\n  yolab.io/github: someone/notes\n",
    );
    let (_server, kube) = a_cluster_with_stars().await;
    let res = TestApi::with_kube(kube)
        .with_peer_token()
        .post("/api/apps/catalog/notes/refresh", "")
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let body = res.json();
    assert_eq!(body["refreshed"], false);
    assert!(!body["note"].as_str().unwrap().is_empty());
    assert_eq!(body["app"]["stars"], 42);
    assert_eq!(body["app"]["pushed_at"], "2026-11-01");
}

#[tokio::test]
async fn refreshing_a_chart_that_is_nowhere_finds_no_app() {
    let _cache = crate::charts::testing::cache_in_temp();
    let (_server, kube) = a_cluster_with_stars().await;
    let res = TestApi::with_kube(kube)
        .with_peer_token()
        .post("/api/apps/catalog/missing/refresh", "")
        .await;
    assert_eq!(res.json()["app"], Value::Null);
}

#[tokio::test]
async fn catalog_pages_need_the_cluster() {
    let api = TestApi::without_cluster().with_peer_token();
    for (verb, uri) in [
        ("GET", "/api/apps/catalog"),
        ("POST", "/api/apps/catalog/notes/refresh"),
        ("POST", "/api/apps/repos/sync"),
    ] {
        let res = api.send(verb, uri, Some("")).await;
        assert!(res.status.is_server_error(), "{verb} {uri}: {}", res.status);
    }
    let res = api
        .post(
            "/api/apps/repos",
            r#"{"name":"mine","url":"https://charts.example/catalog.yaml"}"#,
        )
        .await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    let res = api.send("DELETE", "/api/apps/repos/mine", None).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
}

async fn repos_accept_changes(server: &MockServer) {
    Mock::given(method("PATCH"))
        .and(path(REPOS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "yolab-chart-repos" }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn an_added_repository_is_synced_at_once_and_a_failed_sync_is_reported() {
    let _cache = crate::charts::testing::cache_in_temp();
    let (server, kube) = api_server().await;
    repos_accept_changes(&server).await;
    serve(
        &server,
        REPOS,
        200,
        json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": { "name": "yolab-chart-repos" },
            "data": { "mine": format!("http://[::1]:{}/catalog.yaml", crate::testkit::closed_port()) },
        }),
    )
    .await;
    let api = TestApi::with_kube(kube).with_peer_token();
    let res = api
        .post(
            "/api/apps/repos",
            r#"{"name":"mine","url":"https://charts.example/catalog.yaml"}"#,
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_GATEWAY, "{}", res.body);
    assert!(
        res.body.starts_with("added, but sync failed: "),
        "{}",
        res.body
    );

    let res = api
        .post(
            "/api/apps/repos",
            r#"{"name":"official","url":"https://charts.example/catalog.yaml"}"#,
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = api.post("/api/apps/repos/sync", "").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["mine"]["ok"], false);
}

#[tokio::test]
async fn an_added_repository_the_list_does_not_show_yet_is_not_synced() {
    let _cache = crate::charts::testing::cache_in_temp();
    let (server, kube) = api_server().await;
    repos_accept_changes(&server).await;
    serve(&server, REPOS, 404, status(404, "NotFound")).await;
    let res = TestApi::with_kube(kube)
        .with_peer_token()
        .post(
            "/api/apps/repos",
            r#"{"name":"mine","url":"https://charts.example/catalog.yaml"}"#,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.json(), json!({ "ok": true }));
}

#[tokio::test]
async fn a_repository_is_removed_but_never_the_official_one() {
    let _cache = crate::charts::testing::cache_in_temp();
    let (server, kube) = api_server().await;
    repos_accept_changes(&server).await;
    let api = TestApi::with_kube(kube).with_peer_token();
    let res = api.send("DELETE", "/api/apps/repos/mine", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = api.send("DELETE", "/api/apps/repos/official", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.body, "the official catalog cannot be removed");
}

#[tokio::test]
async fn the_domain_comes_from_the_tunnels_dns_address() {
    let api = TestApi::with_config(&crate::testkit::config_with_tunnel(
        "dns_url = \"https://dns.yolab.example\"\n",
    ))
    .with_peer_token();
    assert_eq!(
        api.get("/api/tunnel/domain").await.json(),
        json!({ "domain": "yolab.example" })
    );
    let bare = TestApi::provisioned().with_peer_token();
    assert_eq!(
        bare.get("/api/tunnel/domain").await.json(),
        json!({ "domain": "" })
    );
    let none = TestApi::with_config("[homelab]\nhostname = \"yolab\"\n").with_peer_token();
    assert!(none
        .get("/api/tunnel/domain")
        .await
        .status
        .is_server_error());
}

#[test]
fn values_carry_the_platform_address_when_the_tunnel_has_one() {
    let mut tunnel = toml::Table::new();
    tunnel.insert(
        "platform_api_url".into(),
        toml::Value::String("https://api.example".into()),
    );
    let values: Value =
        serde_json::from_str(&build_values(&Default::default(), &tunnel, "notes")).unwrap();
    assert_eq!(values["yolab"]["platformApiUrl"], "https://api.example");
}

#[test]
fn a_chart_without_a_display_name_shows_its_chart_name() {
    let dir = tempfile::tempdir().unwrap();
    chart_with(dir.path(), "r", "plain", "");
    chart_with(
        dir.path(),
        "r",
        "named",
        "annotations:\n  yolab.io/display-name: Named Thing\n",
    );
    assert_eq!(
        read_chart(&dir.path().join("r/plain"))
            .unwrap()
            .display_name(),
        "plain"
    );
    assert_eq!(
        read_chart(&dir.path().join("r/named"))
            .unwrap()
            .display_name(),
        "Named Thing"
    );
}
