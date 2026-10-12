use super::*;

use std::collections::BTreeMap;

use serde_json::Map;

use crate::appschema::AppSchema;

pub(crate) const FROM: &str = "$from";
const LEGACY_FROM: &str = "from";

pub(crate) fn field_outputs(config_schema: &Value) -> BTreeMap<String, Vec<String>> {
    fn walk(node: &Value, found: &mut BTreeMap<String, Vec<String>>) {
        match node {
            Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, spec) in props {
                        let boxes: Vec<String> = spec["properties"]
                            .as_object()
                            .map(|p| {
                                p.keys()
                                    .filter(|k| *k != FROM && *k != LEGACY_FROM)
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !boxes.is_empty() {
                            found.insert(name.clone(), boxes);
                        }
                    }
                }
                for child in map.values() {
                    walk(child, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, found)),
            _ => {}
        }
    }
    let mut found = BTreeMap::new();
    walk(config_schema, &mut found);
    found
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkRef {
    pub instance: String,
    pub member: Option<String>,
}

impl LinkRef {
    pub(crate) fn parse(text: &str) -> Option<LinkRef> {
        let (instance, member) = match text.split_once('/') {
            Some((instance, member)) => (instance, Some(member)),
            None => (text, None),
        };
        let fine =
            install::is_instance_name(instance) && member.is_none_or(install::is_instance_name);
        fine.then(|| LinkRef {
            instance: instance.to_string(),
            member: member.map(str::to_string),
        })
    }

    pub(crate) fn namespace(&self) -> String {
        format!("yolab-{}", self.instance)
    }

    pub(crate) fn key(&self, output: &str) -> String {
        match &self.member {
            Some(member) => format!("{member}_{output}"),
            None => output.to_string(),
        }
    }

    fn describe(&self) -> String {
        match &self.member {
            Some(member) => format!("{member} in {}", self.instance),
            None => self.instance.clone(),
        }
    }
}

pub(crate) fn chosen(value: Option<&Value>) -> Option<std::result::Result<LinkRef, String>> {
    let value = value?;
    if let Some(text) = value[FROM].as_str().filter(|s| !s.is_empty()) {
        return Some(
            LinkRef::parse(text).ok_or_else(|| format!("{text:?} is not an installed app")),
        );
    }
    let legacy = value[LEGACY_FROM].as_str()?;
    let instance = legacy.strip_prefix("yolab-")?;
    Some(LinkRef::parse(instance).ok_or_else(|| format!("{legacy:?} is not an installed app")))
}

pub(crate) fn picked(
    found: &BTreeMap<String, String>,
    link: &LinkRef,
    wanted: &[String],
) -> std::result::Result<Map<String, Value>, Vec<String>> {
    let mut values = Map::new();
    let mut missing = Vec::new();
    for output in wanted {
        match found.get(&link.key(output)) {
            Some(value) => {
                values.insert(output.clone(), Value::String(value.clone()));
            }
            None => missing.push(output.clone()),
        }
    }
    if missing.is_empty() {
        Ok(values)
    } else {
        Err(missing)
    }
}

async fn provider_outputs(
    client: &Client,
    catalog: &std::path::Path,
    link: &LinkRef,
) -> anyhow::Result<BTreeMap<String, String>> {
    let ns = link.namespace();
    if namespace(client, &ns).await?.is_none() {
        anyhow::bail!(
            "{} is no longer installed — type its address in this app's settings instead",
            link.describe()
        );
    }
    let known = match known_outputs(client, catalog, &ns, true).await {
        Ok(known) => known,
        Err(_) => known_outputs(client, catalog, &ns, false).await?,
    };
    let config = match read_definition(client, &ns).await {
        Ok(definition) => definition.config,
        Err(_) => known.settings.clone(),
    };
    Ok(crate::outputs::values(
        &known.specs,
        &known.remembered,
        &config,
        &ns,
    ))
}

pub(crate) async fn resolve_links(
    client: &Client,
    catalog: &std::path::Path,
    app: &AppSchema,
    config: &Map<String, Value>,
) -> anyhow::Result<Map<String, Value>> {
    let mut resolved = config.clone();
    let boxes = field_outputs(&app.config());
    for (name, value) in config {
        let Some(choice) = chosen(Some(value)) else {
            continue;
        };
        let link = choice.map_err(|why| anyhow::anyhow!("{name}: {why}"))?;
        let outputs = boxes.get(name).ok_or_else(|| {
            anyhow::anyhow!("{name}: fill it in by hand in this app's settings")
        })?;
        let found = provider_outputs(client, catalog, &link).await?;
        let values = picked(&found, &link, outputs).map_err(|missing| {
            anyhow::anyhow!(
                "{} has not shown its {} yet — open it once it is running, then apply this app's settings again",
                link.describe(),
                missing.join(", ")
            )
        })?;
        resolved.insert(name.clone(), Value::Object(values));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn radarr() -> AppSchema {
        AppSchema::new(json!({
            "properties": { "config": { "type": "object", "properties": {
                "download_client": {
                    "type": "object",
                    "properties": {
                        "internal_url": { "type": "string" },
                        "password": { "type": "string" },
                    },
                },
                "subdomain": { "type": "string" },
            }}}
        }))
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn text(link: &LinkRef) -> String {
        match &link.member {
            Some(member) => format!("{}/{member}", link.instance),
            None => link.instance.clone(),
        }
    }

    fn reference(text: &str) -> LinkRef {
        LinkRef::parse(text).unwrap()
    }

    #[test]
    fn a_link_saved_before_links_were_removed_takes_the_boxes_its_field_has() {
        assert_eq!(
            field_outputs(&radarr().config())
                .into_iter()
                .collect::<Vec<_>>(),
            vec![(
                "download_client".to_string(),
                strings(&["internal_url", "password"])
            )]
        );
    }

    #[test]
    fn a_field_behind_a_switch_is_found() {
        let schema = json!({
            "properties": { "mode": { "type": "string" } },
            "dependencies": { "mode": { "oneOf": [{ "properties": {
                "existing_client": { "properties": { "internal_url": {} } }
            }}]}}
        });
        assert_eq!(
            field_outputs(&schema)["existing_client"],
            strings(&["internal_url"])
        );
    }

    #[test]
    fn an_old_connection_field_never_counts_its_from_box_as_an_output() {
        let schema = json!({ "properties": { "ollama": {
            "type": "object",
            "properties": { "from": {}, "api": {} },
        }}});
        assert_eq!(field_outputs(&schema)["ollama"], strings(&["api"]));
    }

    #[test]
    fn a_plain_field_has_no_boxes() {
        let schema = json!({ "properties": { "subdomain": { "type": "string" } } });
        assert!(field_outputs(&schema).is_empty());
    }

    #[test]
    fn a_reference_names_an_app_or_one_app_of_a_pack() {
        assert_eq!(
            reference("media-ab12"),
            LinkRef {
                instance: "media-ab12".into(),
                member: None
            }
        );
        let member = reference("media-ab12/sonarr");
        assert_eq!(member.member.as_deref(), Some("sonarr"));
        assert_eq!(member.namespace(), "yolab-media-ab12");
        assert_eq!(text(&member), "media-ab12/sonarr");
        assert!(LinkRef::parse("../kube-system").is_none());
        assert!(LinkRef::parse("media/../x").is_none());
        assert!(LinkRef::parse("").is_none());
    }

    #[test]
    fn a_pack_s_app_is_found_under_its_name_prefixed_outputs() {
        assert_eq!(reference("media/sonarr").key("api_key"), "sonarr_api_key");
        assert_eq!(reference("sonarr-1").key("api_key"), "api_key");
    }

    #[test]
    fn only_a_value_naming_an_app_is_a_choice() {
        let pick = |v: Value| chosen(Some(&v)).map(|r| r.map(|l| text(&l)));
        assert_eq!(pick(json!({ "$from": "q" })), Some(Ok("q".into())));
        assert_eq!(
            pick(json!({ "$from": "media/qbittorrent" })),
            Some(Ok("media/qbittorrent".into()))
        );
        assert_eq!(pick(json!({ "internal_url": "http://nas:8080" })), None);
        assert_eq!(pick(json!({ "$from": "" })), None);
        assert!(matches!(
            pick(json!({ "$from": "Kube System" })),
            Some(Err(_))
        ));
        assert_eq!(chosen(None), None);
    }

    #[test]
    fn a_choice_saved_by_the_old_connections_still_counts() {
        let pick = |v: Value| chosen(Some(&v)).map(|r| r.map(|l| text(&l)));
        assert_eq!(
            pick(json!({ "from": "yolab-ollama" })),
            Some(Ok("ollama".into()))
        );
        assert!(matches!(
            pick(json!({ "from": "yolab-Kube System" })),
            Some(Err(_))
        ));
        assert_eq!(pick(json!({ "from": "", "api": "http://x" })), None);
        assert_eq!(
            pick(json!({ "from": "me@example.com", "host": "smtp.example.com" })),
            None,
            "a mail sender is a setting, not a link"
        );
    }

    #[test]
    fn the_chart_gets_exactly_the_outputs_it_asked_for() {
        let found: BTreeMap<String, String> = [
            (
                "qbittorrent_internal_url",
                "http://qbittorrent.yolab-media.svc.cluster.local:8080",
            ),
            ("qbittorrent_password", "p"),
            ("sonarr_api_key", "k"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let values = picked(
            &found,
            &reference("media/qbittorrent"),
            &strings(&["internal_url", "password"]),
        )
        .unwrap();
        assert_eq!(
            Value::Object(values),
            json!({
                "internal_url": "http://qbittorrent.yolab-media.svc.cluster.local:8080",
                "password": "p",
            })
        );
    }

    #[test]
    fn an_output_not_shown_yet_is_named_rather_than_sent_empty() {
        let found = BTreeMap::from([("internal_url".to_string(), "http://q".to_string())]);
        assert_eq!(
            picked(
                &found,
                &reference("q"),
                &strings(&["internal_url", "password"])
            ),
            Err(strings(&["password"]))
        );
    }

    mod against_kubernetes {
        use super::*;
        use crate::k8s::testing::{api_server, serve, status};

        #[tokio::test]
        async fn a_value_typed_by_hand_is_left_exactly_as_typed() {
            let (_server, client) = api_server().await;
            let config = json!({
                "download_client": { "internal_url": "http://nas:8080", "password": "p" },
            });
            let resolved = resolve_links(
                &client,
                std::path::Path::new("/nonexistent"),
                &radarr(),
                config.as_object().unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(Value::Object(resolved), config);
        }

        #[tokio::test]
        async fn a_removed_app_stops_the_apply_and_says_what_to_do() {
            let (server, client) = api_server().await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-qbittorrent",
                404,
                status(404, "NotFound"),
            )
            .await;
            let config = json!({ "download_client": { "$from": "qbittorrent" } });
            let e = resolve_links(
                &client,
                std::path::Path::new("/nonexistent"),
                &radarr(),
                config.as_object().unwrap(),
            )
            .await
            .unwrap_err();
            assert!(format!("{e:#}").contains("no longer installed"), "{e:#}");
            assert!(format!("{e:#}").contains("type its address"), "{e:#}");
        }

        #[tokio::test]
        async fn a_name_that_is_not_an_app_never_reaches_kubernetes() {
            let (server, client) = api_server().await;
            let config = json!({ "download_client": { "$from": "../kube-system" } });
            assert!(resolve_links(
                &client,
                std::path::Path::new("/nonexistent"),
                &radarr(),
                config.as_object().unwrap(),
            )
            .await
            .is_err());
            assert!(server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty());
        }
    }
}
