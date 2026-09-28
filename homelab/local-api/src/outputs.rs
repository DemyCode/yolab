use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::appschema::{AppSchema, Format, OutputSpec, Source};

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

pub fn parse_remembered(raw: Option<&String>) -> Remembered {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

pub async fn read_remembered(ns: &str) -> anyhow::Result<Remembered> {
    let data = crate::kubectl::get_secret(SECRET, ns).await?;
    Ok(parse_remembered(data.as_ref().and_then(|d| d.get(SECRET_KEY))))
}

async fn write_remembered(ns: &str, remembered: &Remembered) -> anyhow::Result<()> {
    let json = serde_json::to_string(remembered)?;
    crate::kubectl::apply_secret(
        SECRET,
        ns,
        &[(SECRET_KEY, json.as_str())],
        &[("yolab.io/managed", "true"), ("yolab.io/outputs", "true")],
    )
    .await?;
    Ok(())
}

pub async fn remembered_everywhere() -> BTreeMap<String, Remembered> {
    let listed = crate::kubectl::get_json(&[
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
    use base64::Engine as _;
    listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|secret| {
            let ns = secret["metadata"]["namespace"].as_str()?;
            let encoded = secret["data"][SECRET_KEY].as_str()?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            let raw = String::from_utf8(bytes).ok()?;
            Some((ns.to_string(), parse_remembered(Some(&raw))))
        })
        .collect()
}

async fn log_lines(ns: &str) -> anyhow::Result<Vec<String>> {
    let pods = crate::kubectl::get_json(&["get", "pods", "-n", ns, "-o", "json"]).await?;
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
            if let Ok(text) = crate::kubectl::run(&[
                "logs",
                "-n",
                ns,
                name,
                "-c",
                container,
                "--timestamps",
                LOGS_TAIL,
            ])
            .await
            {
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

pub async fn rescan(
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

    let stored = crate::kubectl::get_secret(SECRET, ns).await?;
    let mut remembered = parse_remembered(stored.as_ref().and_then(|d| d.get(SECRET_KEY)));
    let mut changed = false;
    for (key, found) in from_legacy_annotation(ann, Utc::now()) {
        if !remembered.contains_key(&key) {
            remembered.insert(key, found);
            changed = true;
        }
    }

    if !looked_for.is_empty() {
        let lines = log_lines(ns).await?;
        let fresh = latest_matches(&looked_for, lines.iter().map(String::as_str));
        changed |= remember(&mut remembered, fresh, Utc::now());
    }

    if changed || (stored.is_none() && !remembered.is_empty()) {
        write_remembered(ns, &remembered).await?;
    }
    Ok(remembered)
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
        let apps = crate::routers::apps::installed_apps().await?;
        let mut failed = Vec::new();
        for app in &apps {
            let schema = crate::routers::apps::app_schema(&catalog_dir, &app.app_id);
            if let Err(e) = rescan(&app.namespace, &schema, &app.settings, &app.annotations).await
            {
                tracing::debug!("{}: outputs could not be rescanned ({e})", app.namespace);
                failed.push(app.namespace.clone());
            }
        }
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
            ["YOLAB_OUTPUT token first", "noise", "YOLAB_OUTPUT token second"],
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
        assert!(remember(&mut remembered, fresh(&[("password", "xyz")]), at(2)));
        assert_eq!(remembered["password"], Found { value: "xyz".into(), found_at: at(2) });
    }

    #[test]
    fn seeing_the_same_value_again_does_not_count_as_a_change() {
        let mut remembered = Remembered::new();
        remember(&mut remembered, fresh(&[("password", "abc")]), at(1));
        assert!(!remember(&mut remembered, fresh(&[("password", "abc")]), at(5)));
        assert_eq!(remembered["password"].found_at, at(1), "it was found at 12:01, not re-found");
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
        assert!(parse_remembered(Some(&"not json".to_string())).is_empty());
        assert!(parse_remembered(None).is_empty());
    }
}
