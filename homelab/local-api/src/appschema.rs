use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Map, Value};

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

pub const OUTPUT_MARK: &str = "YOLAB_OUTPUT ";

#[derive(Clone, Debug)]
pub struct OutputSpec {
    pub key: String,
    pub title: String,
    pub format: Format,
}

#[derive(Clone, Debug)]
pub struct AppSchema {
    document: Value,
}

impl AppSchema {
    pub fn new(schema: Value) -> AppSchema {
        let mut document = if schema.is_object() {
            schema
        } else {
            json!({ "type": "object", "properties": {} })
        };
        if let Some(props) = document["properties"].as_object_mut() {
            props.remove("yolab");
        }
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
            .filter(|(_, spec)| !is_retired_service_output(spec))
            .map(|(key, spec)| parse_output(key, spec))
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
}

impl OutputSpec {
    pub fn find_in(&self, line: &str) -> Option<String> {
        let (_, printed) = line.split_once(OUTPUT_MARK)?;
        let (key, value) = printed.split_once(' ')?;
        let value = value.trim();
        (key == self.key && !value.is_empty()).then(|| value.to_string())
    }
}

pub fn is_retired_service_output(spec: &Value) -> bool {
    spec["source"].get("service").is_some()
}

pub fn parse_output(key: &str, spec: &Value) -> OutputSpec {
    OutputSpec {
        key: key.to_string(),
        title: spec["title"].as_str().unwrap_or(key).to_string(),
        format: Format::parse(spec["format"].as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(config: Value, outputs: Value) -> AppSchema {
        AppSchema::new(json!({
            "type": "object",
            "properties": {
                "config": { "type": "object", "properties": config },
                "outputs": { "type": "object", "readOnly": true, "properties": outputs },
                "yolab": { "type": "object" }
            }
        }))
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
                "url": { "type": "string", "title": "Web URL", "format": "uri" },
                "password": { "type": "string", "title": "Admin password", "format": "secret" },
                "file_explorer_password": {
                    "type": "string", "title": "File explorer password", "format": "secret"
                }
            }),
        )
    }

    #[test]
    fn an_address_for_other_apps_is_no_longer_an_output() {
        let service = json!({ "source": { "service": { "name": "x", "port": 80 } } });
        assert!(is_retired_service_output(&service));
        assert!(!is_retired_service_output(&json!({ "title": "Web URL" })));
    }

    #[test]
    fn a_chart_still_declaring_one_has_it_skipped_quietly() {
        let app = AppSchema::new(json!({ "properties": { "outputs": { "properties": {
            "internal_url": { "source": { "service": { "name": "q", "port": 8080 } } },
            "password": { "title": "Password" },
        }}}}));
        assert_eq!(keys(&app.outputs()), vec!["password"]);
    }

    #[test]
    fn outputs_keep_the_order_the_developer_wrote_them_in() {
        assert_eq!(
            keys(&explorer_app().outputs()),
            vec!["url", "password", "file_explorer_password"]
        );
    }

    #[test]
    fn an_output_is_the_rest_of_the_line_printed_after_its_key() {
        let outputs = explorer_app().outputs();
        let url = &outputs[0];
        assert_eq!(
            url.find_in("2026-09-28T12:00:00Z YOLAB_OUTPUT url https://files.x.yolab.io"),
            Some("https://files.x.yolab.io".to_string())
        );
        assert_eq!(url.find_in("nothing to see"), None);
    }

    #[test]
    fn a_value_may_hold_spaces() {
        let wiring = parse_output("wiring", &json!({}));
        assert_eq!(
            wiring.find_in("YOLAB_OUTPUT wiring Sonarr and Radarr are connected \r"),
            Some("Sonarr and Radarr are connected".to_string())
        );
    }

    #[test]
    fn another_keys_line_is_not_this_outputs_value() {
        let url = parse_output("url", &json!({}));
        assert_eq!(url.find_in("YOLAB_OUTPUT file_explorer_url https://f"), None);
        assert_eq!(url.find_in("YOLAB_OUTPUT url_extra https://f"), None);
        assert_eq!(url.find_in("YOLAB_OUTPUT url "), None);
        assert_eq!(url.find_in("YOLAB_OUTPUT url"), None);
    }

    #[test]
    fn a_saved_setting_wins_over_its_default() {
        let app = explorer_app();
        let saved = json!({ "file_explorer_enabled": false });
        let filled = app.with_defaults(saved.as_object().unwrap());
        assert_eq!(filled["file_explorer_enabled"], json!(false));
        assert_eq!(app.with_defaults(&Map::new())["file_explorer_enabled"], json!(true));
    }

    #[test]
    fn an_output_declares_only_its_title_and_format() {
        let bare = parse_output("pin", &json!({}));
        assert_eq!(bare.title, "pin");
        assert_eq!(bare.format, Format::Text);
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
    fn a_format_the_page_does_not_know_is_shown_as_text() {
        assert_eq!(Format::parse(Some("hologram")), Format::Text);
        assert_eq!(Format::parse(None), Format::Text);
    }

    #[test]
    fn a_chart_with_no_schema_has_an_empty_form_and_no_outputs() {
        let app = AppSchema::new(Value::Null);
        assert_eq!(app.config()["type"], json!("object"));
        assert!(app.outputs().is_empty());
        assert!(app.credentials().is_empty());
    }
}
