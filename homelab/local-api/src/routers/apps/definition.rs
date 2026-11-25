use super::*;

pub(crate) const DEFINITION_SCHEMA: u32 = 1;
pub(crate) const DEFINITION_SECRET_KEY: &str = "app.json";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
    pub name: String,
    pub capacity: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceSpec {
    #[serde(default)]
    pub cpu_millicores: u64,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub gpu: u64,
    #[serde(default)]
    pub replicas: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BackupPolicy {
    pub enabled: bool,
    pub schedule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<u64>,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: "0 3 1 * *".to_string(),
            budget_bytes: Some(crate::backup::points::DEFAULT_BUDGET),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AppDefinition {
    pub schema: u32,
    pub app_id: String,
    #[serde(default)]
    pub chart_repo: String,
    #[serde(default)]
    pub chart_version: String,
    pub instance_name: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub service_name: String,
    pub config: serde_json::Map<String, Value>,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    #[serde(default)]
    pub resources: ResourceSpec,
    #[serde(default)]
    pub backup: BackupPolicy,
}

impl AppDefinition {
    pub(crate) fn release(&self) -> &str {
        if self.release.is_empty() {
            &self.instance_name
        } else {
            &self.release
        }
    }
}

pub(crate) async fn write_definition(
    client: &Client,
    ns: &str,
    def: &AppDefinition,
    app: &crate::appschema::AppSchema,
) -> anyhow::Result<()> {
    let full = serde_json::to_string(def).expect("an app definition is plain data");
    let config_json = Value::Object(def.config.clone()).to_string();
    crate::k8s::apply(
        client,
        &crate::k8s::secret_manifest(
            CONFIG_SECRET,
            ns,
            &[
                (CONFIG_SECRET_KEY, config_json.as_str()),
                (DEFINITION_SECRET_KEY, full.as_str()),
            ],
            &[(LABEL_MANAGED, "true")],
        ),
    )
    .await?;
    let redacted = redact_credentials(&def.config, &app.credentials());
    annotate_ns(client, ns, ANN_CONFIG, &Value::Object(redacted).to_string()).await;
    let backup = serde_json::to_string(&def.backup).expect("a backup policy is plain data");
    annotate_ns(client, ns, ANN_BACKUP, &backup).await;
    crate::runtime::wake("outputs");
    Ok(())
}

pub(crate) async fn read_definition(client: &Client, ns: &str) -> anyhow::Result<AppDefinition> {
    read_definition_opt(client, ns)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} has no saved settings (no {CONFIG_SECRET} Secret)"))
}

pub(crate) async fn read_definition_opt(
    client: &Client,
    ns: &str,
) -> anyhow::Result<Option<AppDefinition>> {
    let Some(data) = crate::k8s::secret_data(client, ns, CONFIG_SECRET).await? else {
        return Ok(None);
    };
    if let Some(raw) = data.get(DEFINITION_SECRET_KEY) {
        if let Ok(def) = serde_json::from_str::<AppDefinition>(raw) {
            return Ok(Some(def));
        }
        tracing::warn!("{ns}: {DEFINITION_SECRET_KEY} is unreadable — falling back to annotations");
    }
    let config = parse_saved_config(ns, &data)?;
    let ns_v = namespace(client, ns).await?.unwrap_or(Value::Null);
    Ok(Some(definition_from_annotations(&ns_v, config)))
}

pub(crate) fn redact_definition(
    def: &AppDefinition,
    app: &crate::appschema::AppSchema,
) -> AppDefinition {
    let mut d = def.clone();
    d.config = redact_credentials(&def.config, &app.credentials());
    d
}

pub(crate) fn merge_credentials(
    mut incoming: serde_json::Map<String, Value>,
    stored: &serde_json::Map<String, Value>,
    app: &crate::appschema::AppSchema,
) -> serde_json::Map<String, Value> {
    for field in app.credentials() {
        let untouched = incoming.get(&field).and_then(|v| v.as_str()) == Some(REDACTED);
        if untouched {
            match stored.get(&field) {
                Some(kept) => {
                    incoming.insert(field, kept.clone());
                }
                None => {
                    incoming.remove(&field);
                }
            }
        }
    }
    incoming
}

pub(crate) fn definition_from_annotations(
    ns_v: &Value,
    config: serde_json::Map<String, Value>,
) -> AppDefinition {
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let get = |k: &str| {
        ann.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let name = ns_v["metadata"]["name"].as_str().unwrap_or("");
    AppDefinition {
        schema: DEFINITION_SCHEMA,
        app_id: get(ANN_APP_ID),
        chart_repo: get(ANN_CHART_REPO),
        chart_version: get(ANN_CHART_VERSION),
        instance_name: name.trim_start_matches("yolab-").to_string(),
        release: String::new(),
        service_name: String::new(),
        config,
        volumes: Vec::new(),
        resources: ResourceSpec::default(),
        backup: BackupPolicy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};
    use serde_json::json;

    const SECRET_PATH: &str = "/api/v1/namespaces/yolab-notes/secrets/yolab-config";
    const NS_PATH: &str = "/api/v1/namespaces/yolab-notes";

    fn schema_with_password() -> crate::appschema::AppSchema {
        crate::appschema::AppSchema::new(json!({
            "properties": { "config": { "properties": {
                "password": { "type": "string", "writeOnly": true },
                "title": { "type": "string" }
            } } }
        }))
    }

    fn config(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn definition() -> AppDefinition {
        AppDefinition {
            schema: DEFINITION_SCHEMA,
            app_id: "notes".into(),
            chart_repo: "yolab".into(),
            chart_version: "1.0.0".into(),
            instance_name: "notes".into(),
            release: String::new(),
            service_name: String::new(),
            config: config(json!({ "password": "hunter2", "title": "Mine" })),
            volumes: Vec::new(),
            resources: ResourceSpec::default(),
            backup: BackupPolicy::default(),
        }
    }

    fn secret(data: &[(&str, &str)]) -> Value {
        use base64::Engine as _;
        let data: serde_json::Map<String, Value> = data
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    Value::from(base64::engine::general_purpose::STANDARD.encode(v)),
                )
            })
            .collect();
        json!({ "apiVersion": "v1", "kind": "Secret", "metadata": { "name": CONFIG_SECRET }, "data": data })
    }

    #[test]
    fn the_release_is_the_service_name_unless_one_was_saved() {
        let mut d = definition();
        assert_eq!(d.release(), "notes");
        d.release = "notes-old".into();
        assert_eq!(d.release(), "notes-old");
    }

    #[tokio::test]
    async fn saving_a_definition_keeps_the_password_in_the_secret_but_not_on_the_namespace() {
        let (server, kube) = api_server().await;
        accept_patches(&server).await;
        write_definition(&kube, "yolab-notes", &definition(), &schema_with_password())
            .await
            .unwrap();
        let sent = patched(&server).await;
        let saved: AppDefinition = serde_json::from_str(
            sent[0]["stringData"][DEFINITION_SECRET_KEY]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved, definition());
        let shown: Value = serde_json::from_str(
            sent[1]["metadata"]["annotations"][ANN_CONFIG]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(shown, json!({ "password": REDACTED, "title": "Mine" }));
        assert!(sent[2]["metadata"]["annotations"][ANN_BACKUP].is_string());
    }

    #[tokio::test]
    async fn a_definition_that_cannot_be_saved_is_an_error() {
        let failed = write_definition(
            &crate::k8s::testing::unreachable(),
            "yolab-notes",
            &definition(),
            &schema_with_password(),
        )
        .await;
        assert!(failed.is_err());
    }

    #[tokio::test]
    async fn a_saved_definition_is_read_back_whole() {
        let (server, kube) = api_server().await;
        let full = serde_json::to_string(&definition()).unwrap();
        serve(
            &server,
            SECRET_PATH,
            200,
            secret(&[(DEFINITION_SECRET_KEY, &full), (CONFIG_SECRET_KEY, "{}")]),
        )
        .await;
        assert_eq!(
            read_definition(&kube, "yolab-notes").await.unwrap(),
            definition()
        );
    }

    #[tokio::test]
    async fn an_unreadable_definition_falls_back_to_the_settings_and_annotations() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            SECRET_PATH,
            200,
            secret(&[
                (DEFINITION_SECRET_KEY, "{broken"),
                (CONFIG_SECRET_KEY, r#"{"title":"Old"}"#),
            ]),
        )
        .await;
        serve(
            &server,
            NS_PATH,
            200,
            json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": {
                "name": "yolab-notes",
                "annotations": { ANN_APP_ID: "notes", ANN_CHART_VERSION: "0.9.0" }
            } }),
        )
        .await;
        let d = read_definition(&kube, "yolab-notes").await.unwrap();
        assert_eq!(d.app_id, "notes");
        assert_eq!(d.chart_version, "0.9.0");
        assert_eq!(d.instance_name, "notes");
        assert_eq!(d.config, config(json!({ "title": "Old" })));
    }

    #[tokio::test]
    async fn settings_saved_before_definitions_existed_are_read_without_a_namespace() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            SECRET_PATH,
            200,
            secret(&[(CONFIG_SECRET_KEY, "{}")]),
        )
        .await;
        serve(&server, NS_PATH, 404, status(404, "NotFound")).await;
        let d = read_definition(&kube, "yolab-notes").await.unwrap();
        assert_eq!(d.app_id, "");
        assert_eq!(d.instance_name, "");
    }

    #[tokio::test]
    async fn a_settings_secret_without_the_settings_is_an_error() {
        let (server, kube) = api_server().await;
        serve(&server, SECRET_PATH, 200, secret(&[("other", "x")])).await;
        let err = read_definition_opt(&kube, "yolab-notes").await.unwrap_err();
        assert!(err.to_string().contains("has no"), "{err}");
    }

    #[tokio::test]
    async fn old_settings_whose_namespace_cannot_be_read_are_an_error() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            SECRET_PATH,
            200,
            secret(&[(CONFIG_SECRET_KEY, "{}")]),
        )
        .await;
        serve(&server, NS_PATH, 500, status(500, "InternalError")).await;
        assert!(read_definition_opt(&kube, "yolab-notes").await.is_err());
    }

    #[tokio::test]
    async fn a_service_without_saved_settings_has_no_definition() {
        let (server, kube) = api_server().await;
        serve(&server, SECRET_PATH, 404, status(404, "NotFound")).await;
        assert_eq!(
            read_definition_opt(&kube, "yolab-notes").await.unwrap(),
            None
        );
        let err = read_definition(&kube, "yolab-notes").await.unwrap_err();
        assert!(err.to_string().contains("has no saved settings"), "{err}");
    }

    #[test]
    fn a_shown_definition_hides_the_password() {
        let shown = redact_definition(&definition(), &schema_with_password());
        assert_eq!(
            shown.config,
            config(json!({ "password": REDACTED, "title": "Mine" }))
        );
    }

    #[test]
    fn an_untouched_password_keeps_the_saved_one_or_is_dropped() {
        let app = schema_with_password();
        let incoming = config(json!({ "password": REDACTED, "title": "New" }));
        let kept = merge_credentials(
            incoming.clone(),
            &config(json!({ "password": "hunter2" })),
            &app,
        );
        assert_eq!(
            kept,
            config(json!({ "password": "hunter2", "title": "New" }))
        );
        let dropped = merge_credentials(incoming, &serde_json::Map::new(), &app);
        assert_eq!(dropped, config(json!({ "title": "New" })));
        let typed = merge_credentials(
            config(json!({ "password": "new" })),
            &config(json!({ "password": "old" })),
            &app,
        );
        assert_eq!(typed, config(json!({ "password": "new" })));
    }
}
