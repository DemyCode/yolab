use crate::error::Outcome as _;
use std::collections::HashSet;

use serde::Serialize;
use serde_json::{json, Value};

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
pub const CAPTURE_FIELD: &str = "x-yolab-capture";

#[derive(Clone, Debug)]
pub struct OutputSpec {
    pub key: String,
    pub title: String,
    pub format: Format,
    pub capture: regex::Regex,
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
            .filter_map(|(key, spec)| {
                parse_output(key, spec).ok_or_warn(format!("output {key} is left out"))
            })
            .collect()
    }
}

impl OutputSpec {
    pub fn find_in(&self, line: &str) -> Option<String> {
        let value = self.capture.captures(line)?.get(1)?.as_str().trim();
        (!value.is_empty()).then(|| value.to_string())
    }
}

pub fn default_capture(key: &str) -> String {
    format!("{}{} (.+)", regex::escape(OUTPUT_MARK), regex::escape(key))
}

pub fn parse_output(key: &str, spec: &Value) -> anyhow::Result<OutputSpec> {
    let pattern = match &spec[CAPTURE_FIELD] {
        Value::Null => default_capture(key),
        Value::String(p) => p.clone(),
        other => anyhow::bail!("{CAPTURE_FIELD} must be a string, got {other}"),
    };
    let capture = regex::Regex::new(&pattern)?;
    if capture.captures_len() != 2 {
        anyhow::bail!(
            "{CAPTURE_FIELD} {pattern:?} must have exactly one capture group, it has {}",
            capture.captures_len() - 1
        );
    }
    Ok(OutputSpec {
        key: key.to_string(),
        title: spec["title"].as_str().unwrap_or(key).to_string(),
        format: Format::parse(spec["format"].as_str()),
        capture,
    })
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
        let wiring = parse_output("wiring", &json!({})).unwrap();
        assert_eq!(
            wiring.find_in("YOLAB_OUTPUT wiring Sonarr and Radarr are connected \r"),
            Some("Sonarr and Radarr are connected".to_string())
        );
    }

    #[test]
    fn another_keys_line_is_not_this_outputs_value() {
        let url = parse_output("url", &json!({})).unwrap();
        assert_eq!(
            url.find_in("YOLAB_OUTPUT file_explorer_url https://f"),
            None
        );
        assert_eq!(url.find_in("YOLAB_OUTPUT url_extra https://f"), None);
        assert_eq!(url.find_in("YOLAB_OUTPUT url "), None);
        assert_eq!(url.find_in("YOLAB_OUTPUT url"), None);
    }

    #[test]
    fn an_output_without_a_title_or_format_is_text_titled_by_its_key() {
        let bare = parse_output("pin", &json!({})).unwrap();
        assert_eq!(bare.title, "pin");
        assert_eq!(bare.format, Format::Text);
    }

    #[test]
    fn a_custom_capture_reads_a_value_straight_from_the_apps_own_logs() {
        let password = parse_output(
            "admin_password",
            &json!({ "x-yolab-capture": "Generated admin password: (\\S+)" }),
        )
        .unwrap();
        assert_eq!(
            password.find_in("[INFO] Generated admin password: s3cr3t (store it)"),
            Some("s3cr3t".to_string())
        );
        assert_eq!(password.find_in("YOLAB_OUTPUT admin_password other"), None);
    }

    #[test]
    fn a_custom_capture_may_anchor_on_the_start_of_the_message() {
        let url = parse_output(
            "admin_url",
            &json!({ "x-yolab-capture": "^Admin panel at (https://\\S+)$" }),
        )
        .unwrap();
        assert_eq!(
            url.find_in("Admin panel at https://x.yolab.io/admin"),
            Some("https://x.yolab.io/admin".to_string())
        );
        assert_eq!(url.find_in("old: Admin panel at https://x"), None);
    }

    #[test]
    fn a_key_with_regex_characters_is_matched_literally_by_the_default_capture() {
        let dotted = parse_output("a.b", &json!({})).unwrap();
        assert_eq!(dotted.find_in("YOLAB_OUTPUT a.b v"), Some("v".to_string()));
        assert_eq!(dotted.find_in("YOLAB_OUTPUT axb v"), None);
    }

    #[test]
    fn a_capture_needs_exactly_one_group() {
        for bad in ["no group", "(a) (b)"] {
            assert!(
                parse_output("k", &json!({ "x-yolab-capture": bad })).is_err(),
                "{bad}"
            );
        }
        assert!(parse_output("k", &json!({ "x-yolab-capture": "(?:a) (b)" })).is_ok());
    }

    #[test]
    fn an_invalid_capture_is_refused_and_leaves_only_that_output_out() {
        assert!(parse_output("k", &json!({ "x-yolab-capture": "(unclosed" })).is_err());
        assert!(parse_output("k", &json!({ "x-yolab-capture": 3 })).is_err());
        let app = schema(
            json!({}),
            json!({
                "good": { "title": "Good" },
                "bad": { "x-yolab-capture": "(unclosed" }
            }),
        );
        assert_eq!(keys(&app.outputs()), vec!["good"]);
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
    fn every_format_the_page_knows_is_read_from_the_chart() {
        assert_eq!(Format::parse(Some("uri")), Format::Uri);
        assert_eq!(Format::parse(Some("secret")), Format::Secret);
        assert_eq!(Format::parse(Some("multiline")), Format::Multiline);
        assert_eq!(Format::parse(Some("text")), Format::Text);
    }

    #[test]
    fn the_form_is_the_chart_s_own_config_section() {
        let app = explorer_app();
        assert_eq!(app.config()["properties"]["password"]["writeOnly"], true);
    }

    #[test]
    fn a_chart_with_no_schema_has_an_empty_form_and_no_outputs() {
        let app = AppSchema::new(Value::Null);
        assert_eq!(app.config()["type"], json!("object"));
        assert!(app.outputs().is_empty());
        assert!(app.credentials().is_empty());
    }
}
