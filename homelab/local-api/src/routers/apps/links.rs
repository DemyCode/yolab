use serde_json::{Map, Value};

const FROM: &str = "$from";
const LEGACY_FROM: &str = "from";

fn is_link_key(key: &str, value: &Value) -> bool {
    match key {
        FROM => value.as_str().is_some_and(|s| !s.is_empty()),
        LEGACY_FROM => value.as_str().is_some_and(|s| s.starts_with("yolab-")),
        _ => false,
    }
}

pub(crate) fn drop_links(config: &Map<String, Value>) -> Map<String, Value> {
    config
        .iter()
        .filter_map(|(name, value)| {
            let Some(fields) = value.as_object() else {
                return Some((name.clone(), value.clone()));
            };
            if !fields.iter().any(|(k, v)| is_link_key(k, v)) {
                return Some((name.clone(), value.clone()));
            }
            let kept: Map<String, Value> = fields
                .iter()
                .filter(|(k, v)| !is_link_key(k, v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            (!kept.is_empty()).then(|| (name.clone(), Value::Object(kept)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dropped(config: Value) -> Value {
        Value::Object(drop_links(config.as_object().unwrap()))
    }

    #[test]
    fn a_saved_link_to_another_app_is_dropped_never_resolved() {
        assert_eq!(
            dropped(json!({
                "download_client": { "$from": "qbittorrent-1" },
                "subdomain": "sonarr",
            })),
            json!({ "subdomain": "sonarr" })
        );
    }

    #[test]
    fn an_old_connection_keeps_what_was_typed_and_loses_only_the_link() {
        assert_eq!(
            dropped(json!({ "ollama": { "from": "yolab-ollama", "api": "http://x:11434" } })),
            json!({ "ollama": { "api": "http://x:11434" } })
        );
    }

    #[test]
    fn values_typed_by_hand_are_left_exactly_as_typed() {
        let typed = json!({
            "download_client": { "internal_url": "http://nas:8080", "password": "p" },
            "smtp": { "from": "me@example.com", "host": "smtp.example.com" },
            "empty": {},
            "port": 8080,
        });
        assert_eq!(
            dropped(typed.clone()),
            typed,
            "a mail sender is a setting, not a link"
        );
    }
}
