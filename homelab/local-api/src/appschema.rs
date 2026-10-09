use std::collections::HashSet;

use regex::Regex;
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

#[derive(Clone, Debug)]
pub enum Source {
    Logs(Regex),
    Config(String),
    Service(Address),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Address {
    scheme: String,
    service: String,
    port: u16,
    path: String,
}

impl Address {
    fn parse(spec: &Value) -> Option<Address> {
        let service = spec["service"]
            .as_str()
            .or_else(|| spec["name"].as_str())
            .filter(|s| !s.is_empty())?;
        let port = spec["port"].as_u64().and_then(|p| u16::try_from(p).ok())?;
        Some(Address {
            scheme: spec["scheme"].as_str().unwrap_or("http").to_string(),
            service: service.to_string(),
            port,
            path: spec["path"].as_str().unwrap_or("").to_string(),
        })
    }

    pub fn url(&self, namespace: &str) -> String {
        format!(
            "{}://{}.{}.svc.cluster.local:{}{}",
            self.scheme, self.service, namespace, self.port, self.path
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Delivered {
    Address(Address),
    Setting(String),
}

#[derive(Clone, Debug)]
pub struct OutputSpec {
    pub key: String,
    pub title: String,
    pub format: Format,
    pub source: Source,
    when: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Provided {
    pub kind: String,
    pub title: String,
    pub values: Vec<(String, Delivered)>,
}

impl Provided {
    pub fn url(&self, namespace: &str) -> Option<String> {
        self.values.iter().find_map(|(_, d)| match d {
            Delivered::Address(a) => Some(a.url(namespace)),
            Delivered::Setting(_) => None,
        })
    }

    pub fn deliver(&self, namespace: &str, settings: &Map<String, Value>) -> Map<String, Value> {
        self.values
            .iter()
            .filter_map(|(key, d)| {
                let value = match d {
                    Delivered::Address(a) => a.url(namespace),
                    Delivered::Setting(field) => match settings.get(field)? {
                        Value::String(s) if !s.is_empty() => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        _ => return None,
                    },
                };
                Some((key.clone(), Value::String(value)))
            })
            .collect()
    }
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
            .filter_map(|(key, spec)| match parse_output(key, spec) {
                Ok(output) => Some(output),
                Err(why) => {
                    tracing::warn!("output {key} is ignored: {why}");
                    None
                }
            })
            .collect()
    }

    pub fn provides(&self) -> Vec<Provided> {
        let Some(kinds) = self.document["x-yolab-provides"].as_object() else {
            return Vec::new();
        };
        kinds
            .iter()
            .filter_map(|(kind, spec)| match spec {
                Value::Array(keys) => self.provided_outputs(kind, keys),
                legacy => {
                    let Some(address) = Address::parse(legacy) else {
                        tracing::warn!("provided {kind} is ignored: it needs a service and a port");
                        return None;
                    };
                    Some(Provided {
                        kind: kind.clone(),
                        title: legacy["title"].as_str().unwrap_or(kind).to_string(),
                        values: vec![("url".to_string(), Delivered::Address(address))],
                    })
                }
            })
            .collect()
    }

    fn provided_outputs(&self, kind: &str, keys: &[Value]) -> Option<Provided> {
        let outputs = self.outputs();
        let mut values = Vec::new();
        for key in keys {
            let Some(output) = key
                .as_str()
                .and_then(|k| outputs.iter().find(|o| o.key == k))
            else {
                tracing::warn!("provided {kind} is ignored: it lists {key}, which is no output");
                return None;
            };
            let delivered = match &output.source {
                Source::Service(address) => Delivered::Address(address.clone()),
                Source::Config(field) => Delivered::Setting(field.clone()),
                Source::Logs(_) => {
                    tracing::warn!(
                        "provided {kind} is ignored: {} is only known once the app runs",
                        output.key
                    );
                    return None;
                }
            };
            values.push((output.key.clone(), delivered));
        }
        let title = values
            .first()
            .and_then(|(k, _)| outputs.iter().find(|o| &o.key == k))
            .map(|o| o.title.clone())
            .unwrap_or_else(|| kind.to_string());
        (!values.is_empty()).then(|| Provided {
            kind: kind.to_string(),
            title,
            values,
        })
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
                tracing::warn!(
                    "output {}: its `when` is not a valid schema ({e})",
                    self.key
                );
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
                let re =
                    Regex::new(pattern).map_err(|e| format!("its logs pattern is invalid: {e}"))?;
                if re.captures_len() < 2 {
                    return Err("its logs pattern has no capture group for the value".into());
                }
                Source::Logs(re)
            } else if let Some(field) = s.get("config").and_then(Value::as_str) {
                Source::Config(field.to_string())
            } else if let Some(service) = s.get("service") {
                Source::Service(
                    Address::parse(service).ok_or("its service source needs a name and a port")?,
                )
            } else {
                return Err("its source is not logs, config or service".into());
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
    fn a_provided_service_is_reached_by_its_in_cluster_address() {
        let app = AppSchema::new(json!({
            "x-yolab-provides": {
                "ollama": { "title": "Ollama API", "service": "ollama", "port": 11434 },
                "electrum": { "scheme": "tcp", "service": "electrs", "port": 50001 },
                "broken": { "service": "x" }
            }
        }));
        let provided = app.provides();
        assert_eq!(provided.len(), 2);
        let ollama = provided.iter().find(|p| p.kind == "ollama").unwrap();
        assert_eq!(ollama.title, "Ollama API");
        assert_eq!(
            ollama.url("yolab-ollama").as_deref(),
            Some("http://ollama.yolab-ollama.svc.cluster.local:11434")
        );
        let electrum = provided.iter().find(|p| p.kind == "electrum").unwrap();
        assert_eq!(
            electrum.url("yolab-node").as_deref(),
            Some("tcp://electrs.yolab-node.svc.cluster.local:50001")
        );
    }

    fn torrent_client() -> AppSchema {
        AppSchema::new(json!({
            "properties": {
                "config": { "type": "object", "properties": {
                    "username": { "type": "string", "default": "admin" },
                    "password": { "type": "string", "writeOnly": true, "generate": true },
                }},
                "outputs": { "type": "object", "properties": {
                    "api": { "title": "Web UI",
                             "source": { "service": { "name": "qbittorrent", "port": 8080 } } },
                    "username": { "title": "User", "source": { "config": "username" } },
                    "password": { "title": "Password", "format": "secret",
                                  "source": { "config": "password" } },
                    "onion": { "title": "Onion", "source": { "logs": "ONION (\\S+)" } },
                }},
            },
            "x-yolab-provides": {
                "torrent-client": ["api", "username", "password"],
                "needs-logs": ["onion"],
                "unknown-key": ["api", "nope"],
            },
        }))
    }

    #[test]
    fn a_provider_hands_over_its_address_and_settings_under_one_interface() {
        let provided = torrent_client().provides();
        assert_eq!(
            provided.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(),
            vec!["torrent-client"]
        );
        let client = &provided[0];
        assert_eq!(client.title, "Web UI");
        let settings = json!({ "username": "admin", "password": "s3cret" });
        assert_eq!(
            Value::Object(client.deliver("yolab-qbittorrent", settings.as_object().unwrap())),
            json!({
                "api": "http://qbittorrent.yolab-qbittorrent.svc.cluster.local:8080",
                "username": "admin",
                "password": "s3cret",
            })
        );
    }

    #[test]
    fn a_setting_the_provider_never_saved_is_left_out_rather_than_sent_empty() {
        let provided = torrent_client().provides();
        let settings = json!({ "username": "" });
        let delivered = provided[0].deliver("yolab-q", settings.as_object().unwrap());
        assert_eq!(
            delivered.keys().collect::<Vec<_>>(),
            vec!["api"],
            "{delivered:?}"
        );
    }

    #[test]
    fn a_service_output_is_parsed_and_a_broken_one_refused() {
        let service = json!({ "source": { "service": { "name": "x", "port": 80 } } });
        let ok = parse_output("api", &service).unwrap();
        assert!(matches!(ok.source, Source::Service(_)));
        assert!(parse_output("api", &json!({ "source": { "service": { "name": "x" } } })).is_err());
    }

    #[test]
    fn an_app_that_provides_nothing_lists_nothing() {
        assert!(AppSchema::new(Value::Null).provides().is_empty());
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
        assert_eq!(
            keys(&explorer_app().applicable_outputs(&off)),
            vec!["url", "password"]
        );
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
        assert!(parse_output("a", &json!({}))
            .unwrap_err()
            .contains("no source"));
        assert!(parse_output("a", &json!({ "source": { "logs": "x" } }))
            .unwrap_err()
            .contains("capture group"));
        assert!(
            parse_output("a", &json!({ "source": { "config": "p" }, "when": 3 }))
                .unwrap_err()
                .contains("when")
        );
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
