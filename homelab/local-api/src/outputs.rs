use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::appschema::{AppSchema, Format, OutputSpec};
use kube::Client;

const SECRET: &str = "yolab-outputs";
const SECRET_KEY: &str = "outputs.json";
const LOGS_TAIL: i64 = 2000;
pub const NONE: &str = "NONE";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub value: String,
    pub found_at: DateTime<Utc>,
}

pub type Remembered = BTreeMap<String, Found>;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ShownOutput {
    pub key: String,
    pub title: String,
    pub format: Format,
    pub value: String,
    pub found_at: DateTime<Utc>,
}

pub fn latest_matches<'a>(
    specs: &[OutputSpec],
    lines: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, String> {
    let mut latest = BTreeMap::new();
    for line in lines {
        for spec in specs {
            if let Some(value) = spec.find_in(line) {
                latest.insert(spec.key.clone(), value);
            }
        }
    }
    latest
}

pub fn remember(
    remembered: &mut Remembered,
    fresh: BTreeMap<String, String>,
    now: DateTime<Utc>,
) -> bool {
    let mut changed = false;
    for (key, value) in fresh {
        if value == NONE {
            changed |= remembered.remove(&key).is_some();
            continue;
        }
        if remembered.get(&key).is_some_and(|f| f.value == value) {
            continue;
        }
        remembered.insert(
            key,
            Found {
                value,
                found_at: now,
            },
        );
        changed = true;
    }
    changed
}

pub fn shown(specs: &[OutputSpec], remembered: &Remembered) -> Vec<ShownOutput> {
    specs
        .iter()
        .filter_map(|spec| {
            let found = remembered.get(&spec.key)?;
            Some(ShownOutput {
                key: spec.key.clone(),
                title: spec.title.clone(),
                format: spec.format,
                value: found.value.clone(),
                found_at: found.found_at,
            })
        })
        .collect()
}

pub fn parse_remembered(raw: Option<&str>) -> Remembered {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

fn stored_json(secret: &Value) -> Option<String> {
    use base64::Engine as _;
    let encoded = secret["data"][SECRET_KEY].as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    String::from_utf8(bytes).ok()
}

pub async fn read_remembered(client: &Client, ns: &str) -> anyhow::Result<Option<Remembered>> {
    let secret = crate::k8s::secret_data(client, ns, SECRET).await?;
    Ok(secret.map(|data| parse_remembered(data.get(SECRET_KEY).map(String::as_str))))
}

fn secret_manifest(ns: &str, remembered: &Remembered) -> anyhow::Result<Value> {
    let encoded = serde_json::to_string(remembered)?;
    Ok(crate::k8s::secret_manifest(
        SECRET,
        ns,
        &[(SECRET_KEY, encoded.as_str())],
        &[("yolab.io/managed", "true"), ("yolab.io/outputs", "true")],
    ))
}

pub async fn remembered_everywhere(client: &Client) -> BTreeMap<String, Remembered> {
    let labelled = kube::api::ListParams::default().labels("yolab.io/outputs=true");
    let Ok(secrets) = crate::k8s::list(client, "v1", "Secret", None, &labelled).await else {
        return BTreeMap::new();
    };
    secrets
        .iter()
        .filter_map(|secret| {
            let ns = secret["metadata"]["namespace"].as_str()?;
            Some((
                ns.to_string(),
                parse_remembered(stored_json(secret).as_deref()),
            ))
        })
        .collect()
}

async fn log_lines(client: &Client, ns: &str) -> anyhow::Result<Vec<String>> {
    use k8s_openapi::api::core::v1::Pod;
    let pods = crate::k8s::list(client, "v1", "Pod", Some(ns), &Default::default()).await?;
    let api = kube::Api::<Pod>::namespaced(client.clone(), ns);
    let mut lines = Vec::new();
    for pod in &pods {
        let Some(name) = pod["metadata"]["name"].as_str() else {
            continue;
        };
        let containers = pod["spec"]["initContainers"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(pod["spec"]["containers"].as_array().into_iter().flatten())
            .filter_map(|c| c["name"].as_str());
        for container in containers {
            let params = kube::api::LogParams {
                container: Some(container.to_string()),
                timestamps: true,
                tail_lines: Some(LOGS_TAIL),
                ..Default::default()
            };
            if let Ok(text) = api.logs(name, &params).await {
                lines.extend(text.lines().map(str::to_string));
            }
        }
    }
    Ok(in_time_order(lines))
}

fn in_time_order(mut lines: Vec<String>) -> Vec<String> {
    lines.sort_by(|a, b| {
        let stamp = |l: &String| l.split_once(' ').map(|(t, _)| t.to_string());
        stamp(a).cmp(&stamp(b))
    });
    lines
}

pub async fn rescan(client: &Client, ns: &str, app: &AppSchema) -> anyhow::Result<Remembered> {
    let looked_for = app.outputs();

    let stored = read_remembered(client, ns).await?;
    let existed = stored.is_some();
    let mut remembered = stored.unwrap_or_default();
    let mut changed = false;
    if !looked_for.is_empty() {
        let lines = log_lines(client, ns).await?;
        let fresh = latest_matches(&looked_for, lines.iter().map(String::as_str));
        changed |= remember(&mut remembered, fresh, Utc::now());
    }

    if changed || (!existed && !remembered.is_empty()) {
        crate::k8s::apply(client, &secret_manifest(ns, &remembered)?).await?;
    }
    Ok(remembered)
}

pub async fn rescan_all(
    client: &Client,
    catalog_dir: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let apps = crate::routers::apps::installed_apps(client).await?;
    let mut failed = Vec::new();
    for app in &apps {
        let schema = crate::routers::apps::installed_schema(
            client,
            &app.namespace,
            &app.app_id,
            catalog_dir,
        )
        .await;
        if let Err(e) = rescan(client, &app.namespace, &schema).await {
            tracing::debug!("{}: outputs could not be rescanned ({e})", app.namespace);
            failed.push(app.namespace.clone());
        }
    }
    Ok(failed)
}

pub struct OutputsController;

impl crate::runtime::Controller for OutputsController {
    fn name(&self) -> &'static str {
        "outputs"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let catalog_dir = crate::config::Config::from_env().catalog_dir();
        let failed = rescan_all(&crate::k8s::client().await?, &catalog_dir).await?;
        if failed.is_empty() {
            Ok(crate::runtime::Tick::Done)
        } else {
            Ok(crate::runtime::Tick::NotYet(format!(
                "could not rescan {}",
                failed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appschema::parse_output;
    use chrono::TimeZone;
    use serde_json::json;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 28, 12, minute, 0).unwrap()
    }

    fn logs(key: &str) -> OutputSpec {
        parse_output(key, &json!({}))
    }

    fn fresh(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_latest_matching_line_wins() {
        let found = latest_matches(
            &[logs("token")],
            [
                "YOLAB_OUTPUT token first",
                "noise",
                "YOLAB_OUTPUT token second",
            ],
        );
        assert_eq!(found.get("token").map(String::as_str), Some("second"));
    }

    #[test]
    fn a_value_found_once_is_kept_when_the_logs_no_longer_show_it() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("password", "abc")]), at(1));
        let changed = remember(&mut remembered, BTreeMap::new(), at(2));
        assert!(!changed);
        assert_eq!(remembered["password"].value, "abc");
    }

    #[test]
    fn a_new_value_replaces_the_remembered_one() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("password", "abc")]), at(1));
        assert!(remember(
            &mut remembered,
            fresh(&[("password", "xyz")]),
            at(2)
        ));
        assert_eq!(
            remembered["password"],
            Found {
                value: "xyz".into(),
                found_at: at(2)
            }
        );
    }

    #[test]
    fn seeing_the_same_value_again_does_not_count_as_a_change() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("password", "abc")]), at(1));
        assert!(!remember(
            &mut remembered,
            fresh(&[("password", "abc")]),
            at(5)
        ));
        assert_eq!(
            remembered["password"].found_at,
            at(1),
            "it was found at 12:01, not re-found"
        );
    }

    #[test]
    fn an_output_never_printed_is_not_shown() {
        assert!(shown(&[logs("onion_address")], &Remembered::new()).is_empty());
    }

    #[test]
    fn a_printed_output_is_shown_with_its_declared_title_and_format() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("password", "abc")]), at(1));
        let spec = parse_output(
            "password",
            &json!({ "title": "Admin password", "format": "secret" }),
        );
        let rows = shown(&[spec], &remembered);
        assert_eq!(rows[0].title, "Admin password");
        assert_eq!(rows[0].format, Format::Secret);
        assert_eq!(rows[0].value, "abc");
        assert_eq!(rows[0].found_at, at(1));
    }

    #[test]
    fn a_printed_line_the_chart_does_not_declare_is_not_shown() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("tunnel_id", "t1")]), at(1));
        assert!(shown(&[logs("url")], &remembered).is_empty());
    }

    #[test]
    fn none_forgets_a_remembered_value() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("tor_url", "http://x.onion/")]), at(1));
        assert!(remember(&mut remembered, fresh(&[("tor_url", NONE)]), at(2)));
        assert!(remembered.is_empty());
        assert!(shown(&[logs("tor_url")], &remembered).is_empty());
    }

    #[test]
    fn none_for_a_value_never_seen_changes_nothing() {
        let mut remembered = Remembered::new();
        assert!(!remember(&mut remembered, fresh(&[("tor_url", NONE)]), at(1)));
    }

    #[test]
    fn a_value_printed_after_none_shows_again() {
        let found = latest_matches(
            &[logs("tor_url")],
            [
                "YOLAB_OUTPUT tor_url http://old.onion/",
                "YOLAB_OUTPUT tor_url NONE",
                "YOLAB_OUTPUT tor_url http://new.onion/",
            ],
        );
        assert_eq!(found["tor_url"], "http://new.onion/");
        let hidden = latest_matches(
            &[logs("tor_url")],
            ["YOLAB_OUTPUT tor_url http://old.onion/", "YOLAB_OUTPUT tor_url NONE"],
        );
        assert_eq!(hidden["tor_url"], NONE);
    }

    #[test]
    fn lines_from_several_containers_are_put_back_in_time_order() {
        let lines = in_time_order(vec![
            "2026-09-28T12:05:00.000000000Z YOLAB_OUTPUT token from-the-restarted-pod".into(),
            "2026-09-28T12:01:00.000000000Z YOLAB_OUTPUT token from-the-first-pod".into(),
        ]);
        let found = latest_matches(&[logs("token")], lines.iter().map(String::as_str));
        assert_eq!(found["token"], "from-the-restarted-pod");
    }

    #[test]
    fn an_unreadable_store_of_outputs_counts_as_empty() {
        assert!(parse_remembered(Some("not json")).is_empty());
        assert!(parse_remembered(None).is_empty());
    }

    mod against_a_cluster {
        use super::*;
        use crate::k8s::testing::{
            accept_patches, api_server, asked, list, patched, secret_with, serve, serve_logs,
            status,
        };
        use wiremock::MockServer;

        const NS: &str = "yolab-files-ab12";
        const NS_PATH: &str = "/api/v1/namespaces/yolab-files-ab12";
        const SECRET_PATH: &str = "/api/v1/namespaces/yolab-files-ab12/secrets/yolab-outputs";
        const POD_PATH: &str = "/api/v1/namespaces/yolab-files-ab12/pods/gateway-7f";

        fn app() -> AppSchema {
            AppSchema::new(json!({ "properties": {
                "outputs": { "properties": {
                    "password": { "title": "Admin password", "format": "secret" },
                    "file_explorer_password": {
                        "title": "File explorer password", "format": "secret"
                    }
                }}
            }}))
        }

        async fn pods(server: &MockServer) {
            serve(
                server,
                &format!("{NS_PATH}/pods"),
                200,
                list(
                    "Pod",
                    vec![json!({
                        "metadata": { "name": "gateway-7f" },
                        "spec": {
                            "initContainers": [{ "name": "file-explorer-init" }],
                            "containers": [{ "name": "caddy" }]
                        }
                    })],
                ),
            )
            .await;
        }

        fn stored(remembered: &Remembered) -> Value {
            secret_with("outputs.json", &serde_json::to_string(remembered).unwrap())
        }

        fn remembered(key: &str, value: &str) -> Remembered {
            Remembered::from([(
                key.to_string(),
                Found {
                    value: value.into(),
                    found_at: at(1),
                },
            )])
        }

        fn printed(value: &str) -> String {
            format!("2026-09-28T12:03:00.000000000Z YOLAB_OUTPUT file_explorer_password {value}")
        }

        fn saved(patch: &Value) -> Remembered {
            parse_remembered(patch["stringData"]["outputs.json"].as_str())
        }

        async fn nothing_stored(server: &MockServer) {
            serve(server, SECRET_PATH, 404, status(404, "NotFound")).await;
        }

        #[tokio::test]
        async fn a_value_printed_by_an_init_container_is_found_and_saved() {
            let (server, kube) = api_server().await;
            nothing_stored(&server).await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", &printed("s3cret")).await;
            serve_logs(&server, POD_PATH, "caddy", "").await;
            accept_patches(&server).await;

            let found = rescan(&kube, NS, &app()).await.unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
            let writes = patched(&server).await;
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0]["metadata"]["name"], "yolab-outputs");
            assert_eq!(saved(&writes[0])["file_explorer_password"].value, "s3cret");
        }

        #[tokio::test]
        async fn nothing_is_written_when_nothing_new_was_found() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                SECRET_PATH,
                200,
                stored(&remembered("file_explorer_password", "s3cret")),
            )
            .await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", &printed("s3cret")).await;
            serve_logs(&server, POD_PATH, "caddy", "").await;

            rescan(&kube, NS, &app()).await.unwrap();

            assert!(patched(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_value_is_kept_after_the_pod_restarts_and_its_logs_are_gone() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                SECRET_PATH,
                200,
                stored(&remembered("file_explorer_password", "s3cret")),
            )
            .await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", "").await;
            serve_logs(&server, POD_PATH, "caddy", "").await;

            let found = rescan(&kube, NS, &app()).await.unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
            assert!(patched(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_newer_value_replaces_the_remembered_one() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                SECRET_PATH,
                200,
                stored(&remembered("file_explorer_password", "old")),
            )
            .await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", &printed("new")).await;
            serve_logs(&server, POD_PATH, "caddy", "").await;
            accept_patches(&server).await;

            let found = rescan(&kube, NS, &app()).await.unwrap();

            assert_eq!(found["file_explorer_password"].value, "new");
            assert_eq!(
                saved(&patched(&server).await[0])["file_explorer_password"].value,
                "new"
            );
        }

        #[tokio::test]
        async fn none_in_the_logs_forgets_the_remembered_value() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                SECRET_PATH,
                200,
                stored(&remembered("file_explorer_password", "s3cret")),
            )
            .await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", &printed(NONE)).await;
            serve_logs(&server, POD_PATH, "caddy", "").await;
            accept_patches(&server).await;

            let found = rescan(&kube, NS, &app()).await.unwrap();

            assert!(found.is_empty());
            assert!(saved(&patched(&server).await[0]).is_empty());
        }

        #[tokio::test]
        async fn logs_are_not_read_when_nothing_is_expected_from_them() {
            let (server, kube) = api_server().await;
            nothing_stored(&server).await;
            let nothing_declared = AppSchema::new(json!({}));

            rescan(&kube, NS, &nothing_declared).await.unwrap();

            assert!(!asked(&server, "/pods").await);
            assert!(
                patched(&server).await.is_empty(),
                "nothing found, nothing to save"
            );
        }

        #[tokio::test]
        async fn one_container_whose_logs_cannot_be_read_does_not_hide_the_others() {
            let (server, kube) = api_server().await;
            nothing_stored(&server).await;
            pods(&server).await;
            serve_logs(&server, POD_PATH, "file-explorer-init", &printed("s3cret")).await;
            accept_patches(&server).await;

            let found = rescan(&kube, NS, &app()).await.unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
        }

        #[tokio::test]
        async fn an_unreachable_cluster_is_an_error_not_an_empty_answer() {
            let (server, kube) = api_server().await;
            serve(&server, SECRET_PATH, 503, status(503, "ServiceUnavailable")).await;

            let result = rescan(&kube, NS, &app()).await;

            assert!(result.is_err());
            assert!(
                patched(&server).await.is_empty(),
                "never overwrite what we could not read"
            );
        }

        #[tokio::test]
        async fn every_apps_remembered_values_are_read_in_one_call() {
            let (server, kube) = api_server().await;
            let mut a = stored(&remembered("url", "https://a"));
            a["metadata"] = json!({ "name": "yolab-outputs", "namespace": "yolab-a" });
            serve(
                &server,
                "/api/v1/secrets",
                200,
                list(
                    "Secret",
                    vec![
                        a,
                        json!({ "metadata": { "name": "yolab-outputs", "namespace": "yolab-b" },
                                "data": { "outputs.json": "not base64!" } }),
                    ],
                ),
            )
            .await;

            let everywhere = remembered_everywhere(&kube).await;

            assert_eq!(everywhere["yolab-a"]["url"].value, "https://a");
            assert!(
                everywhere["yolab-b"].is_empty(),
                "an unreadable one counts as nothing found"
            );
        }

        #[tokio::test]
        async fn the_listing_shows_nothing_remembered_when_the_cluster_cannot_be_read() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                "/api/v1/secrets",
                503,
                status(503, "ServiceUnavailable"),
            )
            .await;
            assert!(remembered_everywhere(&kube).await.is_empty());
        }

        fn catalog_with_files_app() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            let chart = dir.path().join("files");
            std::fs::create_dir_all(&chart).unwrap();
            std::fs::write(
                chart.join("Chart.yaml"),
                "apiVersion: v2\nname: files\nversion: 0.1.0\n",
            )
            .unwrap();
            std::fs::write(
                chart.join("values.schema.json"),
                json!({ "properties": { "outputs": { "properties": {
                    "token": { "title": "Token" }
                }}}})
                .to_string(),
            )
            .unwrap();
            dir
        }

        fn namespace(name: &str, phase: &str) -> Value {
            json!({
                "metadata": { "name": name, "annotations": { "yolab.io/app-id": "files" } },
                "status": { "phase": phase }
            })
        }

        #[tokio::test]
        async fn every_running_app_is_rescanned_and_failures_are_named() {
            let catalog = catalog_with_files_app();
            let (server, kube) = api_server().await;
            serve(
                &server,
                "/api/v1/namespaces",
                200,
                list(
                    "Namespace",
                    vec![
                        namespace("yolab-good", "Active"),
                        namespace("yolab-broken", "Active"),
                        namespace("yolab-leaving", "Terminating"),
                    ],
                ),
            )
            .await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-good/secrets/yolab-outputs",
                404,
                status(404, "NotFound"),
            )
            .await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-good/pods",
                200,
                list("Pod", vec![json!({ "metadata": { "name": "p" }, "spec": { "containers": [{ "name": "app" }] } })]),
            )
            .await;
            serve_logs(
                &server,
                "/api/v1/namespaces/yolab-good/pods/p",
                "app",
                "2026-09-28T12:00:00Z YOLAB_OUTPUT token t0k",
            )
            .await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-broken/secrets/yolab-outputs",
                503,
                status(503, "ServiceUnavailable"),
            )
            .await;
            accept_patches(&server).await;

            let failed = rescan_all(&kube, catalog.path()).await.unwrap();

            assert_eq!(failed, vec!["yolab-broken".to_string()]);
            assert_eq!(saved(&patched(&server).await[0])["token"].value, "t0k");
            assert!(
                !asked(&server, "yolab-leaving").await,
                "an app being removed is left alone"
            );
        }
    }
}
