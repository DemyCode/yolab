use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const KIND_ANNOTATION: &str = "yolab.io/kind";
pub(crate) const KIND_GROUP: &str = "group";
pub(crate) const SCHEMA_FILE: &str = "group.schema.json";
const REF_SCHEME: &str = "chart:";
const MAX_REF_DEPTH: usize = 8;
const OMIT: &str = "x-yolab-omit";
const APP_API_VERSION: &str = "yolab.io/v1";
const APP_KIND: &str = "App";
pub(crate) const REDACTED: &str = "__redacted__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChartRef {
    pub chart: String,
    pub version: Option<String>,
    pub pointer: String,
}

pub(crate) fn parse_ref(raw: &str) -> Option<ChartRef> {
    let rest = raw.strip_prefix(REF_SCHEME)?;
    let (target, pointer) = rest.split_once('#').unwrap_or((rest, ""));
    let (chart, version) = match target.split_once('@') {
        Some((chart, version)) if !version.is_empty() => (chart, Some(version.to_string())),
        Some(_) => return None,
        None => (target, None),
    };
    (is_chart_name(chart) && (pointer.is_empty() || pointer.starts_with('/'))).then(|| ChartRef {
        chart: chart.to_string(),
        version,
        pointer: pointer.to_string(),
    })
}

fn omit(schema: &mut Map<String, Value>, names: &BTreeSet<&str>) {
    if let Some(Value::Object(props)) = schema.get_mut("properties") {
        props.retain(|k, _| !names.contains(k.as_str()));
    }
    if let Some(Value::Array(required)) = schema.get_mut("required") {
        required.retain(|r| !r.as_str().is_some_and(|r| names.contains(r)));
    }
    if let Some(Value::Object(deps)) = schema.get_mut("dependencies") {
        deps.retain(|k, _| !names.contains(k.as_str()));
        for dep in deps.values_mut() {
            for combinator in ["oneOf", "anyOf", "allOf"] {
                if let Some(Value::Array(branches)) = dep.get_mut(combinator) {
                    for branch in branches.iter_mut().filter_map(Value::as_object_mut) {
                        omit(branch, names);
                    }
                }
            }
        }
    }
}

pub(crate) fn resolve(
    schema: &Value,
    load: &dyn Fn(&str, Option<&str>) -> Option<Value>,
) -> Result<Value, String> {
    resolve_at(schema, load, 0)
}

fn resolve_at(
    node: &Value,
    load: &dyn Fn(&str, Option<&str>) -> Option<Value>,
    depth: usize,
) -> Result<Value, String> {
    match node {
        Value::Object(map) => {
            if let Some(raw) = map.get("$ref").and_then(Value::as_str) {
                if raw.starts_with(REF_SCHEME) {
                    if depth >= MAX_REF_DEPTH {
                        return Err(format!("{raw} leads through too many references"));
                    }
                    let r = parse_ref(raw).ok_or_else(|| {
                        format!("{raw} is not chart:<name>[@<version>]#<pointer>")
                    })?;
                    let schema =
                        load(&r.chart, r.version.as_deref()).ok_or_else(|| match &r.version {
                            Some(v) => {
                                format!("{raw}: version {v} of {} is not available", r.chart)
                            }
                            None => format!("{raw}: there is no chart named {}", r.chart),
                        })?;
                    let found = schema.pointer(&r.pointer).ok_or_else(|| {
                        format!("{raw}: {} has nothing at {}", r.chart, r.pointer)
                    })?;
                    let mut resolved = resolve_at(found, load, depth + 1)?;
                    if let Value::Object(target) = &mut resolved {
                        for (k, v) in map.iter().filter(|(k, _)| *k != "$ref") {
                            target.insert(k.clone(), resolve_at(v, load, depth)?);
                        }
                        if let Some(Value::Array(dropped)) = target.remove(OMIT) {
                            let names: BTreeSet<&str> =
                                dropped.iter().filter_map(Value::as_str).collect();
                            omit(target, &names);
                        }
                    }
                    return Ok(resolved);
                }
            }
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve_at(v, load, depth)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|i| resolve_at(i, load, depth))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Member {
    Install {
        key: String,
        chart: String,
        version: Option<String>,
        values: Map<String, Value>,
        main: bool,
    },
    Use {
        key: String,
        namespace: String,
        main: bool,
    },
}

impl Member {
    pub(crate) fn key(&self) -> &str {
        match self {
            Member::Install { key, .. } | Member::Use { key, .. } => key,
        }
    }

    pub(crate) fn main(&self) -> bool {
        match self {
            Member::Install { main, .. } | Member::Use { main, .. } => *main,
        }
    }
}

fn is_plain(name: &str) -> bool {
    crate::folders::is_name(name)
}

fn is_chart_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(crate) fn members(rendered: &str) -> Result<Vec<Member>, String> {
    let mut found = Vec::new();
    let mut keys = BTreeSet::new();
    for doc in serde_norway::Deserializer::from_str(rendered) {
        let v: Value = serde::Deserialize::deserialize(doc)
            .map_err(|e| format!("the group rendered something that is not YAML: {e}"))?;
        if v.is_null() {
            continue;
        }
        if v["apiVersion"].as_str() != Some(APP_API_VERSION) || v["kind"].as_str() != Some(APP_KIND)
        {
            return Err(format!(
                "a group may only render {APP_API_VERSION} {APP_KIND} documents, not {} {}",
                v["apiVersion"].as_str().unwrap_or("?"),
                v["kind"].as_str().unwrap_or("?")
            ));
        }
        let key = v["name"]
            .as_str()
            .filter(|k| is_plain(k))
            .ok_or("an App needs a plain name")?;
        if !keys.insert(key.to_string()) {
            return Err(format!("two Apps are named {key}"));
        }
        let main = v["main"].as_bool().unwrap_or(false);
        let member = match (v["chart"].as_str(), v["use"].as_str()) {
            (Some(chart), None) => {
                let (chart, version) = match chart.split_once('@') {
                    Some((c, ver)) => (c, Some(ver.to_string())),
                    None => (chart, None),
                };
                if !is_chart_name(chart) {
                    return Err(format!("App {key} names an unusable chart {chart:?}"));
                }
                let values = match &v["values"] {
                    Value::Null => Map::new(),
                    Value::Object(m) => m.clone(),
                    _ => return Err(format!("App {key}: values must be a mapping")),
                };
                Member::Install {
                    key: key.to_string(),
                    chart: chart.to_string(),
                    version,
                    values,
                    main,
                }
            }
            (None, Some(namespace)) => {
                if !namespace
                    .strip_prefix("yolab-")
                    .is_some_and(crate::routers::install::is_instance_name)
                {
                    return Err(format!("App {key} uses {namespace:?}, which is not an app"));
                }
                Member::Use {
                    key: key.to_string(),
                    namespace: namespace.to_string(),
                    main,
                }
            }
            (None, None) => {
                return Err(format!(
                    "App {key} needs either chart (install one) or use (an app you have)"
                ))
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "App {key} has both chart and use; it is one or the other"
                ))
            }
        };
        found.push(member);
    }
    if found.is_empty() {
        return Err("the group installs nothing with these choices".into());
    }
    Ok(found)
}

fn names_in(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(from) = map.get("from").and_then(Value::as_str) {
                out.insert(from.to_string());
            }
            map.values().for_each(|v| names_in(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| names_in(v, out)),
        _ => {}
    }
}

pub(crate) fn reused(members: &[Member]) -> BTreeSet<String> {
    members
        .iter()
        .filter(|m| matches!(m, Member::Use { .. }))
        .map(|m| m.key().to_string())
        .collect()
}

pub(crate) fn install_order(
    members: &[Member],
    namespaces: &BTreeMap<String, String>,
) -> Vec<String> {
    let by_namespace: BTreeMap<&str, &str> = namespaces
        .iter()
        .map(|(key, ns)| (ns.as_str(), key.as_str()))
        .collect();
    let needs: BTreeMap<&str, Vec<String>> = members
        .iter()
        .map(|m| {
            let mut used = BTreeSet::new();
            if let Member::Install { values, .. } = m {
                names_in(&Value::Object(values.clone()), &mut used);
            }
            let deps = used
                .iter()
                .filter_map(|ns| by_namespace.get(ns.as_str()).map(|k| k.to_string()))
                .filter(|k| k != m.key())
                .collect();
            (m.key(), deps)
        })
        .collect();
    let mut order: Vec<String> = Vec::new();
    fn visit(
        key: &str,
        needs: &BTreeMap<&str, Vec<String>>,
        path: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) {
        if order.iter().any(|k| k == key) || !path.insert(key.to_string()) {
            return;
        }
        for dep in needs.get(key).into_iter().flatten() {
            visit(dep, needs, path, order);
        }
        path.remove(key);
        if needs.contains_key(key) {
            order.push(key.to_string());
        }
    }
    let mut path = BTreeSet::new();
    for m in members {
        visit(m.key(), &needs, &mut path, &mut order);
    }
    order
}

fn prop_of<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(p) = schema["properties"].get(key) {
        return Some(p);
    }
    for combinator in ["oneOf", "anyOf", "allOf"] {
        for branch in schema[combinator].as_array().into_iter().flatten() {
            if let Some(p) = prop_of(branch, key) {
                return Some(p);
            }
        }
    }
    schema["dependencies"]
        .as_object()
        .into_iter()
        .flat_map(|d| d.values())
        .find_map(|dep| prop_of(dep, key))
}

pub(crate) fn redact(values: &Map<String, Value>, schema: &Value) -> Map<String, Value> {
    values
        .iter()
        .map(|(k, v)| {
            let prop = prop_of(schema, k);
            let shown = match (prop, v) {
                (Some(p), _) if p["writeOnly"] == Value::Bool(true) => {
                    Value::String(REDACTED.into())
                }
                (Some(p), Value::Object(inner)) => Value::Object(redact(inner, p)),
                _ => v.clone(),
            };
            (k.clone(), shown)
        })
        .collect()
}

pub(crate) fn keep_secrets(
    incoming: &Map<String, Value>,
    stored: &Map<String, Value>,
    schema: &Value,
) -> Map<String, Value> {
    incoming
        .iter()
        .filter_map(|(k, v)| {
            let prop = prop_of(schema, k);
            let kept = match (prop, v) {
                (Some(_), Value::String(s)) if s == REDACTED => stored.get(k)?.clone(),
                (Some(p), Value::Object(inner)) => {
                    let before = stored
                        .get(k)
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    Value::Object(keep_secrets(inner, &before, p))
                }
                _ => v.clone(),
            };
            Some((k.clone(), kept))
        })
        .collect()
}

pub(crate) fn generated(config_schema: &Value, values: &Map<String, Value>) -> Map<String, Value> {
    let mut filled = values.clone();
    for (name, prop) in config_schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let wanted =
            prop["writeOnly"] == Value::Bool(true) && prop["generate"] == Value::Bool(true);
        let unset = filled.get(name).is_none_or(|v| v.as_str() == Some(""));
        if wanted && unset {
            let length = prop["minLength"].as_u64().unwrap_or(0).max(24) as usize;
            filled.insert(name.clone(), Value::String(random_secret(length)));
        }
    }
    filled
}

fn random_secret(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..length)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct MemberStatus {
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct GroupRecord {
    pub name: String,
    pub title: String,
    pub chart: String,
    pub repo: String,
    pub version: String,
    #[serde(default)]
    pub values: Map<String, Value>,
    #[serde(default)]
    pub members: BTreeMap<String, String>,
    #[serde(default)]
    pub status: BTreeMap<String, MemberStatus>,
    #[serde(default)]
    pub left: Vec<String>,
    #[serde(default)]
    pub reused: BTreeSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn charts(name: &str, version: Option<&str>) -> Option<Value> {
        match (name, version) {
            ("jellyfin", None) | ("jellyfin", Some("0.2.12")) => Some(json!({
                "properties": { "config": { "properties": {
                    "media_folder": { "type": "string", "format": "folder", "title": "Media folder" },
                    "admin": { "$ref": "chart:sonarr#/properties/config/properties/password" },
                }}}
            })),
            ("sonarr", None) => Some(json!({
                "properties": { "config": { "properties": {
                    "password": { "type": "string", "writeOnly": true, "generate": true },
                }}}
            })),
            ("loop", None) => Some(json!({ "a": { "$ref": "chart:loop#/a" } })),
            ("radarr", None) => Some(json!({
                "properties": { "config": {
                    "type": "object",
                    "required": ["media_folder", "subdomain"],
                    "properties": {
                        "media_folder": { "type": "string" },
                        "subdomain": { "type": "string" },
                    },
                    "dependencies": {
                        "media_folder": { "oneOf": [{ "properties": { "media_folder": {} } }] },
                        "tailscale": { "oneOf": [{
                            "properties": { "tailscale": {}, "media_folder": {} },
                            "required": ["media_folder"],
                        }] },
                    },
                }}
            })),
            _ => None,
        }
    }

    #[test]
    fn a_reference_names_a_chart_an_optional_version_and_a_place_in_its_schema() {
        assert_eq!(
            parse_ref("chart:jellyfin@0.2.12#/properties/config"),
            Some(ChartRef {
                chart: "jellyfin".into(),
                version: Some("0.2.12".into()),
                pointer: "/properties/config".into(),
            })
        );
        assert_eq!(parse_ref("chart:sonarr").unwrap().pointer, "");
        for bad in [
            "sonarr#/x",
            "chart:Sonarr#/x",
            "chart:a@#/x",
            "chart:a#x",
            "chart:../a#/x",
        ] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_imported_field_arrives_whole_and_the_groups_own_words_win() {
        let group = json!({
            "properties": {
                "media": {
                    "$ref": "chart:jellyfin@0.2.12#/properties/config/properties/media_folder",
                    "title": "Where your films go",
                },
                "count": { "type": "integer" },
            }
        });
        let resolved = resolve(&group, &charts).unwrap();
        assert_eq!(
            resolved["properties"]["media"],
            json!({ "type": "string", "format": "folder", "title": "Where your films go" })
        );
        assert_eq!(
            resolved["properties"]["count"],
            json!({ "type": "integer" })
        );
    }

    #[test]
    fn a_reference_inside_an_imported_schema_is_followed_too() {
        let group = json!({ "$ref": "chart:jellyfin#/properties/config/properties/admin" });
        assert_eq!(
            resolve(&group, &charts).unwrap(),
            json!({ "type": "string", "writeOnly": true, "generate": true })
        );
    }

    #[test]
    fn a_reference_that_cannot_be_followed_says_why() {
        let missing_chart = resolve(&json!({ "$ref": "chart:plex#/x" }), &charts).unwrap_err();
        assert!(
            missing_chart.contains("no chart named plex"),
            "{missing_chart}"
        );
        let old = resolve(&json!({ "$ref": "chart:jellyfin@0.1.0#/x" }), &charts).unwrap_err();
        assert!(old.contains("version 0.1.0"), "{old}");
        let nowhere = resolve(&json!({ "$ref": "chart:jellyfin#/nope" }), &charts).unwrap_err();
        assert!(nowhere.contains("nothing at /nope"), "{nowhere}");
        let looped = resolve(&json!({ "$ref": "chart:loop#/a" }), &charts).unwrap_err();
        assert!(looped.contains("too many references"), "{looped}");
    }

    #[test]
    fn a_whole_app_form_comes_in_without_the_fields_the_group_decides_itself() {
        let group = json!({
            "$ref": "chart:radarr#/properties/config",
            "title": "Radarr",
            "x-yolab-omit": ["media_folder"],
        });
        let resolved = resolve(&group, &charts).unwrap();
        assert_eq!(resolved["title"], "Radarr");
        assert!(resolved.get("x-yolab-omit").is_none());
        assert_eq!(
            resolved["properties"],
            json!({ "subdomain": { "type": "string" } })
        );
        assert_eq!(resolved["required"], json!(["subdomain"]));
        assert!(resolved["dependencies"].get("media_folder").is_none());
        let branch = &resolved["dependencies"]["tailscale"]["oneOf"][0];
        assert_eq!(branch["properties"], json!({ "tailscale": {} }));
        assert_eq!(branch["required"], json!([]));
    }

    #[test]
    fn a_local_reference_is_left_for_the_form_to_follow() {
        let group = json!({ "$ref": "#/definitions/x" });
        assert_eq!(resolve(&group, &charts).unwrap(), group);
    }

    const RENDERED: &str = "\
---
apiVersion: yolab.io/v1
kind: App
name: qbittorrent
chart: qbittorrent
values: { media_folder: movies }
---
apiVersion: yolab.io/v1
kind: App
name: sonarr
chart: sonarr@0.1.19
values:
  media_folder: movies
  download_client: { from: yolab-qbittorrent-ab12 }
---
apiVersion: yolab.io/v1
kind: App
name: jellyfin
use: yolab-jellyfin-zz99
main: true
";

    #[test]
    fn a_rendered_group_lists_apps_to_install_and_apps_to_reuse() {
        let found = members(RENDERED).unwrap();
        assert_eq!(found.len(), 3);
        assert!(
            matches!(&found[1], Member::Install { chart, version: Some(v), .. }
            if chart == "sonarr" && v == "0.1.19")
        );
        assert_eq!(
            found[2],
            Member::Use {
                key: "jellyfin".into(),
                namespace: "yolab-jellyfin-zz99".into(),
                main: true,
            }
        );
        assert_eq!(
            reused(&found).into_iter().collect::<Vec<_>>(),
            vec!["jellyfin"],
            "an app the person already had is never removed with the group"
        );
    }

    #[test]
    fn a_group_may_only_render_apps() {
        let e = members("apiVersion: v1\nkind: ConfigMap\nmetadata: { name: x }\n").unwrap_err();
        assert!(e.contains("only render"), "{e}");
        let twice = "apiVersion: yolab.io/v1\nkind: App\nname: a\nchart: a\n---\napiVersion: yolab.io/v1\nkind: App\nname: a\nchart: b\n";
        assert!(members(twice).unwrap_err().contains("two Apps"));
        let both = "apiVersion: yolab.io/v1\nkind: App\nname: a\nchart: a\nuse: yolab-a\n";
        assert!(members(both).unwrap_err().contains("one or the other"));
        let foreign = "apiVersion: yolab.io/v1\nkind: App\nname: a\nuse: kube-system\n";
        assert!(members(foreign).unwrap_err().contains("not an app"));
        assert!(members("").unwrap_err().contains("installs nothing"));
    }

    #[test]
    fn an_app_is_installed_after_the_apps_it_connects_to() {
        let found = members(RENDERED).unwrap();
        let namespaces: BTreeMap<String, String> = [
            ("qbittorrent", "yolab-qbittorrent-ab12"),
            ("sonarr", "yolab-sonarr-cd34"),
            ("jellyfin", "yolab-jellyfin-zz99"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut reversed = found.clone();
        reversed.reverse();
        let order = install_order(&reversed, &namespaces);
        let at = |k: &str| order.iter().position(|o| o == k).unwrap();
        assert!(at("qbittorrent") < at("sonarr"), "{order:?}");
        assert_eq!(order.len(), 3);
    }

    fn secret_schema() -> Value {
        json!({ "properties": {
            "admin_password": { "type": "string", "writeOnly": true },
            "client": { "oneOf": [
                { "properties": { "kind": { "const": "external" },
                                  "password": { "type": "string", "writeOnly": true } } },
            ]},
        }})
    }

    #[test]
    fn a_groups_saved_choices_are_shown_without_their_secrets() {
        let values = json!({
            "admin_password": "hunter2",
            "client": { "kind": "external", "password": "p", "address": "http://nas" },
            "title": "Films",
        });
        let shown = redact(values.as_object().unwrap(), &secret_schema());
        assert_eq!(
            Value::Object(shown),
            json!({
                "admin_password": REDACTED,
                "client": { "kind": "external", "password": REDACTED, "address": "http://nas" },
                "title": "Films",
            })
        );
    }

    #[test]
    fn a_secret_left_untouched_in_the_form_keeps_its_saved_value() {
        let stored = json!({
            "admin_password": "hunter2",
            "client": { "kind": "external", "password": "p" },
        });
        let incoming = json!({
            "admin_password": REDACTED,
            "client": { "kind": "external", "password": "new" },
        });
        let kept = keep_secrets(
            incoming.as_object().unwrap(),
            stored.as_object().unwrap(),
            &secret_schema(),
        );
        assert_eq!(
            Value::Object(kept),
            json!({ "admin_password": "hunter2", "client": { "kind": "external", "password": "new" } })
        );
    }

    #[test]
    fn a_member_gets_the_passwords_its_chart_would_have_generated_in_the_form() {
        let schema = json!({ "properties": {
            "password": { "type": "string", "writeOnly": true, "generate": true },
            "token": { "type": "string", "writeOnly": true, "generate": true, "minLength": 40 },
            "chosen": { "type": "string", "writeOnly": true, "generate": true },
            "plain": { "type": "string" },
        }});
        let values = json!({ "chosen": "mine", "password": "" });
        let filled = generated(&schema, values.as_object().unwrap());
        assert_eq!(filled["password"].as_str().unwrap().len(), 24);
        assert_eq!(filled["token"].as_str().unwrap().len(), 40);
        assert_eq!(filled["chosen"], "mine");
        assert!(!filled.contains_key("plain"));
    }
}
