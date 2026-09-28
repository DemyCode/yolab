use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::appschema::{AppSchema, Format, OutputSpec, Source};
use crate::host::{Host, HOST};

const SECRET: &str = "yolab-outputs";
const SECRET_KEY: &str = "outputs.json";
const LEGACY_ANNOTATION: &str = "yolab.io/outputs";
const LOGS_TAIL: &str = "--tail=2000";

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
    pub value: Option<String>,
    pub found_at: Option<DateTime<Utc>>,
    pub from_config: bool,
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

pub fn from_legacy_annotation(ann: &Map<String, Value>, since: DateTime<Utc>) -> Remembered {
    let Some(raw) = ann.get(LEGACY_ANNOTATION).and_then(Value::as_str) else {
        return Remembered::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return Remembered::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let key = item["key"].as_str()?;
            let value = item["value"].as_str().filter(|v| !v.is_empty())?;
            Some((
                key.to_string(),
                Found {
                    value: value.to_string(),
                    found_at: since,
                },
            ))
        })
        .collect()
}

pub fn shown(
    specs: &[OutputSpec],
    remembered: &Remembered,
    config: &Map<String, Value>,
) -> Vec<ShownOutput> {
    specs
        .iter()
        .map(|spec| {
            let (value, found_at, from_config) = match &spec.source {
                Source::Logs(_) => match remembered.get(&spec.key) {
                    Some(found) => (Some(found.value.clone()), Some(found.found_at), false),
                    None => (None, None, false),
                },
                Source::Config(field) => (config_text(config.get(field)), None, true),
            };
            ShownOutput {
                key: spec.key.clone(),
                title: spec.title.clone(),
                format: spec.format,
                value,
                found_at,
                from_config,
            }
        })
        .collect()
}

fn config_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
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

pub async fn read_remembered<H: Host>(host: &H, ns: &str) -> anyhow::Result<Option<Remembered>> {
    let secret = host
        .kubectl_get_opt(&["get", "secret", SECRET, "-n", ns, "-o", "json"])
        .await?;
    Ok(secret.map(|s| parse_remembered(stored_json(&s).as_deref())))
}

fn secret_manifest(ns: &str, remembered: &Remembered) -> anyhow::Result<String> {
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "Opaque",
        "metadata": {
            "name": SECRET,
            "namespace": ns,
            "labels": { "yolab.io/managed": "true", "yolab.io/outputs": "true" }
        },
        "stringData": { SECRET_KEY: serde_json::to_string(remembered)? }
    });
    Ok(manifest.to_string())
}

pub async fn remembered_everywhere<H: Host>(host: &H) -> BTreeMap<String, Remembered> {
    let listed = host
        .kubectl_json(&[
            "get",
            "secrets",
            "--all-namespaces",
            "-l",
            "yolab.io/outputs=true",
            "-o",
            "json",
        ])
        .await;
    let Ok(listed) = listed else {
        return BTreeMap::new();
    };
    listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|secret| {
            let ns = secret["metadata"]["namespace"].as_str()?;
            Some((
                ns.to_string(),
                parse_remembered(stored_json(secret).as_deref()),
            ))
        })
        .collect()
}

async fn log_lines<H: Host>(host: &H, ns: &str) -> anyhow::Result<Vec<String>> {
    let pods = host
        .kubectl_json(&["get", "pods", "-n", ns, "-o", "json"])
        .await?;
    let mut lines = Vec::new();
    for pod in pods["items"].as_array().into_iter().flatten() {
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
            let args = [
                "logs",
                "-n",
                ns,
                name,
                "-c",
                container,
                "--timestamps",
                LOGS_TAIL,
            ];
            if let Ok(text) = host.kubectl(&args).await {
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

pub async fn rescan<H: Host>(
    host: &H,
    ns: &str,
    app: &AppSchema,
    settings: &Map<String, Value>,
    ann: &Map<String, Value>,
) -> anyhow::Result<Remembered> {
    let looked_for: Vec<OutputSpec> = app
        .applicable_outputs(settings)
        .into_iter()
        .filter(|o| matches!(o.source, Source::Logs(_)))
        .collect();

    let stored = read_remembered(host, ns).await?;
    let existed = stored.is_some();
    let mut remembered = stored.unwrap_or_default();
    let mut changed = false;
    for (key, found) in from_legacy_annotation(ann, Utc::now()) {
        if !remembered.contains_key(&key) {
            remembered.insert(key, found);
            changed = true;
        }
    }

    if !looked_for.is_empty() {
        let lines = log_lines(host, ns).await?;
        let fresh = latest_matches(&looked_for, lines.iter().map(String::as_str));
        changed |= remember(&mut remembered, fresh, Utc::now());
    }

    if changed || (!existed && !remembered.is_empty()) {
        host.kubectl_apply(&secret_manifest(ns, &remembered)?)
            .await?;
    }
    Ok(remembered)
}

pub async fn rescan_all<H: Host>(
    host: &H,
    catalog_dir: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let apps = crate::routers::apps::installed_apps(host).await?;
    let mut failed = Vec::new();
    for app in &apps {
        let schema = crate::routers::apps::app_schema(catalog_dir, &app.app_id);
        if let Err(e) = rescan(
            host,
            &app.namespace,
            &schema,
            &app.settings,
            &app.annotations,
        )
        .await
        {
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
        let failed = rescan_all(&HOST, &catalog_dir).await?;
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
        parse_output(
            key,
            &json!({ "source": { "logs": format!("YOLAB_OUTPUT {key} (\\S+)") } }),
        )
        .unwrap()
    }

    fn from_config(key: &str, field: &str, format: &str) -> OutputSpec {
        parse_output(
            key,
            &json!({ "format": format, "source": { "config": field } }),
        )
        .unwrap()
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
    fn values_scanned_before_this_change_are_carried_over() {
        let mut ann = Map::new();
        ann.insert(
            LEGACY_ANNOTATION.into(),
            json!(r#"[{"key":"owner_password","label":"Owner password","value":"s3cret","type":"text"},{"key":"empty","value":""}]"#),
        );
        let carried = from_legacy_annotation(&ann, at(0));
        assert_eq!(carried.len(), 1);
        assert_eq!(carried["owner_password"].value, "s3cret");
        assert!(from_legacy_annotation(&Map::new(), at(0)).is_empty());
    }

    #[test]
    fn a_logs_output_not_seen_yet_is_shown_as_waiting() {
        let rows = shown(&[logs("onion_address")], &Remembered::new(), &Map::new());
        assert_eq!(rows[0].value, None);
        assert!(!rows[0].from_config);
    }

    #[test]
    fn a_config_output_shows_the_saved_setting() {
        let config = json!({ "password": "correct horse", "port": 25565 })
            .as_object()
            .cloned()
            .unwrap();
        let rows = shown(
            &[
                from_config("password", "password", "secret"),
                from_config("port", "port", "text"),
                from_config("missing", "nope", "text"),
            ],
            &Remembered::new(),
            &config,
        );
        assert_eq!(rows[0].value.as_deref(), Some("correct horse"));
        assert_eq!(rows[0].format, Format::Secret);
        assert!(rows[0].from_config);
        assert_eq!(rows[1].value.as_deref(), Some("25565"));
        assert_eq!(rows[2].value, None);
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
        use crate::host::fake::FakeHost;
        use std::collections::HashMap;

        const NS: &str = "yolab-files-ab12";
        const GET_SECRET: &str = "kubectl get secret yolab-outputs -n yolab-files-ab12 -o json";
        const GET_PODS: &str = "kubectl get pods -n yolab-files-ab12 -o json";
        const INIT_LOGS: &str = "kubectl logs -n yolab-files-ab12 gateway-7f -c file-explorer-init";
        const CADDY_LOGS: &str = "kubectl logs -n yolab-files-ab12 gateway-7f -c caddy";
        const NOT_FOUND: &str =
            r#"Error from server (NotFound): secrets "yolab-outputs" not found"#;

        fn app(explorer_default: bool) -> AppSchema {
            AppSchema::from_parts(
                json!({ "properties": {
                    "config": { "properties": {
                        "password": { "type": "string", "writeOnly": true, "generate": true },
                        "file_explorer_enabled": { "type": "boolean", "default": explorer_default }
                    }},
                    "outputs": { "properties": {
                        "password": { "title": "Admin password", "format": "secret",
                                      "source": { "config": "password" } },
                        "file_explorer_password": {
                            "title": "File explorer password", "format": "secret",
                            "source": { "logs": "YOLAB_OUTPUT file_explorer_password (\\S+)" },
                            "when": { "properties": { "file_explorer_enabled": { "const": true } } }
                        }
                    }}
                }}),
                &HashMap::new(),
            )
        }

        fn pods() -> String {
            json!({ "items": [{
                "metadata": { "name": "gateway-7f" },
                "spec": {
                    "initContainers": [{ "name": "file-explorer-init" }],
                    "containers": [{ "name": "caddy" }]
                }
            }]})
            .to_string()
        }

        fn stored(remembered: &Remembered) -> String {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_string(remembered).unwrap());
            json!({ "data": { "outputs.json": encoded } }).to_string()
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

        fn applied(host: &FakeHost) -> Vec<String> {
            host.calls()
                .into_iter()
                .filter(|c| c.starts_with("kubectl-apply"))
                .collect()
        }

        #[tokio::test]
        async fn a_value_printed_by_an_init_container_is_found_and_saved() {
            let host = FakeHost::new()
                .fail(GET_SECRET, NOT_FOUND)
                .ok(GET_PODS, &pods())
                .ok(INIT_LOGS, &printed("s3cret"))
                .ok(CADDY_LOGS, "")
                .ok("kubectl-apply", "");

            let found = rescan(&host, NS, &app(true), &Map::new(), &Map::new())
                .await
                .unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
            let writes = applied(&host);
            assert_eq!(writes.len(), 1);
            assert!(writes[0].contains("s3cret") && writes[0].contains("yolab-outputs"));
        }

        #[tokio::test]
        async fn nothing_is_written_when_nothing_new_was_found() {
            let host = FakeHost::new()
                .ok(
                    GET_SECRET,
                    &stored(&remembered("file_explorer_password", "s3cret")),
                )
                .ok(GET_PODS, &pods())
                .ok(INIT_LOGS, &printed("s3cret"))
                .ok(CADDY_LOGS, "");

            rescan(&host, NS, &app(true), &Map::new(), &Map::new())
                .await
                .unwrap();

            assert!(applied(&host).is_empty());
        }

        #[tokio::test]
        async fn a_value_is_kept_after_the_pod_restarts_and_its_logs_are_gone() {
            let host = FakeHost::new()
                .ok(
                    GET_SECRET,
                    &stored(&remembered("file_explorer_password", "s3cret")),
                )
                .ok(GET_PODS, &pods())
                .ok(INIT_LOGS, "")
                .ok(CADDY_LOGS, "");

            let found = rescan(&host, NS, &app(true), &Map::new(), &Map::new())
                .await
                .unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
            assert!(applied(&host).is_empty());
        }

        #[tokio::test]
        async fn a_newer_value_replaces_the_remembered_one() {
            let host = FakeHost::new()
                .ok(
                    GET_SECRET,
                    &stored(&remembered("file_explorer_password", "old")),
                )
                .ok(GET_PODS, &pods())
                .ok(INIT_LOGS, &printed("new"))
                .ok(CADDY_LOGS, "")
                .ok("kubectl-apply", "");

            let found = rescan(&host, NS, &app(true), &Map::new(), &Map::new())
                .await
                .unwrap();

            assert_eq!(found["file_explorer_password"].value, "new");
            assert!(applied(&host)[0].contains("new"));
        }

        #[tokio::test]
        async fn logs_are_not_read_when_nothing_is_expected_from_them() {
            let host = FakeHost::new().fail(GET_SECRET, NOT_FOUND);
            let explorer_off = Map::from_iter([("file_explorer_enabled".into(), json!(false))]);

            rescan(&host, NS, &app(true), &explorer_off, &Map::new())
                .await
                .unwrap();

            assert!(!host.ran("get pods"));
            assert!(!host.ran("logs"));
            assert!(applied(&host).is_empty(), "nothing found, nothing to save");
        }

        #[tokio::test]
        async fn values_from_the_old_annotation_are_moved_into_the_secret() {
            let host = FakeHost::new()
                .fail(GET_SECRET, NOT_FOUND)
                .ok(GET_PODS, &pods())
                .ok(INIT_LOGS, "")
                .ok(CADDY_LOGS, "")
                .ok("kubectl-apply", "");
            let ann = Map::from_iter([(
                LEGACY_ANNOTATION.to_string(),
                json!(r#"[{"key":"file_explorer_password","value":"from-before","type":"text"}]"#),
            )]);

            let found = rescan(&host, NS, &app(true), &Map::new(), &ann)
                .await
                .unwrap();

            assert_eq!(found["file_explorer_password"].value, "from-before");
            assert!(applied(&host)[0].contains("from-before"));
        }

        #[tokio::test]
        async fn one_container_whose_logs_cannot_be_read_does_not_hide_the_others() {
            let host = FakeHost::new()
                .fail(GET_SECRET, NOT_FOUND)
                .ok(GET_PODS, &pods())
                .fail(CADDY_LOGS, "container \"caddy\" is waiting to start")
                .ok(INIT_LOGS, &printed("s3cret"))
                .ok("kubectl-apply", "");

            let found = rescan(&host, NS, &app(true), &Map::new(), &Map::new())
                .await
                .unwrap();

            assert_eq!(found["file_explorer_password"].value, "s3cret");
        }

        #[tokio::test]
        async fn an_unreachable_cluster_is_an_error_not_an_empty_answer() {
            let host = FakeHost::new().fail(GET_SECRET, "Unable to connect to the server: timeout");

            let result = rescan(&host, NS, &app(true), &Map::new(), &Map::new()).await;

            assert!(result.is_err());
            assert!(
                applied(&host).is_empty(),
                "never overwrite what we could not read"
            );
        }

        #[tokio::test]
        async fn every_apps_remembered_values_are_read_in_one_call() {
            let listed = json!({ "items": [
                { "metadata": { "namespace": "yolab-a" },
                  "data": serde_json::from_str::<Value>(&stored(&remembered("url", "https://a"))).unwrap()["data"] },
                { "metadata": { "namespace": "yolab-b" }, "data": { "outputs.json": "not base64!" } }
            ]});
            let host =
                FakeHost::new().ok("kubectl get secrets --all-namespaces", &listed.to_string());

            let everywhere = remembered_everywhere(&host).await;

            assert_eq!(everywhere["yolab-a"]["url"].value, "https://a");
            assert!(
                everywhere["yolab-b"].is_empty(),
                "an unreadable one counts as nothing found"
            );
        }

        #[tokio::test]
        async fn the_listing_shows_nothing_remembered_when_the_cluster_cannot_be_read() {
            let host =
                FakeHost::new().fail("kubectl get secrets", "Unable to connect to the server");
            assert!(remembered_everywhere(&host).await.is_empty());
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
                    "token": { "title": "Token", "source": { "logs": "YOLAB_OUTPUT token (\\S+)" } }
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
            let namespaces = json!({ "items": [
                namespace("yolab-good", "Active"),
                namespace("yolab-broken", "Active"),
                namespace("yolab-leaving", "Terminating")
            ]});
            let host = FakeHost::new()
                .ok("kubectl get namespaces", &namespaces.to_string())
                .fail("kubectl get secret yolab-outputs -n yolab-good", NOT_FOUND)
                .ok("kubectl get pods -n yolab-good", r#"{"items":[{"metadata":{"name":"p"},"spec":{"containers":[{"name":"app"}]}}]}"#)
                .ok("kubectl logs -n yolab-good p -c app", "2026-09-28T12:00:00Z YOLAB_OUTPUT token t0k")
                .fail("kubectl get secret yolab-outputs -n yolab-broken", "Unable to connect to the server")
                .ok("kubectl-apply", "");

            let failed = rescan_all(&host, catalog.path()).await.unwrap();

            assert_eq!(failed, vec!["yolab-broken".to_string()]);
            assert!(applied(&host)[0].contains("t0k"));
            assert!(
                !host.ran("yolab-leaving"),
                "an app being removed is left alone"
            );
        }
    }
}
