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
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: "0 3 * * *".to_string(),
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
    let full = serde_json::to_string(def)?;
    let config_json = serde_json::to_string(&def.config)?;
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
    annotate_ns(client, ns, ANN_CONFIG, &serde_json::to_string(&redacted)?).await;
    annotate_ns(client, ns, ANN_BACKUP, &serde_json::to_string(&def.backup)?).await;
    crate::runtime::wake("outputs");
    Ok(())
}

pub(crate) async fn read_definition(client: &Client, ns: &str) -> anyhow::Result<AppDefinition> {
    let data = crate::k8s::secret_data(client, ns, CONFIG_SECRET)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} has no saved settings (no {CONFIG_SECRET} Secret)"))?;
    if let Some(raw) = data.get(DEFINITION_SECRET_KEY) {
        if let Ok(def) = serde_json::from_str::<AppDefinition>(raw) {
            return Ok(def);
        }
        tracing::warn!("{ns}: {DEFINITION_SECRET_KEY} is unreadable — falling back to annotations");
    }
    let config = parse_saved_config(ns, &data)?;
    let ns_v = namespace(client, ns).await?.unwrap_or(Value::Null);
    Ok(definition_from_annotations(&ns_v, config))
}

pub(crate) async fn read_definition_opt(client: &Client, ns: &str) -> Option<AppDefinition> {
    read_definition(client, ns).await.ok()
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
