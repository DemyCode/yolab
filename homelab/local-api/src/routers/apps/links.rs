use super::*;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Map;

use crate::appschema::AppSchema;

const FORMAT: &str = "yolab-output";
const LEGACY_FORMAT: &str = "connection";
pub(crate) const FROM: &str = "$from";
const LEGACY_FROM: &str = "from";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkField {
    pub charts: Vec<String>,
    pub outputs: Vec<String>,
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl LinkField {
    fn of(spec: &Value) -> Option<LinkField> {
        let field = match spec["format"].as_str()? {
            FORMAT => LinkField {
                charts: strings(&spec["x-yolab-from"]),
                outputs: strings(&spec["x-yolab-outputs"]),
            },
            LEGACY_FORMAT => LinkField {
                charts: Vec::new(),
                outputs: spec["properties"]
                    .as_object()
                    .map(|p| p.keys().filter(|k| *k != LEGACY_FROM).cloned().collect())
                    .unwrap_or_default(),
            },
            _ => return None,
        };
        (!field.outputs.is_empty()).then_some(field)
    }
}

pub(crate) fn link_fields(config_schema: &Value) -> BTreeMap<String, LinkField> {
    fn walk(node: &Value, found: &mut BTreeMap<String, LinkField>) {
        match node {
            Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, spec) in props {
                        if let Some(field) = LinkField::of(spec) {
                            found.insert(name.clone(), field);
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
    let legacy = value[LEGACY_FROM].as_str().filter(|s| !s.is_empty())?;
    Some(
        legacy
            .strip_prefix("yolab-")
            .and_then(LinkRef::parse)
            .ok_or_else(|| format!("{legacy:?} is not an installed app")),
    )
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

pub(crate) fn may_link(field: &LinkField, link: &LinkRef, app_id: &str) -> bool {
    field.charts.is_empty()
        || match &link.member {
            Some(member) => field.charts.contains(member),
            None => field.charts.iter().any(|c| c == app_id),
        }
}

async fn provider_outputs(
    client: &Client,
    catalog: &std::path::Path,
    link: &LinkRef,
) -> anyhow::Result<(String, BTreeMap<String, String>)> {
    let ns = link.namespace();
    let found = namespace(client, &ns).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} is no longer installed — pick another one in this app's settings",
            link.describe()
        )
    })?;
    let app_id = found["metadata"]["annotations"][ANN_APP_ID]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let known = match known_outputs(client, catalog, &ns, true).await {
        Ok(known) => known,
        Err(_) => known_outputs(client, catalog, &ns, false).await?,
    };
    let config = match read_definition(client, &ns).await {
        Ok(definition) => definition.config,
        Err(_) => known.settings.clone(),
    };
    Ok((
        app_id,
        crate::outputs::values(&known.specs, &known.remembered, &config, &ns),
    ))
}

pub(crate) async fn resolve_links(
    client: &Client,
    catalog: &std::path::Path,
    app: &AppSchema,
    config: &Map<String, Value>,
) -> anyhow::Result<Map<String, Value>> {
    let mut resolved = config.clone();
    for (name, field) in link_fields(&app.config()) {
        let Some(choice) = chosen(config.get(&name)) else {
            continue;
        };
        let link = choice.map_err(|why| anyhow::anyhow!("{name}: {why}"))?;
        let (app_id, found) = provider_outputs(client, catalog, &link).await?;
        anyhow::ensure!(
            may_link(&field, &link, &app_id),
            "{name}: {} is not one of {} — pick another one in this app's settings",
            link.describe(),
            field.charts.join(", ")
        );
        let values = picked(&found, &link, &field.outputs).map_err(|missing| {
            anyhow::anyhow!(
                "{} has not shown its {} yet — open it once it is running, then apply this app's settings again",
                link.describe(),
                missing.join(", ")
            )
        })?;
        resolved.insert(name, Value::Object(values));
    }
    Ok(resolved)
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct LinkCandidate {
    pub reference: String,
    pub instance: String,
    pub member: Option<String>,
    pub app_id: String,
}

pub(crate) struct OfferedOutputs {
    pub namespace: String,
    pub app_id: String,
    pub keys: BTreeSet<String>,
}

pub(crate) fn candidates(
    apps: &[OfferedOutputs],
    charts: &[String],
    outputs: &[String],
) -> Vec<LinkCandidate> {
    let mut found = Vec::new();
    for app in apps {
        let Some(instance) = app.namespace.strip_prefix("yolab-") else {
            continue;
        };
        let offers = |prefix: &str| {
            !outputs.is_empty()
                && outputs
                    .iter()
                    .all(|o| app.keys.contains(&format!("{prefix}{o}")))
        };
        if (charts.is_empty() || charts.contains(&app.app_id)) && offers("") {
            found.push(LinkCandidate {
                reference: instance.to_string(),
                instance: instance.to_string(),
                member: None,
                app_id: app.app_id.clone(),
            });
        }
        for member in charts {
            if offers(&format!("{member}_")) {
                found.push(LinkCandidate {
                    reference: format!("{instance}/{member}"),
                    instance: instance.to_string(),
                    member: Some(member.clone()),
                    app_id: app.app_id.clone(),
                });
            }
        }
    }
    found.sort_by(|a, b| a.reference.cmp(&b.reference));
    found
}

fn listed(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Deserialize)]
pub struct LinkQuery {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub outputs: String,
}

pub async fn list_links(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<LinkQuery>,
) -> Result<Json<Vec<LinkCandidate>>> {
    let client = state.kube.client().await?;
    let catalog = state.config.catalog_dir();
    let mut apps = Vec::new();
    for app in installed_apps(&client).await? {
        let schema = installed_schema(&client, &app.namespace, &app.app_id, &catalog).await;
        apps.push(OfferedOutputs {
            keys: schema
                .applicable_outputs(&app.settings)
                .into_iter()
                .map(|o| o.key)
                .collect(),
            namespace: app.namespace,
            app_id: app.app_id,
        });
    }
    Ok(Json(candidates(
        &apps,
        &listed(&query.from),
        &listed(&query.outputs),
    )))
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
                    "format": "yolab-output",
                    "x-yolab-from": ["qbittorrent"],
                    "x-yolab-outputs": ["internal_url", "password"],
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
    fn a_link_field_names_the_charts_and_outputs_it_takes() {
        assert_eq!(
            link_fields(&radarr().config())
                .into_iter()
                .collect::<Vec<_>>(),
            vec![(
                "download_client".to_string(),
                LinkField {
                    charts: strings(&["qbittorrent"]),
                    outputs: strings(&["internal_url", "password"]),
                }
            )]
        );
    }

    #[test]
    fn a_link_that_takes_no_outputs_is_no_link() {
        let empty = json!({ "properties": { "x": { "format": "yolab-output" } } });
        assert!(link_fields(&empty).is_empty());
    }

    #[test]
    fn a_link_behind_a_switch_is_found() {
        let schema = json!({
            "properties": { "mode": { "type": "string" } },
            "dependencies": { "mode": { "oneOf": [{ "properties": {
                "existing_client": {
                    "format": "yolab-output",
                    "x-yolab-from": ["qbittorrent"],
                    "x-yolab-outputs": ["internal_url"],
                }
            }}]}}
        });
        assert!(link_fields(&schema).contains_key("existing_client"));
    }

    #[test]
    fn an_old_connection_field_takes_the_outputs_it_has_boxes_for_from_any_app() {
        let schema = json!({ "properties": { "ollama": {
            "type": "object",
            "format": "connection",
            "x-yolab-requires": "ollama",
            "properties": { "from": {}, "api": {} },
        }}});
        assert_eq!(
            link_fields(&schema)["ollama"],
            LinkField {
                charts: vec![],
                outputs: strings(&["api"])
            }
        );
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
            pick(json!({ "from": "kube-system" })),
            Some(Err(_))
        ));
        assert_eq!(pick(json!({ "from": "", "api": "http://x" })), None);
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

    #[test]
    fn a_link_only_reaches_the_charts_it_names() {
        let field = LinkField {
            charts: strings(&["qbittorrent"]),
            outputs: strings(&["password"]),
        };
        assert!(may_link(&field, &reference("q"), "qbittorrent"));
        assert!(!may_link(&field, &reference("t"), "transmission"));
        assert!(may_link(
            &field,
            &reference("media/qbittorrent"),
            "movies-tv"
        ));
        assert!(!may_link(&field, &reference("media/sonarr"), "movies-tv"));
        let any = LinkField {
            charts: vec![],
            outputs: strings(&["api"]),
        };
        assert!(may_link(&any, &reference("x"), "whatever"));
    }

    fn offered(namespace: &str, app_id: &str, keys: &[&str]) -> OfferedOutputs {
        OfferedOutputs {
            namespace: namespace.into(),
            app_id: app_id.into(),
            keys: keys.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn standalone_apps_and_apps_inside_packs_are_both_offered() {
        let apps = vec![
            offered("yolab-q", "qbittorrent", &["internal_url", "password"]),
            offered(
                "yolab-media",
                "movies-tv",
                &[
                    "qbittorrent_internal_url",
                    "qbittorrent_password",
                    "sonarr_api_key",
                ],
            ),
            offered("yolab-t", "transmission", &["internal_url", "password"]),
            offered("yolab-half", "qbittorrent", &["internal_url"]),
        ];
        let found = candidates(
            &apps,
            &strings(&["qbittorrent"]),
            &strings(&["internal_url", "password"]),
        );
        assert_eq!(
            found
                .iter()
                .map(|c| c.reference.as_str())
                .collect::<Vec<_>>(),
            vec!["media/qbittorrent", "q"]
        );
        assert_eq!(found[0].app_id, "movies-tv");
        assert_eq!(found[0].member.as_deref(), Some("qbittorrent"));
    }

    #[test]
    fn asking_for_no_outputs_offers_nothing() {
        let apps = vec![offered("yolab-q", "qbittorrent", &["internal_url"])];
        assert!(candidates(&apps, &strings(&["qbittorrent"]), &[]).is_empty());
    }

    #[test]
    fn a_query_lists_its_names_without_blanks() {
        assert_eq!(listed(" a, b ,,"), strings(&["a", "b"]));
        assert!(listed("").is_empty());
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
