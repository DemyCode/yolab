use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    Json,
};
use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Outcome;
use crate::routers::backup_common::Backend;
use crate::routers::install;
use crate::{config::Config, error::Result, AppState};

const LABEL_MANAGED: &str = "yolab.io/managed";
pub(crate) const ANN_APP_ID: &str = "yolab.io/app-id";
pub(crate) const ANN_CHART_VERSION: &str = "yolab.io/chart-version";
pub(crate) const ANN_CHART_REPO: &str = "yolab.io/chart-repo";

const ANN_CONFIG: &str = "yolab.io/config";
const ANN_BACKUP: &str = "yolab.io/backup";
const ANN_UNINSTALLING: &str = "yolab.io/uninstalling";
const UNINSTALL_LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const LOGS_FOLLOW_TAIL: u32 = 100;

#[derive(Serialize)]
pub struct AppInfo {
    pub app_id: String,
    pub instance_name: String,
    pub instance_id: Option<String>,
    pub status: String,
    pub detail: String,
    pub outputs: Vec<crate::outputs::ShownOutput>,
    pub config: serde_json::Map<String, Value>,
    pub backup: AppBackupStatus,
}

pub(crate) struct InstalledApp {
    pub namespace: String,
    pub app_id: String,
    pub settings: serde_json::Map<String, Value>,
    pub annotations: serde_json::Map<String, Value>,
}

#[derive(Serialize)]
pub struct AppBackupStatus {
    pub enabled: bool,
    pub schedule: String,
    pub last_ok_at: Option<String>,
    pub running: bool,
}

impl Default for AppBackupStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: "0 3 * * *".to_string(),
            last_ok_at: None,
            running: false,
        }
    }
}

#[derive(Serialize)]
pub struct CatalogApp {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub description: String,
    pub home: String,
    pub icon: String,
    pub category: String,
    pub chart_version: String,
    pub schema: Value,
}

#[derive(Serialize)]
pub struct PodInfo {
    pub name: String,
    pub phase: String,
    pub ready: bool,
}

#[derive(Serialize)]
pub struct OutputsResponse {
    pub outputs: Vec<crate::outputs::ShownOutput>,
}

#[derive(Serialize)]
pub struct DomainResponse {
    pub domain: String,
}

#[derive(Deserialize)]
pub struct InstallRequest {
    pub instance_name: String,
    pub config: serde_json::Map<String, Value>,
    #[serde(default)]
    pub source: Option<install::InstallSource>,
}

fn namespace_ref(ns: &str) -> Value {
    crate::k8s::cluster_reference("v1", "Namespace", ns)
}

async fn namespace(client: &Client, ns: &str) -> anyhow::Result<Option<Value>> {
    crate::k8s::get(client, &namespace_ref(ns)).await
}

async fn annotate_ns(client: &Client, ns: &str, key: &str, value: &str) {
    let mut patch = namespace_ref(ns);
    patch["metadata"]["annotations"] = serde_json::json!({ key: value });
    if let Err(e) = crate::k8s::merge_patch(client, &patch).await {
        tracing::warn!("annotate {ns} {key} failed: {e}");
    }
}

const CONFIG_SECRET: &str = "yolab-config";
const CONFIG_SECRET_KEY: &str = "config.json";
const REDACTED: &str = "__redacted__";

fn redact_credentials(
    config: &serde_json::Map<String, Value>,
    credentials: &std::collections::HashSet<String>,
) -> serde_json::Map<String, Value> {
    config
        .iter()
        .map(|(k, v)| {
            if credentials.contains(k) {
                (k.clone(), Value::String(REDACTED.to_string()))
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

async fn read_config(client: &Client, ns: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
    let data = crate::k8s::secret_data(client, ns, CONFIG_SECRET)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} has no saved settings (no {CONFIG_SECRET} Secret)"))?;
    parse_saved_config(ns, &data)
}

fn parse_saved_config(
    ns: &str,
    data: &std::collections::HashMap<String, String>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let raw = data.get(CONFIG_SECRET_KEY).ok_or_else(|| {
        anyhow::anyhow!("{ns}: the {CONFIG_SECRET} Secret has no {CONFIG_SECRET_KEY}")
    })?;
    serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("{ns}: the saved settings are unreadable: {e}"))
}

pub(crate) const DEFINITION_SCHEMA: u32 = 1;
const DEFINITION_SECRET_KEY: &str = "app.json";

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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppDefinition {
    pub schema: u32,
    pub app_id: String,
    #[serde(default)]
    pub chart_repo: String,
    #[serde(default)]
    pub chart_version: String,
    pub instance_name: String,
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
    record_definition(ns, def);
    let redacted = redact_credentials(&def.config, &app.credentials());
    annotate_ns(client, ns, ANN_CONFIG, &serde_json::to_string(&redacted)?).await;
    annotate_ns(client, ns, ANN_BACKUP, &serde_json::to_string(&def.backup)?).await;
    crate::runtime::wake("outputs");
    Ok(())
}

pub(crate) async fn read_definition(client: &Client, ns: &str) -> anyhow::Result<AppDefinition> {
    match read_definition_from_cluster(client, ns).await {
        Ok(def) => {
            take_definition_in(ns, &def);
            Ok(def)
        }
        Err(e) => match stored_definition(ns) {
            Some(def) => {
                tracing::warn!(
                    "{ns}: its saved settings could not be read from the cluster ({e}) — using the \
                     copy replicated to this machine"
                );
                Ok(def)
            }
            None => Err(e),
        },
    }
}

fn stored_definition(ns: &str) -> Option<AppDefinition> {
    crate::store::locked()
        .app_definition(ns)
        .unwrap_or_else(|e| {
            tracing::error!("{ns}: the replicated copy of its settings is unreadable ({e})");
            None
        })
}

fn take_definition_in(ns: &str, def: &AppDefinition) {
    let mut store = crate::store::locked();
    match store.import_app_definition(ns, def) {
        Ok(true) => {
            if let Err(e) = store.persist(&crate::store::default_path()) {
                tracing::warn!("{ns}: the desired-state store could not be saved ({e})");
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("{ns}: could not be taken into the desired-state store ({e})"),
    }
}

fn record_definition(ns: &str, def: &AppDefinition) {
    let mut store = crate::store::locked();
    if let Err(e) = store.set_app_definition(ns, def) {
        tracing::warn!(
            "{ns}: its settings were saved to the cluster but not recorded in the desired-state \
             store ({e})"
        );
        return;
    }
    if let Err(e) = store.persist(&crate::store::default_path()) {
        tracing::warn!("{ns}: the desired-state store could not be saved ({e})");
    }
}

async fn read_definition_from_cluster(client: &Client, ns: &str) -> anyhow::Result<AppDefinition> {
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
    catalog_dir: &std::path::Path,
) -> AppDefinition {
    let app = app_schema(catalog_dir, &def.app_id);
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
        service_name: String::new(),
        config,
        volumes: Vec::new(),
        resources: ResourceSpec::default(),
        backup: BackupPolicy::default(),
    }
}

pub(crate) async fn collect_runtime(
    client: &kube::Client,
    ns: &str,
) -> (Vec<VolumeSpec>, ResourceSpec) {
    let volumes = crate::routers::backup_common::user_pvcs(client)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.namespace == ns)
        .map(|p| VolumeSpec {
            name: p.name,
            capacity: p.capacity,
        })
        .collect();

    let resources = match workloads(client, ns).await {
        Ok(items) => requested_resources(&items),
        Err(e) => {
            tracing::warn!("{ns}: could not read what this app runs: {e:#}");
            ResourceSpec::default()
        }
    };
    (volumes, resources)
}

async fn workloads(client: &kube::Client, ns: &str) -> anyhow::Result<Vec<Value>> {
    let params = kube::api::ListParams::default();
    let mut items = Vec::new();
    for kind in ["Deployment", "StatefulSet", "DaemonSet"] {
        items.extend(crate::k8s::list(client, "apps/v1", kind, Some(ns), &params).await?);
    }
    Ok(items)
}

fn requested_resources(items: &[Value]) -> ResourceSpec {
    let mut resources = ResourceSpec::default();
    {
        for item in items {
            let replicas = item["spec"]["replicas"].as_u64().unwrap_or(1);
            resources.replicas += replicas;
            for c in item["spec"]["template"]["spec"]["containers"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let Some(req) = c["resources"]["requests"].as_object() else {
                    continue;
                };
                if let Some(cpu) = req.get("cpu").and_then(|v| v.as_str()) {
                    resources.cpu_millicores += parse_cpu_millicores(cpu) * replicas;
                }
                if let Some(mem) = req.get("memory").and_then(|v| v.as_str()) {
                    resources.memory_bytes += parse_memory_bytes(mem) * replicas;
                }
                for (k, v) in req {
                    if k.ends_with("/gpu") {
                        if let Some(n) = v.as_str().and_then(|s| s.parse::<u64>().ok()) {
                            resources.gpu += n * replicas;
                        }
                    }
                }
            }
        }
    }
    resources
}

pub(crate) fn parse_cpu_millicores(s: &str) -> u64 {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.trim().parse::<f64>().unwrap_or(0.0).round() as u64;
    }
    if let Some(n) = s.strip_suffix('n') {
        return (n.trim().parse::<f64>().unwrap_or(0.0) / 1_000_000.0).round() as u64;
    }
    (s.parse::<f64>().unwrap_or(0.0) * 1000.0).round() as u64
}

pub(crate) fn parse_memory_bytes(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("Ki") {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("Gi") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("Ti") {
        (n, 1024 * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1_000_000)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1_000_000_000)
    } else if let Some(n) = s.strip_suffix('T') {
        (n, 1_000_000_000_000)
    } else {
        (s, 1)
    };
    num.trim().parse::<f64>().unwrap_or(0.0) as u64 * mult
}

fn tunnel_config(cfg: &Config) -> anyhow::Result<toml::Table> {
    cfg.tunnel_table()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))
}

const ANN_DISPLAY_NAME: &str = "yolab.io/display-name";
const ANN_ICON: &str = "yolab.io/icon";
const ANN_CATEGORY: &str = "yolab.io/category";

#[derive(Deserialize, Default)]
struct ChartYaml {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    home: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "type")]
    type_: String,
    #[serde(default)]
    annotations: std::collections::HashMap<String, String>,
}

struct ChartMeta {
    chart: ChartYaml,
    app: crate::appschema::AppSchema,
}

impl ChartMeta {
    fn ann(&self, key: &str) -> &str {
        self.chart
            .annotations
            .get(key)
            .map(String::as_str)
            .unwrap_or("")
    }
    fn display_name(&self) -> String {
        let n = self.ann(ANN_DISPLAY_NAME);
        if n.is_empty() {
            self.chart.name.clone()
        } else {
            n.to_string()
        }
    }
}

fn read_chart(dir: &std::path::Path) -> Option<ChartMeta> {
    let chart: ChartYaml =
        serde_norway::from_str(&std::fs::read_to_string(dir.join("Chart.yaml")).ok()?).ok()?;
    if chart.type_ == "library" {
        return None;
    }
    let schema = std::fs::read_to_string(dir.join("values.schema.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    let app = crate::appschema::AppSchema::from_parts(schema, &chart.annotations);
    Some(ChartMeta { chart, app })
}

pub(crate) fn app_schema(catalog_dir: &std::path::Path, id: &str) -> crate::appschema::AppSchema {
    let found = (!id.is_empty())
        .then(|| read_chart(&catalog_dir.join(id)))
        .flatten();
    match found {
        Some(meta) => meta.app,
        None => crate::appschema::AppSchema::from_parts(Value::Null, &Default::default()),
    }
}

fn resolve_service_name(schema: &Value, config: &serde_json::Map<String, Value>) -> String {
    fn tunnel_field(props: Option<&serde_json::Map<String, Value>>) -> Option<(String, Value)> {
        props?.iter().find_map(|(k, v)| {
            (v["format"].as_str() == Some("tunnel")).then(|| (k.clone(), v.clone()))
        })
    }

    let nested = schema["properties"]["config"]["properties"].as_object();
    let top = schema["properties"].as_object();

    let Some((field, spec)) = tunnel_field(nested).or_else(|| tunnel_field(top)) else {
        return String::new();
    };

    config
        .get(&field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| spec["default"].as_str().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_string()
}

fn build_values(
    config: &serde_json::Map<String, Value>,
    tunnel_cfg: &toml::Table,
    service_name: &str,
) -> String {
    serde_json::json!({
        "config": config,
        "yolab": {
            "platformApiUrl": tunnel_cfg.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or(""),
            "serviceName": service_name,
        },
    })
    .to_string()
}

async fn ensure_tunnel_credentials(
    client: &Client,
    ns: &str,
    tunnel_cfg: &toml::Table,
) -> anyhow::Result<()> {
    let token = tunnel_cfg
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    crate::k8s::apply(
        client,
        &crate::k8s::secret_manifest(
            "yolab-tunnel-credentials",
            ns,
            &[("account-token", token)],
            &[("app.kubernetes.io/managed-by", "yolab")],
        ),
    )
    .await
}

async fn ensure_app_namespace(
    client: &Client,
    ns: &str,
    app_id: &str,
    repo: &str,
    chart_version: &str,
) -> anyhow::Result<()> {
    crate::k8s::apply(
        client,
        &serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": ns,
                "labels": { LABEL_MANAGED: "true" },
                "annotations": {
                    ANN_APP_ID: app_id,
                    ANN_CHART_REPO: repo,
                    ANN_CHART_VERSION: chart_version,
                    "volsync.backube/privileged-movers": "true",
                },
            },
        }),
    )
    .await
}

fn validate_config_values(
    config: &serde_json::Map<String, Value>,
) -> std::result::Result<(), String> {
    fn check(v: &Value) -> std::result::Result<(), String> {
        match v {
            Value::String(s) => {
                if s.len() > 8192 {
                    return Err("value exceeds 8192 bytes".into());
                }
                if let Some(c) = s.chars().find(|c| c.is_control() && *c != '\t') {
                    return Err(format!("value contains illegal control character {c:?}"));
                }
                Ok(())
            }
            Value::Array(a) => a.iter().try_for_each(check),
            Value::Object(o) => o.values().try_for_each(check),
            _ => Ok(()),
        }
    }
    for (k, v) in config {
        check(v).map_err(|e| format!("field '{k}': {e}"))?;
    }
    Ok(())
}

fn derive_domain(dns_url: &str) -> String {
    let host = dns_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 1 && !parts[0].chars().all(|c| c.is_ascii_digit()) {
        parts[1..].join(".")
    } else {
        host.to_string()
    }
}

pub async fn tunnel_domain(State(state): State<AppState>) -> Result<Json<DomainResponse>> {
    let tunnel = tunnel_config(&state.config)?;
    let dns_url = tunnel.get("dns_url").and_then(|v| v.as_str()).unwrap_or("");
    Ok(Json(DomainResponse {
        domain: derive_domain(dns_url),
    }))
}

fn catalog_entry_from(repo: String, meta: ChartMeta) -> CatalogApp {
    CatalogApp {
        id: meta.chart.name.clone(),
        repo,
        name: meta.display_name(),
        description: meta.chart.description.clone(),
        home: meta.chart.home.clone(),
        icon: meta.ann(ANN_ICON).to_string(),
        category: meta.ann(ANN_CATEGORY).to_string(),
        chart_version: meta.chart.version.clone(),
        schema: meta.app.config(),
    }
}

pub async fn refresh_catalog_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let mut refreshed = false;
    let mut note = String::new();

    for repo in crate::charts::list_repos(&b.kube).await {
        match crate::charts::sync_chart(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repo,
            &id,
        )
        .await
        {
            Ok(()) => {
                refreshed = true;
                break;
            }
            Err(e) => note = e.to_string(),
        }
    }

    let entry = crate::charts::chart_sources(&b.kube)
        .await
        .into_iter()
        .find_map(|(repo, dir)| {
            let m = read_chart(&dir.join(&id))?;
            Some(catalog_entry_from(repo, m))
        });

    Ok(Json(serde_json::json!({
        "refreshed": refreshed,
        "note": note,
        "app": entry,
    })))
}

pub async fn catalog(State(state): State<AppState>) -> Result<Json<Vec<CatalogApp>>> {
    let mut apps: Vec<CatalogApp> = vec![];
    let mut seen: std::collections::HashSet<String> = Default::default();

    for (repo, dir) in crate::charts::chart_sources(&state.kube.client().await?).await {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Some(meta) = read_chart(&entry.path()) else {
                continue;
            };
            if !seen.insert(meta.chart.name.clone()) {
                continue;
            }
            apps.push(catalog_entry_from(repo.clone(), meta));
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(Json(apps))
}

#[derive(Deserialize)]
pub struct AddRepoBody {
    pub name: String,
    pub url: String,
}

pub async fn list_repos(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::charts::ChartRepo>>> {
    Ok(Json(
        crate::charts::list_repos(&state.kube.client().await?).await,
    ))
}

pub async fn add_repo(
    State(state): State<AppState>,
    Json(body): Json<AddRepoBody>,
) -> impl IntoResponse {
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    if let Err(e) = crate::charts::add_repo(&b.kube, &body.name, &body.url).await {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let repos = crate::charts::list_repos(&b.kube).await;
    if let Some(r) = repos.iter().find(|r| r.name == body.name) {
        if let Err(e) =
            crate::charts::sync_repo(&b.host, std::path::Path::new(crate::charts::CACHE_DIR), r)
                .await
        {
            return (
                StatusCode::BAD_GATEWAY,
                format!("added, but sync failed: {e}"),
            )
                .into_response();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

pub async fn remove_repo(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    match crate::charts::remove_repo(&client, &name).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn sync_repos(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let mut results = serde_json::Map::new();
    for repo in crate::charts::list_repos(&b.kube).await {
        let entry = match crate::charts::sync_repo(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repo,
        )
        .await
        {
            Ok(n) => serde_json::json!({ "ok": true, "charts": n }),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        };
        results.insert(repo.name.clone(), entry);
    }
    Ok(Json(Value::Object(results)))
}

pub(crate) fn is_backup_mover_pod(pod: &Value) -> bool {
    pod["metadata"]["name"]
        .as_str()
        .is_some_and(|n| n.starts_with("volsync-"))
        || pod["metadata"]["labels"]["app.kubernetes.io/created-by"].as_str() == Some("volsync")
}

pub(crate) fn is_terminating_pod(pod: &Value) -> bool {
    !pod["metadata"]["deletionTimestamp"].is_null()
}

pub(crate) fn explain_app_state(pods: &[&Value]) -> String {
    if pods.is_empty() {
        return "Waiting to be given a machine to run on".into();
    }

    let mut restarts: i64 = 0;
    let mut waiting: Vec<(String, bool)> = Vec::new();
    let mut unschedulable = false;
    let mut storage_pending = false;
    let mut running_not_ready = false;

    for pod in pods {
        if pod["status"]["phase"].as_str() == Some("Pending") {
            let conditions = pod["status"]["conditions"].as_array();
            unschedulable |= conditions
                .map(|cs| {
                    cs.iter()
                        .any(|c| c["type"] == "PodScheduled" && c["reason"] == "Unschedulable")
                })
                .unwrap_or(false);
            storage_pending |= conditions
                .map(|cs| {
                    cs.iter().any(|c| {
                        c["message"]
                            .as_str()
                            .map(|m| m.contains("PersistentVolumeClaim"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
        }

        for (key, is_init) in [
            ("initContainerStatuses", true),
            ("containerStatuses", false),
        ] {
            for cs in pod["status"][key].as_array().into_iter().flatten() {
                restarts += cs["restartCount"].as_i64().unwrap_or(0);
                if let Some(reason) = cs["state"]["waiting"]["reason"].as_str() {
                    waiting.push((reason.to_string(), is_init));
                }
                if cs["state"]["running"].is_object() && cs["ready"] == false {
                    running_not_ready = true;
                }
            }
        }
    }

    let has = |r: &str| waiting.iter().any(|(reason, _)| reason == r);

    if has("CrashLoopBackOff") {
        return if restarts > 1 {
            format!("Keeps stopping unexpectedly — restarted {restarts} times. Check the logs.")
        } else {
            "Keeps stopping unexpectedly. Check the logs.".into()
        };
    }
    if has("ImagePullBackOff") || has("ErrImagePull") {
        return "Could not download this app. Check that the machine is online.".into();
    }
    if has("CreateContainerConfigError") || has("CreateContainerError") {
        return "A setting is missing or wrong, so it cannot start.".into();
    }
    if has("InvalidImageName") {
        return "This app's image name is not valid, so it cannot be downloaded.".into();
    }
    if unschedulable {
        return if storage_pending {
            "Getting this app's storage ready — copying its files can take a while.".into()
        } else {
            "No machine has room for this app right now.".into()
        };
    }

    if has("ContainerCreating") {
        return "Getting ready — downloading files and connecting storage.".into();
    }
    if has("PodInitializing") {
        return "Running first-time setup.".into();
    }
    if running_not_ready {
        return "Almost ready — waiting for the app to respond.".into();
    }
    if !waiting.is_empty() {
        let (reason, _) = &waiting[0];
        return format!("Waiting: {reason}");
    }

    String::new()
}

fn is_cloning(pvc: &Value) -> bool {
    pvc["status"]["phase"].as_str() == Some("Pending")
        && ["dataSource", "dataSourceRef"]
            .iter()
            .any(|key| pvc["spec"][key]["kind"].as_str() == Some("VolumeSnapshot"))
}

fn clone_percent(events: &[Value], namespace: &str, pvc: &str) -> Option<u32> {
    let mut latest: Option<(&str, &str)> = None;
    for event in events {
        if event["involvedObject"]["kind"].as_str() != Some("PersistentVolumeClaim")
            || event["involvedObject"]["namespace"].as_str() != Some(namespace)
            || event["involvedObject"]["name"].as_str() != Some(pvc)
        {
            continue;
        }
        let Some(message) = event["message"].as_str() else {
            continue;
        };
        if !message.contains("percentage cloned=") {
            continue;
        }
        let at = event["lastTimestamp"]
            .as_str()
            .or_else(|| event["eventTime"].as_str())
            .unwrap_or("");
        if latest.map(|(seen, _)| at >= seen).unwrap_or(true) {
            latest = Some((at, message));
        }
    }
    let (_, message) = latest?;
    let rest = message.split("percentage cloned=").nth(1)?;
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number
        .parse::<f64>()
        .ok()
        .map(|percent| percent.round() as u32)
}

fn copying_data(
    namespace: &str,
    pvcs_by_ns: &std::collections::HashMap<&str, Vec<&Value>>,
    events: &[Value],
) -> Option<String> {
    let pvc = pvcs_by_ns
        .get(namespace)?
        .iter()
        .find(|pvc| is_cloning(pvc))?;
    let name = pvc["metadata"]["name"].as_str().unwrap_or("");
    Some(match clone_percent(events, namespace, name) {
        Some(percent) => format!("Copying this app's files… {percent}%"),
        None => "Copying this app's files…".to_string(),
    })
}

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let client = &state.kube.client().await?;
    let catalog_dir = state.config.catalog_dir();
    let backup_status = crate::routers::backup::app_backup_status(client).await;
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let everything = kube::api::ListParams::default();
    let (ns_out, pods_out, pvcs_out, events_out, mut remembered) = tokio::join!(
        crate::k8s::list(client, "v1", "Namespace", None, &managed),
        crate::k8s::list(client, "v1", "Pod", None, &everything),
        crate::k8s::list(client, "v1", "PersistentVolumeClaim", None, &everything),
        crate::k8s::list(client, "v1", "Event", None, &everything),
        crate::outputs::remembered_everywhere(client),
    );
    let namespaces = ns_out?;

    let all_pod_items = pods_out.unwrap_or_default();
    let mut pods_by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
    for pod in &all_pod_items {
        if let Some(ns) = pod["metadata"]["namespace"].as_str() {
            pods_by_ns.entry(ns).or_default().push(pod);
        }
    }
    let all_pvc_items = pvcs_out.unwrap_or_default();
    let mut pvcs_by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
    for pvc in &all_pvc_items {
        if let Some(ns) = pvc["metadata"]["namespace"].as_str() {
            pvcs_by_ns.entry(ns).or_default().push(pvc);
        }
    }
    let all_event_items = events_out.unwrap_or_default();

    let mut apps = vec![];
    for ns in &namespaces {
        let ann = ns["metadata"]["annotations"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let name = ns["metadata"]["name"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("yolab-")
            .to_string();
        let phase = ns["status"]["phase"].as_str().unwrap_or("Active");
        let ns_full = format!("yolab-{name}");
        let mut detail = String::new();
        let status = if phase == "Terminating" || uninstall_lock_is_fresh(&ann) {
            "uninstalling".to_string()
        } else if let Some(copying) = copying_data(&ns_full, &pvcs_by_ns, &all_event_items) {
            detail = copying;
            "copying".to_string()
        } else {
            let items: Vec<&Value> = pods_by_ns
                .get(ns_full.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|p| !is_backup_mover_pod(p) && !is_terminating_pod(p))
                .copied()
                .collect();
            let all_ready = !items.is_empty()
                && items.iter().all(|p| {
                    p["status"]["conditions"]
                        .as_array()
                        .map(|cs| {
                            cs.iter()
                                .any(|c| c["type"] == "Ready" && c["status"] == "True")
                        })
                        .unwrap_or(false)
                });
            if !all_ready {
                detail = explain_app_state(&items);
            }
            if all_ready { "running" } else { "starting" }.to_string()
        };

        let id = ann
            .get(ANN_APP_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let config = saved_settings(&ann);
        let outputs = listed_outputs(
            &app_schema(&catalog_dir, &id),
            remembered
                .remove(&format!("yolab-{name}"))
                .unwrap_or_default(),
            &ann,
            &config,
        );

        let policy: BackupPolicy = ann
            .get(ANN_BACKUP)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let (last_ok_at, running) = backup_status
            .get(&format!("yolab-{name}"))
            .cloned()
            .unwrap_or((None, false));

        apps.push(AppInfo {
            app_id: id,
            instance_id: split_instance_name(&name).1.map(str::to_string),
            instance_name: name,
            status,
            detail,
            outputs,
            config,
            backup: AppBackupStatus {
                enabled: policy.enabled,
                schedule: policy.schedule,
                last_ok_at,
                running,
            },
        });
    }
    Ok(Json(apps))
}

const MAX_NS_LEN: usize = 63;
const NS_OVERHEAD: usize = "yolab-".len() + 1 + INSTANCE_SUFFIX_LEN;
const INSTANCE_SUFFIX_LEN: usize = 4;

fn instance_suffix() -> String {
    (0..INSTANCE_SUFFIX_LEN)
        .map(|_| SUFFIX_ALPHABET[rand::random::<usize>() % SUFFIX_ALPHABET.len()] as char)
        .collect()
}

const SUFFIX_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

fn split_instance_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('-') {
        Some((stem, id))
            if !stem.is_empty()
                && id.len() == INSTANCE_SUFFIX_LEN
                && id.bytes().all(|b| SUFFIX_ALPHABET.contains(&b)) =>
        {
            (stem, Some(id))
        }
        _ => (name, None),
    }
}

fn instance_stem(requested: &str) -> Option<String> {
    let stem: String = requested
        .chars()
        .take(MAX_NS_LEN.saturating_sub(NS_OVERHEAD))
        .collect();
    let stem = stem.trim_end_matches('-');
    (!stem.is_empty()).then(|| stem.to_string())
}

async fn unique_instance_name(client: &Client, requested: &str) -> Option<String> {
    let stem = instance_stem(requested)?;
    for _ in 0..8 {
        let candidate = format!("{stem}-{}", instance_suffix());
        match crate::k8s::exists(client, &namespace_ref(&format!("yolab-{candidate}"))).await {
            Ok(false) => return Some(candidate),
            Ok(true) => {}
            Err(e) => {
                tracing::warn!("cannot tell whether yolab-{candidate} is taken: {e:#}");
                return None;
            }
        }
    }
    None
}

pub async fn install_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InstallRequest>,
) -> impl IntoResponse {
    let refuse = |why: String| (StatusCode::BAD_REQUEST, why).into_response();

    if !install::is_instance_name(&body.instance_name) {
        return refuse("the name may only use lowercase letters, numbers and hyphens".into());
    }
    if let Err(e) = validate_config_values(&body.config) {
        return refuse(format!("invalid config: {e}"));
    }
    if !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::NOT_FOUND, format!("App '{id}' not found")).into_response();
    }

    let sources = match install::resolve_sources(body.source.as_ref()) {
        Ok(s) => s,
        Err(e) => return refuse(e),
    };
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let source_definition = match install::source_definition(&b, &sources.config).await {
        Ok(d) => d,
        Err(e) => return refuse(format!("{e}")),
    };
    let Some(instance_name) = unique_instance_name(&b.kube, &body.instance_name).await else {
        return refuse("could not derive a unique name for this app".into());
    };
    let plan = match install::plan(
        &id,
        &instance_name,
        body.config,
        source_definition.as_ref(),
        sources.data,
        &app_schema(&state.config.catalog_dir(), &id),
    ) {
        Ok(p) => p,
        Err(e) => return refuse(e),
    };

    Sse::new(install::install_stream(b, state.config.clone(), plan)).into_response()
}

pub(crate) async fn rollback_failed_install<H: crate::host::Host>(
    b: &Backend<H>,
    ns: &str,
    instance_name: &str,
) {
    match b
        .host
        .run_cmd_bounded(
            "helm",
            &["uninstall", instance_name, "-n", ns],
            std::time::Duration::from_secs(120),
        )
        .await
    {
        Ok(o) if !o.success => {
            tracing::debug!("rollback {ns}: helm uninstall: {}", o.stderr.trim())
        }
        Err(e) => tracing::debug!("rollback {ns}: helm uninstall: {e}"),
        Ok(_) => {}
    }
    crate::k8s::delete_if_present(&b.kube, &namespace_ref(ns))
        .await
        .debug_on_err(format!("rollback {ns}: delete namespace"));
}

pub(crate) struct StagedInstall {
    pub(crate) ns: String,
    pub(crate) chart_repo: String,
    pub(crate) chart_version: String,
    pub(crate) service_name: String,
    pub(crate) chart_dir: std::path::PathBuf,
    pub(crate) values: tempfile::NamedTempFile,
}

pub(crate) async fn stage_install(
    client: &Client,
    cfg: &Config,
    id: &str,
    instance_name: &str,
    config: &serde_json::Map<String, Value>,
    prefer_repo: Option<&str>,
) -> anyhow::Result<StagedInstall> {
    let tunnel_cfg =
        tunnel_config(cfg).map_err(|_| anyhow::anyhow!("could not read tunnel config"))?;
    let Some((repo, chart_dir)) = crate::charts::resolve_chart(client, id, prefer_repo).await
    else {
        anyhow::bail!("no chart named {id} in any configured repository");
    };
    let Some(meta) = read_chart(&chart_dir) else {
        anyhow::bail!("{id} is not a valid chart");
    };
    let ns = format!("yolab-{instance_name}");
    ensure_app_namespace(client, &ns, id, &repo, &meta.chart.version)
        .await
        .map_err(|e| anyhow::anyhow!("create namespace: {e}"))?;
    ensure_tunnel_credentials(client, &ns, &tunnel_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("stage tunnel credentials: {e}"))?;
    let service_name = resolve_service_name(&meta.app.config(), config);
    let values = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .map_err(|e| anyhow::anyhow!("staging values: {e}"))?;
    std::fs::write(
        values.path(),
        build_values(config, &tunnel_cfg, &service_name),
    )
    .map_err(|e| anyhow::anyhow!("write values: {e}"))?;
    Ok(StagedInstall {
        ns,
        chart_repo: repo,
        chart_version: meta.chart.version.clone(),
        service_name,
        chart_dir,
        values,
    })
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    pub config: Option<serde_json::Map<String, Value>>,
}

pub async fn update_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
    body: Option<Json<UpdateRequest>>,
) -> impl IntoResponse {
    let ns = format!("yolab-{instance_name}");
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let ns_v = match namespace(&client, &ns).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, "Instance not found").into_response(),
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("cannot read the app right now: {e}"),
            )
                .into_response()
        }
    };
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let annotation = |key: &str| {
        ann.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let id = annotation(ANN_APP_ID).unwrap_or_default();
    let app = app_schema(&state.config.catalog_dir(), &id);
    let stored_config = match read_config(&client, &ns).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("cannot read this app's saved settings right now: {e}"),
            )
                .into_response()
        }
    };

    let config = match body.and_then(|b| b.0.config) {
        Some(incoming) => merge_credentials(incoming, &stored_config, &app),
        None => stored_config,
    };

    if let Err(e) = validate_config_values(&config) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response();
    }
    if id.is_empty() || !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::BAD_REQUEST, "App not found in catalog").into_response();
    }

    let plan = install::UpgradePlan {
        app_id: id,
        instance_name,
        config,
        chart_repo: annotation(ANN_CHART_REPO),
        backup: read_definition_opt(&client, &ns)
            .await
            .map(|d| d.backup)
            .unwrap_or_default(),
    };
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    Sse::new(install::upgrade_stream(b, state.config.clone(), plan)).into_response()
}

#[derive(Deserialize)]
pub struct BackupPolicyRequest {
    pub enabled: bool,
    pub schedule: String,
}

pub async fn set_backup_policy(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
    Json(body): Json<BackupPolicyRequest>,
) -> Result<Json<serde_json::Value>> {
    crate::cron::Cron::parse(&body.schedule)
        .map_err(|e| anyhow::anyhow!("that schedule is not valid: {e}"))?;
    let ns = format!("yolab-{instance_name}");
    let client = state.kube.client().await?;
    let mut def = read_definition(&client, &ns).await?;
    def.backup = BackupPolicy {
        enabled: body.enabled,
        schedule: body.schedule,
    };
    let app = app_schema(&state.config.catalog_dir(), &def.app_id);
    write_definition(&client, &ns, &def, &app).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn app_definition(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<AppDefinition>> {
    let ns = format!("yolab-{instance_name}");
    let def = read_definition(&state.kube.client().await?, &ns).await?;
    Ok(Json(redact_definition(&def, &state.config.catalog_dir())))
}

fn saved_settings(ann: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    ann.get(ANN_CONFIG)
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

fn without_redacted(config: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    config
        .iter()
        .filter(|(_, v)| v.as_str() != Some(REDACTED))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn listed_outputs(
    app: &crate::appschema::AppSchema,
    mut remembered: crate::outputs::Remembered,
    ann: &serde_json::Map<String, Value>,
    settings: &serde_json::Map<String, Value>,
) -> Vec<crate::outputs::ShownOutput> {
    for (key, found) in crate::outputs::from_legacy_annotation(ann, chrono::Utc::now()) {
        remembered.entry(key).or_insert(found);
    }
    crate::outputs::shown(
        &app.applicable_outputs(settings),
        &remembered,
        &without_redacted(settings),
    )
}

pub(crate) async fn installed_apps(client: &Client) -> anyhow::Result<Vec<InstalledApp>> {
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let listed = crate::k8s::list(client, "v1", "Namespace", None, &managed).await?;
    Ok(listed
        .iter()
        .filter(|ns| ns["status"]["phase"].as_str() != Some("Terminating"))
        .filter_map(|ns| {
            let annotations = ns["metadata"]["annotations"].as_object().cloned()?;
            if uninstall_lock_is_fresh(&annotations) {
                return None;
            }
            Some(InstalledApp {
                namespace: ns["metadata"]["name"].as_str()?.to_string(),
                app_id: annotations.get(ANN_APP_ID)?.as_str()?.to_string(),
                settings: saved_settings(&annotations),
                annotations,
            })
        })
        .collect())
}

pub(crate) struct KnownOutputs {
    pub specs: Vec<crate::appschema::OutputSpec>,
    pub remembered: crate::outputs::Remembered,
    pub settings: serde_json::Map<String, Value>,
}

pub(crate) async fn known_outputs(
    client: &Client,
    catalog_dir: &std::path::Path,
    ns: &str,
    rescan_first: bool,
) -> anyhow::Result<KnownOutputs> {
    let ns_v = namespace(client, ns)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} does not exist"))?;
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("");
    let app = app_schema(catalog_dir, id);
    let settings = saved_settings(&ann);

    let remembered = if rescan_first {
        crate::outputs::rescan(client, ns, &app, &settings, &ann).await?
    } else {
        let mut stored = crate::outputs::read_remembered(client, ns)
            .await?
            .unwrap_or_default();
        for (key, found) in crate::outputs::from_legacy_annotation(&ann, chrono::Utc::now()) {
            stored.entry(key).or_insert(found);
        }
        stored
    };

    Ok(KnownOutputs {
        specs: app.applicable_outputs(&settings),
        remembered,
        settings,
    })
}

async fn outputs_of(
    state: &AppState,
    instance_name: &str,
    rescan_first: bool,
) -> Result<Json<OutputsResponse>> {
    let ns = format!("yolab-{instance_name}");
    let client = state.kube.client().await?;
    let known = known_outputs(&client, &state.config.catalog_dir(), &ns, rescan_first).await?;
    let full_config = match read_definition(&client, &ns).await {
        Ok(def) => def.config,
        Err(e) => {
            tracing::warn!(
                "{ns}: settings unreadable, outputs taken from settings are hidden ({e})"
            );
            without_redacted(&known.settings)
        }
    };
    Ok(Json(OutputsResponse {
        outputs: crate::outputs::shown(&known.specs, &known.remembered, &full_config),
    }))
}

pub async fn app_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<OutputsResponse>> {
    outputs_of(&state, &instance_name, false).await
}

pub async fn scan_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<OutputsResponse>> {
    outputs_of(&state, &instance_name, true).await
}

fn uninstall_lock_is_fresh(ann: &serde_json::Map<String, Value>) -> bool {
    ann.get(ANN_UNINSTALLING)
        .and_then(|v| v.as_str())
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| {
            chrono::Utc::now().signed_duration_since(t).num_seconds()
                <= UNINSTALL_LOCK_TTL.as_secs() as i64
        })
        .unwrap_or(false)
}

async fn claim_uninstall_lock(client: &Client, ns: &str) -> anyhow::Result<bool> {
    let Some(existing) = namespace(client, ns).await? else {
        return Ok(true);
    };
    let ann = existing["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if uninstall_lock_is_fresh(&ann) {
        return Ok(false);
    }
    if ann.contains_key(ANN_UNINSTALLING) {
        tracing::warn!("uninstall {ns}: reclaiming a stale uninstall lock");
    }
    let mut claim = namespace_ref(ns);
    claim["metadata"]["resourceVersion"] = existing["metadata"]["resourceVersion"].clone();
    claim["metadata"]["annotations"] =
        serde_json::json!({ ANN_UNINSTALLING: chrono::Utc::now().to_rfc3339() });
    match crate::k8s::merge_patch(client, &claim).await {
        Ok(()) => Ok(true),
        Err(e) if crate::k8s::refused_with(&e, 409) => Ok(false),
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(true),
        Err(e) => Err(e),
    }
}

const NAMESPACE_DELETE_ATTEMPTS: u32 = 4;
const NAMESPACE_DELETE_FIRST_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

async fn delete_namespace_with_retry(client: &Client, ns: &str) {
    let mut delay = NAMESPACE_DELETE_FIRST_RETRY;
    for attempt in 1..=NAMESPACE_DELETE_ATTEMPTS {
        match crate::k8s::delete_if_present(client, &namespace_ref(ns)).await {
            Ok(()) => return,
            Err(e) if attempt == NAMESPACE_DELETE_ATTEMPTS => {
                tracing::warn!(
                    "uninstall {ns}: delete namespace failed after {NAMESPACE_DELETE_ATTEMPTS} \
                     attempts, leaving it for a future retry: {e}"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "uninstall {ns}: delete namespace attempt {attempt} failed, retrying: {e}"
                );
                tokio::time::sleep(delay).await;
                delay *= 3;
            }
        }
    }
}

pub async fn uninstall_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let ns = format!("yolab-{instance_name}");
    let b = state.backend().await?;

    if !claim_uninstall_lock(&b.kube, &ns).await? {
        return Err(anyhow::anyhow!("uninstall for {instance_name} is already in progress").into());
    }

    let instance_owned = instance_name.clone();
    let task = tokio::spawn(async move {
        run_teardown(&b, &instance_owned, &ns).await;
    });
    if let Err(e) = task.await {
        tracing::error!("uninstall {instance_name}: teardown task failed: {e}");
        return Err(anyhow::anyhow!("the uninstall did not finish: {e}").into());
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn namespace_is_terminating(client: &Client, ns: &str) -> bool {
    namespace(client, ns)
        .await
        .ok()
        .flatten()
        .is_some_and(|v| v["status"]["phase"] == "Terminating")
}

const HELM_UNINSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

async fn run_teardown<H: crate::host::Host>(b: &Backend<H>, instance_name: &str, ns: &str) {
    if namespace_is_terminating(&b.kube, ns).await {
        tracing::info!(
            "uninstall {instance_name}: namespace is already terminating — waiting for it \
             to finish rather than re-running helm"
        );
        delete_namespace_with_retry(&b.kube, ns).await;
        return;
    }

    let out = b
        .host
        .run_cmd_bounded(
            "helm",
            &[
                "uninstall",
                instance_name,
                "-n",
                ns,
                "--ignore-not-found",
                "--wait",
            ],
            HELM_UNINSTALL_TIMEOUT,
        )
        .await;
    match out {
        Ok(o) if !o.success => tracing::warn!(
            "uninstall {instance_name}: helm uninstall failed: {}",
            o.stderr.trim()
        ),
        Err(e) => tracing::warn!(
            "uninstall {instance_name}: helm uninstall did not finish ({e}) — deleting the namespace anyway"
        ),
        Ok(_) => {}
    }

    delete_namespace_with_retry(&b.kube, ns).await;
}

fn abandoned_in(namespaces: &[Value]) -> Vec<(String, String)> {
    namespaces
        .iter()
        .filter_map(|ns| {
            let name = ns["metadata"]["name"].as_str()?;
            let ann = ns["metadata"]["annotations"].as_object()?;
            ann.get(ANN_UNINSTALLING)?;
            if uninstall_lock_is_fresh(ann) {
                return None;
            }
            let instance = name.strip_prefix("yolab-")?.to_string();
            Some((name.to_string(), instance))
        })
        .collect()
}

async fn abandoned_uninstalls(client: &Client) -> anyhow::Result<Vec<(String, String)>> {
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let namespaces = crate::k8s::list(client, "v1", "Namespace", None, &managed).await?;
    Ok(abandoned_in(&namespaces))
}

pub struct UninstallWatchdogController;

impl crate::runtime::Controller for UninstallWatchdogController {
    fn name(&self) -> &'static str {
        "uninstall-watchdog"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    fn not_before_uptime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(90)
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        finish_abandoned_uninstalls(&Backend::real().await?).await
    }
}

async fn finish_abandoned_uninstalls<H: crate::host::Host>(
    b: &Backend<H>,
) -> anyhow::Result<crate::runtime::Tick> {
    let abandoned = abandoned_uninstalls(&b.kube).await?;
    if abandoned.is_empty() {
        return Ok(crate::runtime::Tick::Idle("no abandoned uninstalls".into()));
    }
    for (ns, instance) in abandoned {
        tracing::warn!(
            "uninstall {instance}: claim is stale and nothing is driving it — \
             finishing the teardown"
        );
        run_teardown(b, &instance, &ns).await;
    }
    Ok(crate::runtime::Tick::Done)
}

pub async fn list_pods(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<Vec<PodInfo>>> {
    let pods = crate::k8s::list(
        &state.kube.client().await?,
        "v1",
        "Pod",
        Some(&format!("yolab-{instance_name}")),
        &Default::default(),
    )
    .await?;
    Ok(Json(
        pods.iter()
            .filter(|p| !is_backup_mover_pod(p))
            .map(|p| PodInfo {
                name: p["metadata"]["name"].as_str().unwrap_or("").to_string(),
                phase: p["status"]["phase"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_string(),
                ready: p["status"]["conditions"]
                    .as_array()
                    .map(|cs| {
                        cs.iter()
                            .any(|c| c["type"] == "Ready" && c["status"] == "True")
                    })
                    .unwrap_or(false),
            })
            .collect(),
    ))
}

pub async fn pod_logs(
    State(state): State<AppState>,
    Path((instance_name, pod_name)): Path<(String, String)>,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>> {
    let ns = format!("yolab-{instance_name}");
    let kube = state.kube.clone();
    let stream = async_stream::stream! {
        use futures::StreamExt as _;
        let followed = match kube.client().await {
            Ok(client) => follow_pod_logs(&client, &ns, &pod_name).await,
            Err(e) => Err(e),
        };
        let mut lines = match followed {
            Ok(lines) => lines,
            Err(e) => {
                yield Ok(Event::default().data(format!("[yolab] could not read the logs: {e:#}")));
                return;
            }
        };
        while let Some(line) = lines.next().await {
            yield Ok(Event::default().data(line));
        }
    };
    Sse::new(stream)
}

async fn follow_pod_logs(
    client: &Client,
    ns: &str,
    pod: &str,
) -> anyhow::Result<futures::stream::BoxStream<'static, String>> {
    use futures::{AsyncBufReadExt as _, StreamExt as _};
    use k8s_openapi::api::core::v1::Pod;
    let pods = kube::Api::<Pod>::namespaced(client.clone(), ns);
    let spec = pods.get(pod).await?.spec.unwrap_or_default();
    let containers = spec
        .init_containers
        .unwrap_or_default()
        .into_iter()
        .chain(spec.containers)
        .map(|c| c.name);
    let mut streams = Vec::new();
    for container in containers {
        let prefix = format!("[pod/{pod}/{container}]");
        let params = kube::api::LogParams {
            container: Some(container),
            follow: true,
            tail_lines: Some(i64::from(LOGS_FOLLOW_TAIL)),
            ..Default::default()
        };
        match pods.log_stream(pod, &params).await {
            Ok(reader) => streams.push(
                reader
                    .lines()
                    .filter_map(move |line| {
                        let prefixed = line.ok().map(|l| format!("{prefix} {l}"));
                        async move { prefixed }
                    })
                    .boxed(),
            ),
            Err(e) => {
                let said = format!("[yolab] {prefix} {e}");
                streams.push(futures::stream::once(async move { said }).boxed());
            }
        }
    }
    Ok(futures::stream::select_all(streams).boxed())
}

#[cfg(test)]
mod tests {

    fn pod(name: &str) -> Value {
        json!({"metadata": {"name": name}})
    }

    #[test]
    fn a_backup_mover_is_not_one_of_the_apps_pods() {
        assert!(is_backup_mover_pod(&pod(
            "volsync-src-volsync-filebrowser-data-4k46c"
        )));
        assert!(is_backup_mover_pod(&pod(
            "volsync-dst-volsync-vaultwarden-data-abc12"
        )));
    }

    #[test]
    fn the_volsync_label_is_enough_on_its_own() {
        assert!(is_backup_mover_pod(&json!({
            "metadata": {
                "name": "some-future-mover-name",
                "labels": {"app.kubernetes.io/created-by": "volsync"}
            }
        })));
    }

    #[test]
    fn ordinary_app_pods_are_kept() {
        for name in [
            "filebrowser-7d9c8b6f5-x2k9p",
            "vaultwarden-0",
            "syncthing-abc",
            "my-backup-tool-123",
            "",
        ] {
            assert!(
                !is_backup_mover_pod(&pod(name)),
                "{name} is the app's own pod"
            );
        }
    }

    #[test]
    fn a_pod_without_labels_is_handled() {
        assert!(!is_backup_mover_pod(
            &json!({"metadata": {"name": "app-1"}})
        ));
        assert!(!is_backup_mover_pod(&json!({})));
    }

    #[test]
    fn a_pod_being_deleted_is_terminating() {
        assert!(is_terminating_pod(&json!({
            "metadata": {
                "name": "gateway-85495df94-64kkw",
                "deletionTimestamp": "2026-09-06T00:16:35Z"
            }
        })));
    }

    #[test]
    fn a_live_pod_is_not_terminating() {
        assert!(!is_terminating_pod(&pod("gateway-85495df94-74s4t")));
        assert!(!is_terminating_pod(&json!({})));
        assert!(!is_terminating_pod(&json!({
            "metadata": {"name": "app-1", "deletionTimestamp": null}
        })));
    }

    #[test]
    fn a_wedged_terminating_pod_does_not_hold_the_app_at_starting() {
        let ready = |ready: bool| {
            json!({"status": {"conditions": [
                {"type": "Ready", "status": if ready {"True"} else {"False"}}
            ]}})
        };
        let mut ghost = ready(false);
        ghost["metadata"] =
            json!({"name": "gateway-old", "deletionTimestamp": "2026-09-06T00:16:35Z"});
        let mut live = ready(true);
        live["metadata"] = json!({"name": "gateway-new"});

        let counted: Vec<&Value> = [&ghost, &live]
            .into_iter()
            .filter(|p| !is_backup_mover_pod(p) && !is_terminating_pod(p))
            .collect();

        assert_eq!(counted.len(), 1, "only the replacement is evidence");
        assert!(counted.iter().all(|p| {
            p["status"]["conditions"].as_array().is_some_and(|cs| {
                cs.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            })
        }));
    }

    fn waiting_pod(kind: &str, reason: &str, restarts: i64) -> Value {
        json!({"status": {"phase": "Pending", kind: [
            {"restartCount": restarts, "state": {"waiting": {"reason": reason}}}
        ]}})
    }

    #[test]
    fn a_crash_loop_is_never_described_as_starting() {
        let pod = waiting_pod("containerStatuses", "CrashLoopBackOff", 335);
        let msg = explain_app_state(&[&pod]);
        assert!(
            msg.contains("335"),
            "the restart count is the whole signal: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("logs"),
            "must point somewhere: {msg}"
        );
        assert!(!msg.to_lowercase().contains("getting ready"));
    }

    #[test]
    fn a_single_restart_reads_naturally() {
        let pod = waiting_pod("containerStatuses", "CrashLoopBackOff", 1);
        assert!(!explain_app_state(&[&pod]).contains('1'));
    }

    #[test]
    fn a_failed_download_says_so_and_names_the_likely_cause() {
        for reason in ["ImagePullBackOff", "ErrImagePull"] {
            let pod = waiting_pod("containerStatuses", reason, 0);
            let msg = explain_app_state(&[&pod]).to_lowercase();
            assert!(msg.contains("download"), "{reason}: {msg}");
            assert!(msg.contains("online"), "{reason}: {msg}");
        }
    }

    #[test]
    fn something_broken_outranks_something_merely_slow() {
        let pod = json!({"status": {"phase": "Pending", "containerStatuses": [
            {"restartCount": 0, "state": {"waiting": {"reason": "ContainerCreating"}}},
            {"restartCount": 9, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
        ]}});
        assert!(explain_app_state(&[&pod]).contains("stopping"));
    }

    #[test]
    fn first_time_setup_and_downloading_are_distinguishable() {
        let creating = waiting_pod("containerStatuses", "ContainerCreating", 0);
        assert!(explain_app_state(&[&creating]).contains("downloading"));

        let initing = waiting_pod("containerStatuses", "PodInitializing", 0);
        let msg = explain_app_state(&[&initing]);
        assert!(msg.contains("setup"), "{msg}");
        assert!(
            !msg.contains("downloading"),
            "a different state, a different sentence"
        );
    }

    #[test]
    fn nowhere_to_run_is_reported_as_such() {
        let pod = json!({"status": {"phase": "Pending", "conditions": [
            {"type": "PodScheduled", "status": "False", "reason": "Unschedulable"}
        ]}});
        assert!(explain_app_state(&[&pod]).to_lowercase().contains("room"));
    }

    #[test]
    fn a_pod_waiting_on_its_storage_is_not_told_there_is_no_room() {
        let pod = json!({"status": {"phase": "Pending", "conditions": [
            {"type": "PodScheduled", "status": "False", "reason": "Unschedulable",
             "message": "0/2 nodes are available: pod has unbound immediate PersistentVolumeClaims. not found"}
        ]}});
        let msg = explain_app_state(&[&pod]).to_lowercase();
        assert!(msg.contains("storage"), "{msg}");
        assert!(!msg.contains("room"), "{msg}");
    }

    fn cloning(name: &str, phase: &str) -> Value {
        json!({
            "metadata": {"name": name, "namespace": "yolab-notes"},
            "spec": {"dataSource": {"kind": "VolumeSnapshot", "name": "yolab-copy-abcd"}},
            "status": {"phase": phase},
        })
    }

    fn progress_event(name: &str, percent: f64, at: &str) -> Value {
        json!({
            "involvedObject": {"kind": "PersistentVolumeClaim", "namespace": "yolab-notes", "name": name},
            "lastTimestamp": at,
            "message": format!(
                "failed to provision volume: clone from snapshot is already in progress. \
                 progress report: percentage cloned={percent}%, amount cloned=88M/236M, \
                 files cloned=123/307"
            ),
        })
    }

    #[test]
    fn a_copy_in_progress_names_the_files_and_the_latest_percentage() {
        let pvc = cloning("minecraft-9hr2-data", "Pending");
        let events = [
            progress_event("minecraft-9hr2-data", 11.4, "2026-09-28T00:00:00Z"),
            progress_event("minecraft-9hr2-data", 37.287, "2026-09-28T00:01:00Z"),
        ];
        let mut by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
        by_ns.insert("yolab-notes", vec![&pvc]);
        assert_eq!(
            copying_data("yolab-notes", &by_ns, &events).as_deref(),
            Some("Copying this app's files… 37%")
        );
    }

    #[test]
    fn a_copy_before_the_first_percentage_still_reads_as_copying() {
        let pvc = cloning("minecraft-9hr2-data", "Pending");
        let mut by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
        by_ns.insert("yolab-notes", vec![&pvc]);
        assert_eq!(
            copying_data("yolab-notes", &by_ns, &[]).as_deref(),
            Some("Copying this app's files…")
        );
    }

    #[test]
    fn a_bound_volume_is_not_a_copy_in_progress() {
        let pvc = cloning("minecraft-9hr2-data", "Bound");
        let mut by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
        by_ns.insert("yolab-notes", vec![&pvc]);
        assert!(copying_data("yolab-notes", &by_ns, &[]).is_none());
    }

    #[test]
    fn running_but_not_answering_is_its_own_state() {
        let pod = json!({"status": {"phase": "Running", "containerStatuses": [
            {"restartCount": 0, "ready": false, "state": {"running": {}}}
        ]}});
        assert!(explain_app_state(&[&pod]).contains("Almost ready"));
    }

    #[test]
    fn an_unknown_reason_is_shown_not_invented_over() {
        let pod = waiting_pod("containerStatuses", "SomeFutureReason", 0);
        assert!(explain_app_state(&[&pod]).contains("SomeFutureReason"));
    }

    #[test]
    fn no_pods_at_all_says_it_is_waiting_for_a_machine() {
        assert!(explain_app_state(&[]).to_lowercase().contains("machine"));
    }

    #[test]
    fn a_stuck_init_container_is_not_hidden() {
        let pod = waiting_pod("initContainerStatuses", "CrashLoopBackOff", 4);
        assert!(explain_app_state(&[&pod]).contains("stopping"));
    }

    fn real_schema() -> Value {
        serde_json::json!({
            "properties": {
                "config": {
                    "properties": {
                        "subdomain": {
                            "type": "string",
                            "format": "tunnel",
                            "default": "qbittorrent"
                        },
                        "storage_size": {"type": "string"}
                    }
                },
                "yolab": {"type": "object"}
            }
        })
    }

    fn cfg(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect()
    }

    #[test]
    fn the_tunnel_field_is_found_under_config_properties() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[("subdomain", "qbittorrent")])),
            "qbittorrent"
        );
    }

    #[test]
    fn a_top_level_only_search_would_have_returned_nothing() {
        let schema = real_schema();
        let top_level_hit = schema["properties"]
            .as_object()
            .unwrap()
            .iter()
            .any(|(_, v)| v["format"].as_str() == Some("tunnel"));
        assert!(
            !top_level_hit,
            "the tunnel field is not at the top level — that was the bug"
        );
    }

    #[test]
    fn a_flat_schema_is_still_supported() {
        let schema = serde_json::json!({
            "properties": {"subdomain": {"format": "tunnel"}}
        });
        assert_eq!(
            resolve_service_name(&schema, &cfg(&[("subdomain", "flat")])),
            "flat"
        );
    }

    #[test]
    fn a_schema_with_no_tunnel_field_yields_empty() {
        let schema = serde_json::json!({
            "properties": {"config": {"properties": {"storage_size": {"type": "string"}}}}
        });
        assert_eq!(
            resolve_service_name(&schema, &cfg(&[("storage_size", "1Gi")])),
            ""
        );
    }

    #[test]
    fn a_missing_answer_falls_back_to_the_schema_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[])),
            "qbittorrent"
        );
    }

    #[test]
    fn an_empty_answer_also_falls_back_to_the_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[("subdomain", "")])),
            "qbittorrent"
        );
    }

    #[test]
    fn an_explicit_answer_beats_the_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[("subdomain", "torrents")])),
            "torrents"
        );
    }

    #[test]
    fn no_answer_and_no_default_yields_empty() {
        let schema = serde_json::json!({
            "properties": {"config": {"properties": {"subdomain": {"format": "tunnel"}}}}
        });
        assert_eq!(resolve_service_name(&schema, &cfg(&[])), "");
    }

    #[test]
    fn a_non_string_answer_falls_back_to_the_default() {
        let mut c = serde_json::Map::new();
        c.insert("subdomain".into(), serde_json::json!(42));
        assert_eq!(resolve_service_name(&real_schema(), &c), "qbittorrent");
    }

    #[test]
    fn a_schema_with_no_properties_at_all_yields_empty() {
        assert_eq!(resolve_service_name(&serde_json::json!({}), &cfg(&[])), "");
    }
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn a_long_name_still_fits_a_namespace() {
        let long = "a".repeat(200);
        let stem = instance_stem(&long).unwrap();
        let ns = format!("yolab-{stem}-{}", instance_suffix());
        assert!(
            ns.len() <= MAX_NS_LEN,
            "namespace {} chars, limit {MAX_NS_LEN}: {ns}",
            ns.len()
        );
    }

    #[test]
    fn an_ordinary_name_is_left_alone() {
        assert_eq!(instance_stem("filebrowser").as_deref(), Some("filebrowser"));
    }

    #[test]
    fn a_trailing_hyphen_is_trimmed() {
        assert_eq!(instance_stem("my-app-").as_deref(), Some("my-app"));
        assert_eq!(instance_stem("my-app---").as_deref(), Some("my-app"));
    }

    #[test]
    fn a_name_with_nothing_left_is_rejected() {
        assert!(instance_stem("---").is_none());
        assert!(instance_stem("").is_none());
    }

    #[test]
    fn the_suffix_avoids_characters_that_are_misread() {
        for _ in 0..200 {
            let s = instance_suffix();
            assert_eq!(s.len(), INSTANCE_SUFFIX_LEN);
            for c in s.chars() {
                assert!(
                    c.is_ascii_lowercase() || c.is_ascii_digit(),
                    "suffix must survive the instance_name validation: {s}"
                );
                assert!(
                    !"lo01".contains(c),
                    "{c} is easily misread, and these get typed back: {s}"
                );
            }
        }
    }

    #[test]
    fn split_instance_name_recovers_what_unique_instance_name_built() {
        for stem in ["filebrowser", "filebrowser-2", "my-blog"] {
            let id = instance_suffix();
            let name = format!("{stem}-{id}");
            assert_eq!(split_instance_name(&name), (stem, Some(id.as_str())));
        }
        assert_eq!(
            split_instance_name("filebrowser-pgxw"),
            ("filebrowser", Some("pgxw"))
        );
        assert_eq!(split_instance_name("nextcloud-2"), ("nextcloud-2", None));
        assert_eq!(split_instance_name("gitea"), ("gitea", None));
        assert_eq!(split_instance_name("app-ab01"), ("app-ab01", None));
        assert_eq!(split_instance_name("-pgxw"), ("-pgxw", None));
    }

    #[test]
    fn two_installs_of_one_name_differ() {
        let names: std::collections::HashSet<String> = (0..50).map(|_| instance_suffix()).collect();
        assert!(
            names.len() > 40,
            "suffixes are barely varying, which defeats the point: {} distinct of 50",
            names.len()
        );
    }

    #[test]
    fn derive_domain_drops_subdomain() {
        assert_eq!(derive_domain("https://yolab.10.yolab.io"), "10.yolab.io");
        assert_eq!(derive_domain("http://node1.example.com/"), "example.com");
    }

    #[test]
    fn derive_domain_keeps_numeric_first_label() {
        assert_eq!(derive_domain("https://127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn derive_domain_single_label() {
        assert_eq!(derive_domain("https://localhost"), "localhost");
    }

    #[test]
    fn validate_config_rejects_newline() {
        let cfg = map(json!({ "domain": "a.com\nmalicious: true" }));
        assert!(validate_config_values(&cfg).is_err());
    }

    #[test]
    fn validate_config_allows_tab_and_normal() {
        let cfg = map(json!({ "name": "hello world\ttabbed", "size": 10, "on": true }));
        assert!(validate_config_values(&cfg).is_ok());
    }

    #[test]
    fn validate_config_checks_nested() {
        let cfg = map(json!({ "outer": { "inner": ["ok", "bad\r"] } }));
        assert!(validate_config_values(&cfg).is_err());
    }

    #[test]
    fn validate_config_rejects_oversized() {
        let big = "x".repeat(8193);
        let cfg = map(json!({ "blob": big }));
        assert!(validate_config_values(&cfg).is_err());
    }

    #[test]
    fn no_lock_annotation_is_not_fresh() {
        assert!(!uninstall_lock_is_fresh(&serde_json::Map::new()));
    }

    #[test]
    fn a_lock_claimed_moments_ago_is_fresh() {
        let ts = chrono::Utc::now().to_rfc3339();
        assert!(uninstall_lock_is_fresh(&map(
            serde_json::json!({ ANN_UNINSTALLING: ts })
        )));
    }

    #[test]
    fn a_lock_past_the_ttl_is_not_fresh() {
        let ts = (chrono::Utc::now()
            - chrono::Duration::seconds(UNINSTALL_LOCK_TTL.as_secs() as i64 + 1))
        .to_rfc3339();
        assert!(!uninstall_lock_is_fresh(&map(
            serde_json::json!({ ANN_UNINSTALLING: ts })
        )));
    }

    #[test]
    fn the_watchdog_only_picks_up_abandoned_uninstalls() {
        let stale = (chrono::Utc::now()
            - chrono::Duration::seconds(UNINSTALL_LOCK_TTL.as_secs() as i64 + 1))
        .to_rfc3339();
        let fresh = chrono::Utc::now().to_rfc3339();

        let list = serde_json::json!([
            { "metadata": { "name": "yolab-minecraft",
                            "annotations": { ANN_UNINSTALLING: stale } } },
            { "metadata": { "name": "yolab-filebrowser",
                            "annotations": { ANN_UNINSTALLING: fresh } } },
            { "metadata": { "name": "yolab-vaultwarden",
                            "annotations": { "yolab.io/app-id": "vaultwarden" } } },
            { "metadata": { "name": "yolab-babybuddy" } },
        ]);

        assert_eq!(
            abandoned_in(list.as_array().unwrap()),
            vec![("yolab-minecraft".to_string(), "minecraft".to_string())],
            "only the stale claim, and the instance name comes off the prefix"
        );
    }

    #[test]
    fn the_watchdog_ignores_namespaces_outside_the_yolab_prefix() {
        let stale = (chrono::Utc::now()
            - chrono::Duration::seconds(UNINSTALL_LOCK_TTL.as_secs() as i64 + 1))
        .to_rfc3339();
        let list = vec![serde_json::json!(
            { "metadata": { "name": "kube-system",
                            "annotations": { ANN_UNINSTALLING: stale } } }
        )];
        assert!(abandoned_in(&list).is_empty());
    }

    #[test]
    fn no_namespaces_means_nothing_abandoned() {
        assert!(abandoned_in(&[]).is_empty());
    }

    #[test]
    fn an_unparsable_lock_timestamp_is_not_fresh() {
        assert!(!uninstall_lock_is_fresh(&map(
            serde_json::json!({ ANN_UNINSTALLING: "not-a-timestamp" })
        )));
    }

    fn chart_dir_with(schema: Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let chart = dir.path().join("filebrowser");
        std::fs::create_dir_all(&chart).unwrap();
        std::fs::write(
            chart.join("Chart.yaml"),
            "apiVersion: v2\nname: filebrowser\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(chart.join("values.schema.json"), schema.to_string()).unwrap();
        dir
    }

    fn filebrowser_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "config": { "type": "object", "properties": {
                    "password": { "type": "string", "writeOnly": true, "generate": true },
                    "file_explorer_enabled": { "type": "boolean", "default": true }
                }},
                "outputs": { "type": "object", "readOnly": true, "properties": {
                    "url": { "type": "string", "format": "uri",
                             "source": { "logs": "YOLAB_OUTPUT url (\\S+)" } },
                    "password": { "type": "string", "format": "secret",
                                  "source": { "config": "password" } },
                    "file_explorer_password": { "type": "string", "format": "secret",
                        "source": { "logs": "YOLAB_OUTPUT file_explorer_password (\\S+)" },
                        "when": { "properties": { "file_explorer_enabled": { "const": true } } } }
                }}
            }
        })
    }

    fn keys_of(outputs: &[crate::outputs::ShownOutput]) -> Vec<&str> {
        outputs.iter().map(|o| o.key.as_str()).collect()
    }

    #[test]
    fn an_apps_schema_is_read_off_its_chart() {
        let dir = chart_dir_with(filebrowser_schema());
        let app = app_schema(dir.path(), "filebrowser");
        assert_eq!(app.outputs().len(), 3);
        assert!(app.credentials().contains("password"));
    }

    #[test]
    fn an_app_missing_from_the_catalog_has_no_schema() {
        let dir = chart_dir_with(filebrowser_schema());
        assert!(app_schema(dir.path(), "not-in-the-catalog")
            .outputs()
            .is_empty());
        assert!(app_schema(dir.path(), "").outputs().is_empty());
    }

    #[test]
    fn the_listing_never_shows_a_redacted_credential_as_its_value() {
        let dir = chart_dir_with(filebrowser_schema());
        let settings = map(json!({ "password": REDACTED, "file_explorer_enabled": true }));
        let rows = listed_outputs(
            &app_schema(dir.path(), "filebrowser"),
            Default::default(),
            &serde_json::Map::new(),
            &settings,
        );
        let password = rows.iter().find(|o| o.key == "password").unwrap();
        assert_eq!(password.value, None);
    }

    #[test]
    fn the_listing_leaves_out_outputs_of_a_feature_that_is_off() {
        let dir = chart_dir_with(filebrowser_schema());
        let settings = map(json!({ "file_explorer_enabled": false }));
        let rows = listed_outputs(
            &app_schema(dir.path(), "filebrowser"),
            Default::default(),
            &serde_json::Map::new(),
            &settings,
        );
        assert_eq!(keys_of(&rows), vec!["url", "password"]);
    }

    #[test]
    fn the_listing_shows_values_scanned_before_the_outputs_moved_to_a_secret() {
        let dir = chart_dir_with(filebrowser_schema());
        let ann = map(json!({
            "yolab.io/outputs": r#"[{"key":"url","label":"Web URL","value":"https://files.x","type":"url"}]"#
        }));
        let rows = listed_outputs(
            &app_schema(dir.path(), "filebrowser"),
            Default::default(),
            &ann,
            &serde_json::Map::new(),
        );
        assert_eq!(rows[0].value.as_deref(), Some("https://files.x"));
    }

    #[test]
    fn the_saved_settings_are_read_from_the_namespace() {
        let ann = map(json!({ ANN_CONFIG: r#"{"file_explorer_enabled":false}"# }));
        assert_eq!(saved_settings(&ann)["file_explorer_enabled"], json!(false));
        assert!(saved_settings(&serde_json::Map::new()).is_empty());
    }

    mod outputs_endpoints {
        use super::*;
        use crate::k8s::testing::{
            accept_patches, api_server, asked, list, patched, secret_with, serve, serve_logs,
            status,
        };

        const NS: &str = "yolab-filebrowser-ab12";
        const NS_PATH: &str = "/api/v1/namespaces/yolab-filebrowser-ab12";
        const SECRET_PATH: &str = "/api/v1/namespaces/yolab-filebrowser-ab12/secrets/yolab-outputs";

        fn namespace_with(annotations: Value) -> Value {
            json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": NS, "annotations": annotations } })
        }

        fn filebrowser_ns(explorer: bool) -> Value {
            namespace_with(json!({
                ANN_APP_ID: "filebrowser",
                ANN_CONFIG: format!(r#"{{"password":"{REDACTED}","file_explorer_enabled":{explorer}}}"#)
            }))
        }

        fn stored_secret(key: &str, value: &str) -> Value {
            secret_with(
                "outputs.json",
                &format!(r#"{{"{key}":{{"value":"{value}","found_at":"2026-09-28T12:00:00Z"}}}}"#),
            )
        }

        fn gone() -> Value {
            status(404, "NotFound")
        }

        #[tokio::test]
        async fn showing_the_page_reads_what_is_remembered_without_touching_the_logs() {
            let catalog = chart_dir_with(filebrowser_schema());
            let (server, kube) = api_server().await;
            serve(&server, NS_PATH, 200, filebrowser_ns(true)).await;
            serve(
                &server,
                SECRET_PATH,
                200,
                stored_secret("url", "https://files.x"),
            )
            .await;

            let known = known_outputs(&kube, catalog.path(), NS, false)
                .await
                .unwrap();

            assert_eq!(known.remembered["url"].value, "https://files.x");
            assert!(!asked(&server, "/pods").await);
        }

        #[tokio::test]
        async fn check_again_reads_the_logs_now() {
            let catalog = chart_dir_with(filebrowser_schema());
            let (server, kube) = api_server().await;
            serve(&server, NS_PATH, 200, filebrowser_ns(true)).await;
            serve(&server, SECRET_PATH, 404, gone()).await;
            serve(
                &server,
                &format!("{NS_PATH}/pods"),
                200,
                list("Pod", vec![json!({ "metadata": { "name": "gw" },
                    "spec": { "initContainers": [{ "name": "file-explorer-init" }], "containers": [] } })]),
            )
            .await;
            serve_logs(
                &server,
                &format!("{NS_PATH}/pods/gw"),
                "file-explorer-init",
                "2026-09-28T12:00:00Z YOLAB_OUTPUT file_explorer_password pw123",
            )
            .await;
            accept_patches(&server).await;

            let known = known_outputs(&kube, catalog.path(), NS, true)
                .await
                .unwrap();

            assert_eq!(known.remembered["file_explorer_password"].value, "pw123");
            assert_eq!(patched(&server).await.len(), 1);
        }

        #[tokio::test]
        async fn the_page_only_expects_outputs_that_apply_to_this_install() {
            let catalog = chart_dir_with(filebrowser_schema());
            let (server, kube) = api_server().await;
            serve(&server, NS_PATH, 200, filebrowser_ns(false)).await;
            serve(&server, SECRET_PATH, 404, gone()).await;

            let known = known_outputs(&kube, catalog.path(), NS, false)
                .await
                .unwrap();

            let keys: Vec<&str> = known.specs.iter().map(|s| s.key.as_str()).collect();
            assert_eq!(keys, vec!["url", "password"]);
        }

        #[tokio::test]
        async fn values_found_by_the_old_scanner_still_show_before_the_first_rescan() {
            let catalog = chart_dir_with(filebrowser_schema());
            let (server, kube) = api_server().await;
            serve(
                &server,
                NS_PATH,
                200,
                namespace_with(json!({
                    ANN_APP_ID: "filebrowser",
                    "yolab.io/outputs": r#"[{"key":"url","value":"https://old.x","type":"url"}]"#
                })),
            )
            .await;
            serve(&server, SECRET_PATH, 404, gone()).await;

            let known = known_outputs(&kube, catalog.path(), NS, false)
                .await
                .unwrap();

            assert_eq!(known.remembered["url"].value, "https://old.x");
        }

        #[tokio::test]
        async fn an_app_whose_namespace_cannot_be_read_is_an_error() {
            let catalog = chart_dir_with(filebrowser_schema());
            let (server, kube) = api_server().await;
            serve(&server, NS_PATH, 503, status(503, "ServiceUnavailable")).await;
            assert!(known_outputs(&kube, catalog.path(), NS, false)
                .await
                .is_err());
        }

        #[tokio::test]
        async fn installed_apps_leaves_out_what_is_being_removed_or_is_not_an_app() {
            let (server, kube) = api_server().await;
            serve(
                &server,
                "/api/v1/namespaces",
                200,
                list(
                    "Namespace",
                    vec![
                        json!({ "metadata": { "name": "yolab-a", "annotations": { ANN_APP_ID: "gitea" } },
                          "status": { "phase": "Active" } }),
                        json!({ "metadata": { "name": "yolab-b", "annotations": { ANN_APP_ID: "gitea" } },
                          "status": { "phase": "Terminating" } }),
                        json!({ "metadata": { "name": "yolab-c", "annotations": {} },
                          "status": { "phase": "Active" } }),
                        json!({ "metadata": { "name": "yolab-d" }, "status": { "phase": "Active" } }),
                    ],
                ),
            )
            .await;

            let apps = installed_apps(&kube).await.unwrap();

            let names: Vec<&str> = apps.iter().map(|a| a.namespace.as_str()).collect();
            assert_eq!(names, vec!["yolab-a"]);
            assert_eq!(apps[0].app_id, "gitea");
        }
    }

    #[test]
    fn saved_settings_are_read_exactly_or_reported() {
        use std::collections::HashMap;
        let ok = HashMap::from([(
            CONFIG_SECRET_KEY.to_string(),
            r#"{"password":"hunter2"}"#.to_string(),
        )]);
        let cfg = parse_saved_config("yolab-a", &ok).unwrap();
        assert_eq!(cfg["password"], "hunter2");

        let missing = HashMap::new();
        let e = parse_saved_config("yolab-a", &missing)
            .unwrap_err()
            .to_string();
        assert!(e.contains(CONFIG_SECRET_KEY), "{e}");

        let junk = HashMap::from([(CONFIG_SECRET_KEY.to_string(), "not json".to_string())]);
        let e = parse_saved_config("yolab-a", &junk)
            .unwrap_err()
            .to_string();
        assert!(e.contains("unreadable"), "{e}");
    }

    #[test]
    fn requested_resources_add_up_across_replicas_and_containers() {
        let items = vec![
            json!({ "spec": { "replicas": 2, "template": { "spec": { "containers": [
                { "resources": { "requests": { "cpu": "250m", "memory": "64Mi", "nvidia.com/gpu": "1" } } },
                { "resources": {} }
            ] } } } }),
            json!({ "spec": { "template": { "spec": { "containers": [
                { "resources": { "requests": { "cpu": "1" } } }
            ] } } } }),
        ];
        let r = requested_resources(&items);
        assert_eq!(r.replicas, 3);
        assert_eq!(r.cpu_millicores, 1500);
        assert_eq!(r.memory_bytes, 2 * 64 * 1024 * 1024);
        assert_eq!(r.gpu, 2);
    }

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list, status};
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const NS_PATH: &str = "/api/v1/namespaces/yolab-notes";

        async fn namespace_is(server: &MockServer, code: u16, body: Value) {
            Mock::given(method("GET"))
                .and(path(NS_PATH))
                .respond_with(ResponseTemplate::new(code).set_body_json(body))
                .mount(server)
                .await;
        }

        fn ns(annotations: Value, phase: &str) -> Value {
            json!({
                "metadata": { "name": "yolab-notes", "resourceVersion": "42", "annotations": annotations },
                "status": { "phase": phase }
            })
        }

        fn gone() -> Value {
            status(404, "NotFound")
        }

        #[tokio::test]
        async fn an_app_that_is_already_gone_may_be_uninstalled() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 404, gone()).await;
            assert!(claim_uninstall_lock(&kube, "yolab-notes").await.unwrap());
        }

        #[tokio::test]
        async fn a_fresh_uninstall_is_not_started_twice() {
            let (server, kube) = api_server().await;
            let now = chrono::Utc::now().to_rfc3339();
            namespace_is(&server, 200, ns(json!({ ANN_UNINSTALLING: now }), "Active")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            assert!(!claim_uninstall_lock(&kube, "yolab-notes").await.unwrap());
        }

        #[tokio::test]
        async fn the_uninstall_claim_is_conditional_on_what_was_read() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 200, ns(json!({}), "Active")).await;
            Mock::given(method("PATCH"))
                .and(path(NS_PATH))
                .and(body_partial_json(
                    json!({ "metadata": { "resourceVersion": "42" } }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(ns(json!({}), "Active")))
                .expect(1)
                .mount(&server)
                .await;
            assert!(claim_uninstall_lock(&kube, "yolab-notes").await.unwrap());
        }

        #[tokio::test]
        async fn losing_the_race_for_the_uninstall_claim_is_not_an_error() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 200, ns(json!({}), "Active")).await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(409).set_body_json(status(409, "Conflict")))
                .mount(&server)
                .await;
            assert!(!claim_uninstall_lock(&kube, "yolab-notes").await.unwrap());
        }

        #[tokio::test]
        async fn an_unreachable_cluster_blocks_the_uninstall_claim() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 503, status(503, "ServiceUnavailable")).await;
            assert!(claim_uninstall_lock(&kube, "yolab-notes").await.is_err());
        }

        #[tokio::test]
        async fn teardown_uninstalls_the_release_then_deletes_the_namespace() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 200, ns(json!({}), "Active")).await;
            Mock::given(method("DELETE"))
                .and(path(NS_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ns(json!({}), "Terminating")),
                )
                .expect(1)
                .mount(&server)
                .await;
            let b = Backend {
                kube,
                host: FakeHost::new().ok("helm uninstall", ""),
            };

            run_teardown(&b, "notes", "yolab-notes").await;
            assert!(b
                .host
                .ran("helm uninstall notes -n yolab-notes --ignore-not-found --wait"));
        }

        #[tokio::test]
        async fn a_namespace_already_going_away_is_not_handed_to_helm_again() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 200, ns(json!({}), "Terminating")).await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            run_teardown(&b, "notes", "yolab-notes").await;
            assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
        }

        #[tokio::test]
        async fn a_failed_helm_uninstall_still_deletes_the_namespace() {
            let (server, kube) = api_server().await;
            namespace_is(&server, 200, ns(json!({}), "Active")).await;
            Mock::given(method("DELETE"))
                .and(path(NS_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ns(json!({}), "Terminating")),
                )
                .expect(1)
                .mount(&server)
                .await;
            let b = Backend {
                kube,
                host: FakeHost::new().fail("helm uninstall", "timed out"),
            };
            run_teardown(&b, "notes", "yolab-notes").await;
        }

        #[tokio::test]
        async fn a_failed_install_is_rolled_back_by_release_and_namespace() {
            let (server, kube) = api_server().await;
            Mock::given(method("DELETE"))
                .and(path(NS_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(ns(json!({}), "Terminating")),
                )
                .expect(1)
                .mount(&server)
                .await;
            let b = Backend {
                kube,
                host: FakeHost::new().ok("helm uninstall", ""),
            };
            rollback_failed_install(&b, "yolab-notes", "notes").await;
            assert!(b.host.ran("helm uninstall notes -n yolab-notes"));
        }

        #[tokio::test]
        async fn no_name_is_handed_out_while_the_cluster_cannot_say_it_is_free() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            assert_eq!(unique_instance_name(&kube, "notes").await, None);
        }

        #[tokio::test]
        async fn a_free_name_keeps_the_requested_stem() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            let name = unique_instance_name(&kube, "notes").await.unwrap();
            assert_eq!(split_instance_name(&name).0, "notes");
        }

        #[tokio::test]
        async fn settings_without_a_saved_definition_are_rebuilt_from_the_namespace() {
            use base64::Engine as _;
            let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces/yolab-notes/secrets/yolab-config"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "metadata": { "name": "yolab-config", "namespace": "yolab-notes" },
                    "data": { "config.json": b(r#"{"title":"mine"}"#) }
                })))
                .mount(&server)
                .await;
            namespace_is(
                &server,
                200,
                ns(
                    json!({ ANN_APP_ID: "notes", ANN_CHART_VERSION: "1.2.0" }),
                    "Active",
                ),
            )
            .await;

            let def = read_definition_from_cluster(&kube, "yolab-notes")
                .await
                .unwrap();
            assert_eq!(def.app_id, "notes");
            assert_eq!(def.chart_version, "1.2.0");
            assert_eq!(def.instance_name, "notes");
            assert_eq!(def.config["title"], "mine");
        }

        #[tokio::test]
        async fn an_app_without_saved_settings_is_an_error() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            let e = read_definition_from_cluster(&kube, "yolab-notes")
                .await
                .unwrap_err();
            assert!(e.to_string().contains("no saved settings"), "{e}");
        }

        #[tokio::test]
        async fn the_pods_route_lists_the_apps_pods_but_not_backup_movers() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces/yolab-notes/pods"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Pod",
                    vec![
                        json!({ "metadata": { "name": "notes-0" }, "status": { "phase": "Running",
                            "conditions": [{ "type": "Ready", "status": "True" }] } }),
                        json!({ "metadata": { "name": "volsync-src-notes-x" }, "status": { "phase": "Running" } }),
                    ],
                )))
                .mount(&server)
                .await;
            let api = crate::testkit::TestApi::with_kube(kube).login().await;

            let res = api.get("/api/apps/notes/pods").await;
            assert_eq!(res.status, axum::http::StatusCode::OK, "{}", res.body);
            assert_eq!(
                res.json(),
                json!([{ "name": "notes-0", "phase": "Running", "ready": true }])
            );
        }

        #[tokio::test]
        async fn the_repos_route_always_offers_the_official_catalog() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path(
                    "/api/v1/namespaces/kube-system/configmaps/yolab-chart-repos",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "metadata": { "name": "yolab-chart-repos", "namespace": "kube-system" },
                    "data": { "community": "https://charts.example/catalog.yaml" }
                })))
                .mount(&server)
                .await;
            let api = crate::testkit::TestApi::with_kube(kube).login().await;

            let repos = api.get("/api/apps/repos").await.json();
            let names: Vec<&str> = repos
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["official", "community"]);
        }
        #[tokio::test]
        async fn logs_follow_every_container_each_line_marked_with_where_it_came_from() {
            use futures::StreamExt as _;
            use wiremock::matchers::query_param;
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces/yolab-notes/pods/notes-0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "metadata": { "name": "notes-0", "namespace": "yolab-notes" },
                    "spec": {
                        "initContainers": [{ "name": "setup" }],
                        "containers": [{ "name": "app" }]
                    }
                })))
                .mount(&server)
                .await;
            for (container, text) in [("setup", "migrated\n"), ("app", "listening\nready\n")] {
                Mock::given(method("GET"))
                    .and(path("/api/v1/namespaces/yolab-notes/pods/notes-0/log"))
                    .and(query_param("container", container))
                    .and(query_param("follow", "true"))
                    .and(query_param("tailLines", "100"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(text))
                    .mount(&server)
                    .await;
            }

            let mut lines: Vec<String> = follow_pod_logs(&kube, "yolab-notes", "notes-0")
                .await
                .unwrap()
                .collect()
                .await;
            lines.sort();
            assert_eq!(
                lines,
                vec![
                    "[pod/notes-0/app] listening",
                    "[pod/notes-0/app] ready",
                    "[pod/notes-0/setup] migrated",
                ]
            );
        }

        #[tokio::test]
        async fn logs_of_a_pod_that_does_not_exist_are_an_error() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            assert!(follow_pod_logs(&kube, "yolab-notes", "nope").await.is_err());
        }
    }
}
