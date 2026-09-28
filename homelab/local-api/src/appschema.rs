use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};

pub const LEGACY_UISCHEMA: &str = "yolab.io/uischema";
pub const LEGACY_OUTPUTS: &str = "yolab.io/outputs";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Text,
    Uri,
    Secret,
    Multiline,
}

impl Format {
    fn parse(raw: Option<&str>) -> Format {
        match raw {
            Some("uri") => Format::Uri,
            Some("secret") => Format::Secret,
            Some("multiline") => Format::Multiline,
            _ => Format::Text,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Source {
    Logs(Regex),
    Config(String),
}

#[derive(Clone, Debug)]
pub struct OutputSpec {
    pub key: String,
    pub title: String,
    pub format: Format,
    pub source: Source,
    when: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct AppSchema {
    document: Value,
}

impl AppSchema {
    pub fn from_parts(schema: Value, annotations: &HashMap<String, String>) -> AppSchema {
        let mut document = if schema.is_object() {
            schema
        } else {
            json!({ "type": "object", "properties": {} })
        };
        if let Some(props) = document["properties"].as_object_mut() {
            props.remove("yolab");
        }
        legacy::translate(&mut document, annotations);
        AppSchema { document }
    }

    pub fn config(&self) -> Value {
        match &self.document["properties"]["config"] {
            Value::Null => json!({ "type": "object", "properties": {} }),
            config => config.clone(),
        }
    }

    pub fn credentials(&self) -> HashSet<String> {
        self.document["properties"]["config"]["properties"]
            .as_object()
            .map(|props| {
                props
                    .iter()
                    .filter(|(_, spec)| spec["writeOnly"] == json!(true))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn outputs(&self) -> Vec<OutputSpec> {
        let Some(props) = self.document["properties"]["outputs"]["properties"].as_object() else {
            return Vec::new();
        };
        props
            .iter()
            .filter_map(|(key, spec)| match parse_output(key, spec) {
                Ok(output) => Some(output),
                Err(why) => {
                    tracing::warn!("output {key} is ignored: {why}");
                    None
                }
            })
            .collect()
    }

    pub fn with_defaults(&self, config: &Map<String, Value>) -> Map<String, Value> {
        let mut filled = config.clone();
        if let Some(props) = self.document["properties"]["config"]["properties"].as_object() {
            for (name, spec) in props {
                if let Some(default) = spec.get("default") {
                    filled
                        .entry(name.clone())
                        .or_insert_with(|| default.clone());
                }
            }
        }
        filled
    }

    pub fn applicable_outputs(&self, config: &Map<String, Value>) -> Vec<OutputSpec> {
        let settings = Value::Object(self.with_defaults(config));
        self.outputs()
            .into_iter()
            .filter(|output| output.applies_to(&settings))
            .collect()
    }
}

impl OutputSpec {
    fn applies_to(&self, settings: &Value) -> bool {
        let Some(when) = &self.when else {
            return true;
        };
        match jsonschema::validator_for(when) {
            Ok(validator) => validator.is_valid(settings),
            Err(e) => {
                tracing::warn!("output {}: its `when` is not a valid schema ({e})", self.key);
                false
            }
        }
    }

    pub fn find_in(&self, line: &str) -> Option<String> {
        let Source::Logs(re) = &self.source else {
            return None;
        };
        re.captures(line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

pub fn parse_output(key: &str, spec: &Value) -> Result<OutputSpec, String> {
    let source = match &spec["source"] {
        Value::Object(s) if s.len() == 1 => {
            if let Some(pattern) = s.get("logs").and_then(Value::as_str) {
                let re = Regex::new(pattern).map_err(|e| format!("its logs pattern is invalid: {e}"))?;
                if re.captures_len() < 2 {
                    return Err("its logs pattern has no capture group for the value".into());
                }
                Source::Logs(re)
            } else if let Some(field) = s.get("config").and_then(Value::as_str) {
                Source::Config(field.to_string())
            } else {
                return Err("its source is neither {\"logs\": <pattern>} nor {\"config\": <field>}".into());
            }
        }
        _ => return Err("it has no source".into()),
    };
    let when = match spec.get("when") {
        None => None,
        Some(w) if w.is_object() || w.is_boolean() => Some(w.clone()),
        Some(_) => return Err("its `when` is not a schema".into()),
    };
    Ok(OutputSpec {
        key: key.to_string(),
        title: spec["title"].as_str().unwrap_or(key).to_string(),
        format: Format::parse(spec["format"].as_str()),
        source,
        when,
    })
}

mod legacy {
    use super::*;

    pub(super) fn translate(document: &mut Value, annotations: &HashMap<String, String>) {
        if let Some(ui) = annotations
            .get(LEGACY_UISCHEMA)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        {
            credentials_from(document, &ui);
        }
        if document["properties"]["outputs"].is_null() {
            if let Some(list) = annotations
                .get(LEGACY_OUTPUTS)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            {
                outputs_from(document, &list);
            }
        }
    }

    fn credentials_from(document: &mut Value, ui: &Value) {
        let Some(ui) = ui.as_object() else { return };
        let Some(props) = document["properties"]["config"]["properties"].as_object_mut() else {
            return;
        };
        for (name, hints) in ui {
            let Some(spec) = props.get_mut(name).and_then(Value::as_object_mut) else {
                continue;
            };
            match hints["ui:widget"].as_str() {
                Some("PasswordWidget") => {
                    spec.insert("writeOnly".into(), json!(true));
                    spec.insert("generate".into(), json!(true));
                }
                Some("password") => {
                    spec.insert("writeOnly".into(), json!(true));
                }
                _ => {}
            }
        }
    }

    fn outputs_from(document: &mut Value, list: &Value) {
        let mut outputs = Map::new();
        for item in list.as_array().into_iter().flatten() {
            let (Some(key), Some(pattern)) = (item["key"].as_str(), item["pattern"].as_str()) else {
                continue;
            };
            let format = match item["type"].as_str() {
                Some("hidden") => continue,
                Some("url") => "uri",
                _ if key.contains("password") || key.contains("secret") => "secret",
                _ => "text",
            };
            outputs.insert(
                key.to_string(),
                json!({
                    "type": "string",
                    "title": item["label"].as_str().unwrap_or(key),
                    "format": format,
                    "source": { "logs": pattern },
                }),
            );
        }
        if outputs.is_empty() {
            return;
        }
        if let Some(props) = document["properties"].as_object_mut() {
            props.insert(
                "outputs".into(),
                json!({ "type": "object", "readOnly": true, "properties": outputs }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(config: Value, outputs: Value) -> AppSchema {
        AppSchema::from_parts(
            json!({
                "type": "object",
                "properties": {
                    "config": { "type": "object", "properties": config },
                    "outputs": { "type": "object", "readOnly": true, "properties": outputs },
                    "yolab": { "type": "object" }
                }
            }),
            &HashMap::new(),
        )
    }

    fn settings(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn keys(outputs: &[OutputSpec]) -> Vec<&str> {
        outputs.iter().map(|o| o.key.as_str()).collect()
    }

    fn explorer_app() -> AppSchema {
        schema(
            json!({
                "password": { "type": "string", "writeOnly": true, "generate": true },
                "file_explorer_enabled": { "type": "boolean", "default": true }
            }),
            json!({
                "url": { "type": "string", "title": "Web URL", "format": "uri",
                         "source": { "logs": "YOLAB_OUTPUT url (\\S+)" } },
                "password": { "type": "string", "title": "Admin password", "format": "secret",
                              "source": { "config": "password" } },
                "file_explorer_password": {
                    "type": "string", "title": "File explorer password", "format": "secret",
                    "source": { "logs": "YOLAB_OUTPUT file_explorer_password (\\S+)" },
                    "when": { "properties": { "file_explorer_enabled": { "const": true } } }
                }
            }),
        )
    }

    #[test]
    fn outputs_keep_the_order_the_developer_wrote_them_in() {
        assert_eq!(
            keys(&explorer_app().outputs()),
            vec!["url", "password", "file_explorer_password"]
        );
    }

    #[test]
    fn an_output_whose_condition_fails_is_not_expected() {
        let off = settings(json!({ "file_explorer_enabled": false }));
        assert_eq!(keys(&explorer_app().applicable_outputs(&off)), vec!["url", "password"]);
    }

    #[test]
    fn a_condition_is_checked_against_the_defaults_for_settings_never_saved() {
        assert_eq!(
            keys(&explorer_app().applicable_outputs(&Map::new())),
            vec!["url", "password", "file_explorer_password"]
        );
    }

    #[test]
    fn a_saved_setting_wins_over_its_default() {
        let on = settings(json!({ "file_explorer_enabled": true }));
        assert_eq!(explorer_app().applicable_outputs(&on).len(), 3);
    }

    #[test]
    fn a_logs_output_takes_the_first_capture_group_of_a_matching_line() {
        let outputs = explorer_app().outputs();
        let url = &outputs[0];
        assert_eq!(
            url.find_in("2026-09-28 YOLAB_OUTPUT url https://files.x.yolab.io"),
            Some("https://files.x.yolab.io".to_string())
        );
        assert_eq!(url.find_in("nothing to see"), None);
    }

    #[test]
    fn a_config_output_is_never_looked_for_in_logs() {
        let outputs = explorer_app().outputs();
        assert!(matches!(&outputs[1].source, Source::Config(f) if f == "password"));
        assert_eq!(outputs[1].find_in("YOLAB_OUTPUT password hunter2"), None);
    }

    #[test]
    fn write_only_settings_are_the_credentials() {
        assert_eq!(
            explorer_app().credentials(),
            HashSet::from(["password".to_string()])
        );
    }

    #[test]
    fn the_platform_section_is_not_part_of_what_a_developer_describes() {
        assert!(explorer_app().document["properties"]["yolab"].is_null());
    }

    #[test]
    fn a_broken_output_is_left_out_rather_than_breaking_the_others() {
        let app = schema(
            json!({}),
            json!({
                "no_source": { "type": "string" },
                "no_group": { "type": "string", "source": { "logs": "YOLAB_OUTPUT x \\S+" } },
                "bad_regex": { "type": "string", "source": { "logs": "(" } },
                "fine": { "type": "string", "source": { "logs": "x=(\\S+)" } }
            }),
        );
        assert_eq!(keys(&app.outputs()), vec!["fine"]);
    }

    #[test]
    fn parse_output_explains_what_is_wrong() {
        assert!(parse_output("a", &json!({})).unwrap_err().contains("no source"));
        assert!(parse_output("a", &json!({ "source": { "logs": "x" } }))
            .unwrap_err()
            .contains("capture group"));
        assert!(parse_output("a", &json!({ "source": { "config": "p" }, "when": 3 }))
            .unwrap_err()
            .contains("when"));
    }

    #[test]
    fn a_format_the_page_does_not_know_is_shown_as_text() {
        assert_eq!(Format::parse(Some("hologram")), Format::Text);
        assert_eq!(Format::parse(None), Format::Text);
    }

    #[test]
    fn a_chart_in_the_old_annotation_format_reads_as_the_new_one() {
        let annotations = HashMap::from([
            (
                LEGACY_UISCHEMA.to_string(),
                r#"{"subdomain":{"ui:widget":"TunnelWidget"},"admin_password":{"ui:widget":"PasswordWidget"},"pin":{"ui:widget":"password"}}"#.to_string(),
            ),
            (
                LEGACY_OUTPUTS.to_string(),
                r#"[{"key":"url","label":"Web URL","type":"url","pattern":"YOLAB_OUTPUT url (\\S+)"},
                    {"key":"owner_password","label":"Owner password","type":"text","pattern":"YOLAB_OUTPUT owner_password (\\S+)"},
                    {"key":"jwt","label":"JWT","type":"hidden","pattern":"YOLAB_OUTPUT jwt (\\S+)"}]"#
                    .to_string(),
            ),
        ]);
        let app = AppSchema::from_parts(
            json!({ "properties": { "config": { "properties": {
                "subdomain": { "type": "string", "format": "tunnel" },
                "admin_password": { "type": "string" },
                "pin": { "type": "string" }
            }}}}),
            &annotations,
        );

        assert_eq!(
            app.credentials(),
            HashSet::from(["admin_password".to_string(), "pin".to_string()])
        );
        assert_eq!(app.config()["properties"]["admin_password"]["generate"], json!(true));
        assert!(app.config()["properties"]["pin"].get("generate").is_none());

        let outputs = app.outputs();
        assert_eq!(keys(&outputs), vec!["url", "owner_password"], "hidden outputs are not shown");
        assert_eq!(outputs[0].format, Format::Uri);
        assert_eq!(outputs[1].format, Format::Secret);
    }

    #[test]
    fn outputs_declared_in_the_schema_win_over_the_old_annotation() {
        let annotations = HashMap::from([(
            LEGACY_OUTPUTS.to_string(),
            r#"[{"key":"old","type":"text","pattern":"old (\\S+)"}]"#.to_string(),
        )]);
        let app = AppSchema::from_parts(
            json!({ "properties": { "outputs": { "properties": {
                "new": { "type": "string", "source": { "logs": "new (\\S+)" } }
            }}}}),
            &annotations,
        );
        assert_eq!(keys(&app.outputs()), vec!["new"]);
    }

    #[test]
    fn a_chart_with_no_schema_has_an_empty_form_and_no_outputs() {
        let app = AppSchema::from_parts(Value::Null, &HashMap::new());
        assert_eq!(app.config()["type"], json!("object"));
        assert!(app.outputs().is_empty());
        assert!(app.credentials().is_empty());
    }
}
