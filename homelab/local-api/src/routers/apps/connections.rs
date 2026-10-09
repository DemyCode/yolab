use super::*;

use std::collections::BTreeMap;

use serde_json::Map;

use crate::appschema::AppSchema;

const FORMAT: &str = "connection";
const FROM: &str = "from";

pub(crate) fn connection_fields(config_schema: &Value) -> BTreeMap<String, String> {
    fn walk(node: &Value, found: &mut BTreeMap<String, String>) {
        match node {
            Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, spec) in props {
                        let wants = spec["x-yolab-requires"].as_str().filter(|w| !w.is_empty());
                        if let (Some(FORMAT), Some(wants)) = (spec["format"].as_str(), wants) {
                            found.insert(name.clone(), wants.to_string());
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

pub(crate) fn provider_of(value: Option<&Value>) -> Option<&str> {
    value?[FROM].as_str().filter(|from| !from.is_empty())
}

fn connected(from: &str, delivered: Map<String, Value>) -> Value {
    let mut value = delivered;
    value.insert(FROM.to_string(), Value::String(from.to_string()));
    Value::Object(value)
}

async fn provided_by(
    client: &Client,
    catalog: &std::path::Path,
    provider: &str,
) -> anyhow::Result<(String, Vec<crate::appschema::Provided>, Map<String, Value>)> {
    let found = namespace(client, provider).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} is no longer installed — pick another one in this app's settings",
            provider.trim_start_matches("yolab-")
        )
    })?;
    let app_id = found["metadata"]["annotations"][ANN_APP_ID]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let schema = installed_schema(client, provider, &app_id, catalog).await;
    let settings = read_config(client, provider).await.unwrap_or_default();
    Ok((app_id, schema.provides(), settings))
}

pub(crate) async fn resolve_connections(
    client: &Client,
    catalog: &std::path::Path,
    app: &AppSchema,
    config: &Map<String, Value>,
) -> anyhow::Result<Map<String, Value>> {
    let mut resolved = config.clone();
    for (field, wants) in connection_fields(&app.config()) {
        let Some(from) = provider_of(config.get(&field)) else {
            continue;
        };
        anyhow::ensure!(
            install::is_app_namespace(from),
            "{field} names {from:?}, which is not an installed app"
        );
        let (app_id, provided, settings) = provided_by(client, catalog, from).await?;
        let offered = provided
            .into_iter()
            .find(|p| p.kind == wants)
            .ok_or_else(|| {
                anyhow::anyhow!(
                "{} ({app_id}) does not offer {wants} — pick another one in this app's settings",
                from.trim_start_matches("yolab-")
            )
            })?;
        resolved.insert(field, connected(from, offered.deliver(from, &settings)));
    }
    Ok(resolved)
}

pub(crate) async fn everything_provided(
    client: &Client,
    catalog: &std::path::Path,
    provider: &str,
) -> BTreeMap<String, Map<String, Value>> {
    match provided_by(client, catalog, provider).await {
        Ok((_, provided, settings)) => provided
            .into_iter()
            .map(|p| (p.kind.clone(), p.deliver(provider, &settings)))
            .collect(),
        Err(_) => BTreeMap::new(),
    }
}

pub(crate) fn uses(app: &AppSchema, config: &Map<String, Value>, provider: &str) -> bool {
    connection_fields(&app.config())
        .keys()
        .any(|field| provider_of(config.get(field)) == Some(provider))
}

pub(crate) async fn consumers_of(
    client: &Client,
    catalog: &std::path::Path,
    provider: &str,
) -> anyhow::Result<Vec<String>> {
    let mut consumers = Vec::new();
    for app in installed_apps(client).await? {
        if app.namespace == provider {
            continue;
        }
        let schema = installed_schema(client, &app.namespace, &app.app_id, catalog).await;
        if uses(&schema, &app.settings, provider) {
            consumers.push(app.namespace.trim_start_matches("yolab-").to_string());
        }
    }
    Ok(consumers)
}

pub(crate) async fn stored_upgrade_plan(
    client: &Client,
    instance_name: &str,
) -> anyhow::Result<install::UpgradePlan> {
    let ns = format!("yolab-{instance_name}");
    let found = namespace(client, &ns)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{instance_name} is no longer installed"))?;
    let annotation = |key: &str| {
        found["metadata"]["annotations"][key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let stored = read_definition_opt(client, &ns).await;
    Ok(install::UpgradePlan {
        app_id: annotation(ANN_APP_ID)
            .ok_or_else(|| anyhow::anyhow!("{instance_name} does not say which app it is"))?,
        release: stored
            .as_ref()
            .map(|d| d.release().to_string())
            .unwrap_or_else(|| instance_name.to_string()),
        instance_name: instance_name.to_string(),
        config: read_config(client, &ns).await?,
        chart_repo: annotation(ANN_CHART_REPO),
        backup: stored.map(|d| d.backup).unwrap_or_default(),
        keep_version: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn consumer() -> AppSchema {
        AppSchema::new(json!({
            "properties": { "config": { "type": "object", "properties": {
                "download_client": {
                    "type": "object",
                    "format": "connection",
                    "x-yolab-requires": "torrent-client",
                },
                "subdomain": { "type": "string" },
            }}}
        }))
    }

    #[test]
    fn connection_fields_name_the_interface_they_want() {
        let fields = connection_fields(&consumer().config());
        assert_eq!(
            fields.into_iter().collect::<Vec<_>>(),
            vec![("download_client".to_string(), "torrent-client".to_string())]
        );
        let unnamed = json!({ "properties": { "x": { "format": "connection" } } });
        assert!(connection_fields(&unnamed).is_empty());
    }

    #[test]
    fn only_a_field_pointing_at_an_app_has_a_provider() {
        assert_eq!(
            provider_of(Some(&json!({ "from": "yolab-qbittorrent" }))),
            Some("yolab-qbittorrent")
        );
        let typed = json!({ "from": "", "api": "http://nas:8080" });
        assert_eq!(provider_of(Some(&typed)), None);
        assert_eq!(provider_of(Some(&json!("http://old-style-url"))), None);
        assert_eq!(provider_of(None), None);
    }

    #[test]
    fn the_chart_gets_the_providers_values_and_where_they_came_from() {
        let delivered = json!({
            "api": "http://q.yolab-q.svc.cluster.local:8080",
            "password": "p",
        })
        .as_object()
        .cloned()
        .unwrap();
        assert_eq!(
            connected("yolab-q", delivered),
            json!({
                "from": "yolab-q",
                "api": "http://q.yolab-q.svc.cluster.local:8080",
                "password": "p",
            })
        );
    }

    #[test]
    fn an_app_uses_a_provider_only_through_a_connection_field() {
        let config = json!({
            "download_client": { "from": "yolab-qbittorrent" },
            "subdomain": "yolab-qbittorrent",
        });
        let config = config.as_object().unwrap();
        assert!(uses(&consumer(), config, "yolab-qbittorrent"));
        assert!(!uses(&consumer(), config, "yolab-transmission"));
    }

    mod against_kubernetes {
        use super::*;
        use crate::k8s::testing::{api_server, serve, status};

        #[tokio::test]
        async fn a_field_typed_by_hand_is_left_exactly_as_typed() {
            let (_server, client) = api_server().await;
            let config = json!({
                "download_client": { "from": "", "api": "http://nas:8080" },
            });
            let resolved = resolve_connections(
                &client,
                std::path::Path::new("/nonexistent"),
                &consumer(),
                config.as_object().unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(Value::Object(resolved), config);
        }

        #[tokio::test]
        async fn a_removed_provider_stops_the_install_and_says_what_to_do() {
            let (server, client) = api_server().await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-qbittorrent",
                404,
                status(404, "NotFound"),
            )
            .await;
            let config = json!({ "download_client": { "from": "yolab-qbittorrent" } });
            let e = resolve_connections(
                &client,
                std::path::Path::new("/nonexistent"),
                &consumer(),
                config.as_object().unwrap(),
            )
            .await
            .unwrap_err();
            assert!(format!("{e:#}").contains("no longer installed"), "{e:#}");
        }

        #[tokio::test]
        async fn a_name_that_is_not_an_app_never_reaches_kubernetes() {
            let (server, client) = api_server().await;
            let config = json!({ "download_client": { "from": "kube-system" } });
            assert!(resolve_connections(
                &client,
                std::path::Path::new("/nonexistent"),
                &consumer(),
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
