use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Outcome;
use crate::routers::install;
use crate::{config::Config, error::Result, proc::KillOnDrop, AppState};

const LABEL_MANAGED: &str = "yolab.io/managed";
pub(crate) const ANN_APP_ID: &str = "yolab.io/app-id";
pub(crate) const ANN_CHART_VERSION: &str = "yolab.io/chart-version";
pub(crate) const ANN_CHART_REPO: &str = "yolab.io/chart-repo";

const ANN_CONFIG: &str = "yolab.io/config";
const ANN_BACKUP: &str = "yolab.io/backup";
const ANN_OUTPUTS: &str = "yolab.io/outputs";
const ANN_UNINSTALLING: &str = "yolab.io/uninstalling";
const UNINSTALL_LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const LOGS_SCAN_TAIL: u32 = 500;
const LOGS_FOLLOW_TAIL: u32 = 100;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppOutput {
    pub key: String,
    pub label: String,
    pub value: String,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Serialize, Clone)]
pub struct OutputSpec {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Serialize)]
pub struct AppInfo {
    pub app_id: String,
    pub instance_name: String,
    pub instance_id: Option<String>,
    pub status: String,
    pub detail: String,
    pub outputs: Vec<AppOutput>,
    pub outputs_spec: Vec<OutputSpec>,
    pub config: serde_json::Map<String, Value>,
    pub backup: AppBackupStatus,
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
    pub uischema: Value,
}

#[derive(Serialize)]
pub struct PodInfo {
    pub name: String,
    pub phase: String,
    pub ready: bool,
}

#[derive(Serialize)]
pub struct ScanOutputsResponse {
    pub outputs: Vec<AppOutput>,
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

async fn annotate_ns(ns: &str, key: &str, value: &str) {
    if let Err(e) = crate::kubectl::run(&[
        "annotate",
        "namespace",
        ns,
        &format!("{key}={value}"),
        "--overwrite=true",
    ])
    .await
    {
        tracing::warn!("annotate {ns} {key} failed: {e}");
    }
}

const CONFIG_SECRET: &str = "yolab-config";
const CONFIG_SECRET_KEY: &str = "config.json";
const REDACTED: &str = "__redacted__";

fn credential_fields(uischema: &Value) -> std::collections::HashSet<String> {
    uischema
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, spec)| spec["ui:widget"] == "PasswordWidget")
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

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

async fn read_config(ns: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
    let data = crate::kubectl::get_secret(CONFIG_SECRET, ns)
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
    ns: &str,
    def: &AppDefinition,
    uischema: &Value,
) -> anyhow::Result<()> {
    let full = serde_json::to_string(def)?;
    let config_json = serde_json::to_string(&def.config)?;
    crate::kubectl::apply_secret(
        CONFIG_SECRET,
        ns,
        &[
            (CONFIG_SECRET_KEY, config_json.as_str()),
            (DEFINITION_SECRET_KEY, full.as_str()),
        ],
        &[("yolab.io/managed", "true")],
    )
    .await?;
    let redacted = redact_credentials(&def.config, &credential_fields(uischema));
    annotate_ns(ns, ANN_CONFIG, &serde_json::to_string(&redacted)?).await;
    annotate_ns(ns, ANN_BACKUP, &serde_json::to_string(&def.backup)?).await;
    Ok(())
}

pub(crate) async fn read_definition(ns: &str) -> anyhow::Result<AppDefinition> {
    let data = crate::kubectl::get_secret(CONFIG_SECRET, ns)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} has no saved settings (no {CONFIG_SECRET} Secret)"))?;
    if let Some(raw) = data.get(DEFINITION_SECRET_KEY) {
        if let Ok(def) = serde_json::from_str::<AppDefinition>(raw) {
            return Ok(def);
        }
        tracing::warn!("{ns}: {DEFINITION_SECRET_KEY} is unreadable — falling back to annotations");
    }
    let config = parse_saved_config(ns, &data)?;
    let ns_v = crate::kubectl::get_opt(&["get", "namespace", ns, "-o", "json"])
        .await?
        .unwrap_or(Value::Null);
    Ok(definition_from_annotations(&ns_v, config))
}

pub(crate) async fn read_definition_opt(ns: &str) -> Option<AppDefinition> {
    read_definition(ns).await.ok()
}

pub(crate) fn redact_definition(
    def: &AppDefinition,
    catalog_dir: &std::path::Path,
) -> AppDefinition {
    let ui = chart_uischema(catalog_dir, &def.app_id);
    let mut d = def.clone();
    d.config = redact_credentials(&def.config, &credential_fields(&ui));
    d
}

pub(crate) fn merge_credentials(
    mut incoming: serde_json::Map<String, Value>,
    stored: &serde_json::Map<String, Value>,
    uischema: &Value,
) -> serde_json::Map<String, Value> {
    for field in credential_fields(uischema) {
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

pub(crate) async fn collect_runtime(ns: &str) -> (Vec<VolumeSpec>, ResourceSpec) {
    let volumes = crate::routers::backup_common::list_user_pvcs()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.namespace == ns)
        .map(|p| VolumeSpec {
            name: p.name,
            capacity: p.capacity,
        })
        .collect();

    let mut resources = ResourceSpec::default();
    if let Ok(v) = crate::kubectl::get_json(&[
        "get",
        "deploy,statefulset,daemonset",
        "-n",
        ns,
        "-o",
        "json",
    ])
    .await
    {
        for item in v["items"].as_array().into_iter().flatten() {
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
    (volumes, resources)
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
const ANN_UISCHEMA: &str = "yolab.io/uischema";
const ANN_CHART_OUTPUTS: &str = "yolab.io/outputs";

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
    schema: Value,
}

impl ChartMeta {
    fn ann(&self, key: &str) -> &str {
        self.chart
            .annotations
            .get(key)
            .map(String::as_str)
            .unwrap_or("")
    }
    fn ann_json(&self, key: &str) -> Value {
        serde_json::from_str(self.ann(key)).unwrap_or(Value::Null)
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
        .map(|v| v["properties"]["config"].clone())
        .unwrap_or(Value::Null);
    Some(ChartMeta { chart, schema })
}

pub(crate) fn chart_uischema(catalog_dir: &std::path::Path, id: &str) -> Value {
    if id.is_empty() {
        return Value::Null;
    }
    read_chart(&catalog_dir.join(id))
        .map(|m| m.ann_json(ANN_UISCHEMA))
        .unwrap_or(Value::Null)
}

fn chart_outputs_spec(catalog_dir: &std::path::Path, id: &str) -> Vec<Value> {
    if id.is_empty() {
        return Vec::new();
    }
    read_chart(&catalog_dir.join(id))
        .map(|m| m.ann_json(ANN_CHART_OUTPUTS))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
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

async fn ensure_tunnel_credentials(ns: &str, tunnel_cfg: &toml::Table) -> anyhow::Result<()> {
    let token = tunnel_cfg
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    crate::kubectl::apply_secret(
        "yolab-tunnel-credentials",
        ns,
        &[("account-token", token)],
        &[("app.kubernetes.io/managed-by", "yolab")],
    )
    .await?;
    Ok(())
}

async fn ensure_app_namespace(
    ns: &str,
    app_id: &str,
    repo: &str,
    chart_version: &str,
) -> anyhow::Result<()> {
    crate::kubectl::apply(
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
        })
        .to_string(),
    )
    .await?;
    Ok(())
}

fn normalize_outputs(ann: &serde_json::Map<String, Value>) -> Vec<AppOutput> {
    let raw = ann.get(ANN_OUTPUTS).and_then(|v| v.as_str()).unwrap_or("");
    if raw.is_empty() {
        return vec![];
    }
    serde_json::from_str::<Vec<AppOutput>>(raw).unwrap_or_else(|e| {
        tracing::warn!("{ANN_OUTPUTS} does not hold a list of outputs ({e}) — showing none");
        vec![]
    })
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
        schema: meta.schema.clone(),
        uischema: meta.ann_json(ANN_UISCHEMA),
    }
}

pub async fn refresh_catalog_app(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let mut refreshed = false;
    let mut note = String::new();

    for repo in crate::charts::list_repos().await {
        match crate::charts::sync_chart(&repo, &id).await {
            Ok(()) => {
                refreshed = true;
                break;
            }
            Err(e) => note = e.to_string(),
        }
    }

    let entry = crate::charts::chart_sources()
        .await
        .into_iter()
        .find_map(|(repo, dir)| {
            let m = read_chart(&dir.join(&id))?;
            Some(catalog_entry_from(repo, m))
        });

    Json(serde_json::json!({
        "refreshed": refreshed,
        "note": note,
        "app": entry,
    }))
}

pub async fn catalog(State(_state): State<AppState>) -> Json<Vec<CatalogApp>> {
    let mut apps: Vec<CatalogApp> = vec![];
    let mut seen: std::collections::HashSet<String> = Default::default();

    for (repo, dir) in crate::charts::chart_sources().await {
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
    Json(apps)
}

#[derive(Deserialize)]
pub struct AddRepoBody {
    pub name: String,
    pub url: String,
}

pub async fn list_repos(State(_s): State<AppState>) -> Json<Vec<crate::charts::ChartRepo>> {
    Json(crate::charts::list_repos().await)
}

pub async fn add_repo(
    State(_s): State<AppState>,
    Json(body): Json<AddRepoBody>,
) -> impl IntoResponse {
    if let Err(e) = crate::charts::add_repo(&body.name, &body.url).await {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let repos = crate::charts::list_repos().await;
    if let Some(r) = repos.iter().find(|r| r.name == body.name) {
        if let Err(e) = crate::charts::sync_repo(r).await {
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
    State(_s): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match crate::charts::remove_repo(&name).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn sync_repos(State(_s): State<AppState>) -> Json<serde_json::Value> {
    let mut results = serde_json::Map::new();
    for repo in crate::charts::list_repos().await {
        let entry = match crate::charts::sync_repo(&repo).await {
            Ok(n) => serde_json::json!({ "ok": true, "charts": n }),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        };
        results.insert(repo.name.clone(), entry);
    }
    Json(Value::Object(results))
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
    let mut running_not_ready = false;

    for pod in pods {
        if pod["status"]["phase"].as_str() == Some("Pending") {
            unschedulable |= pod["status"]["conditions"]
                .as_array()
                .map(|cs| {
                    cs.iter()
                        .any(|c| c["type"] == "PodScheduled" && c["reason"] == "Unschedulable")
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
        return "No machine has room for this app right now.".into();
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

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let catalog_dir = state.config.catalog_dir();
    let backup_status = crate::routers::backup::app_backup_status().await;
    let ns_selector = format!("{LABEL_MANAGED}=true");
    let ns_args = ["get", "namespaces", "-l", &ns_selector, "-o", "json"];
    let pod_args = ["get", "pods", "--all-namespaces", "-o", "json"];
    let (ns_out, pods_out) = tokio::join!(
        crate::kubectl::get_json(&ns_args),
        crate::kubectl::get_json(&pod_args),
    );
    let v: Value = ns_out?;

    let pods_v: Value = pods_out.unwrap_or_else(|_| serde_json::json!({"items": []}));
    let empty_pods: Vec<Value> = vec![];
    let all_pod_items = pods_v["items"].as_array().unwrap_or(&empty_pods);
    let mut pods_by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
    for pod in all_pod_items {
        if let Some(ns) = pod["metadata"]["namespace"].as_str() {
            pods_by_ns.entry(ns).or_default().push(pod);
        }
    }

    let mut apps = vec![];
    let empty_ns: Vec<Value> = vec![];
    for ns in v["items"].as_array().unwrap_or(&empty_ns) {
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
        let mut detail = String::new();
        let status = if phase == "Terminating" || uninstall_lock_is_fresh(&ann) {
            "uninstalling".to_string()
        } else {
            let ns_full = format!("yolab-{name}");
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
        let config: serde_json::Map<String, Value> = ann
            .get(ANN_CONFIG)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let outputs_spec = chart_outputs_spec(&catalog_dir, &id)
            .into_iter()
            .filter(|o| o["type"].as_str() != Some("hidden"))
            .filter_map(|o| {
                Some(OutputSpec {
                    key: o["key"].as_str()?.to_string(),
                    label: o
                        .get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or(o["key"].as_str()?)
                        .to_string(),
                    type_: o
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text")
                        .to_string(),
                })
            })
            .collect();

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
            outputs: normalize_outputs(&ann),
            outputs_spec,
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

async fn unique_instance_name(requested: &str) -> Option<String> {
    let stem = instance_stem(requested)?;
    for _ in 0..8 {
        let candidate = format!("{stem}-{}", instance_suffix());
        if !namespace_exists(&format!("yolab-{candidate}")).await {
            return Some(candidate);
        }
    }
    None
}

async fn namespace_exists(ns: &str) -> bool {
    crate::kubectl::get_json(&["get", "namespace", ns, "-o", "json"])
        .await
        .is_ok()
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
    let source_definition = match install::source_definition(&sources.config).await {
        Ok(d) => d,
        Err(e) => return refuse(format!("{e}")),
    };
    let Some(instance_name) = unique_instance_name(&body.instance_name).await else {
        return refuse("could not derive a unique name for this app".into());
    };
    let plan = match install::plan(
        &id,
        &instance_name,
        body.config,
        source_definition.as_ref(),
        sources.data,
        &chart_uischema(&state.config.catalog_dir(), &id),
    ) {
        Ok(p) => p,
        Err(e) => return refuse(e),
    };

    Sse::new(install::install_stream(state.config.clone(), plan)).into_response()
}

pub(crate) async fn rollback_failed_install(ns: &str, instance_name: &str) {
    crate::exec::checked(
        "helm",
        &["uninstall", instance_name, "-n", ns],
        std::time::Duration::from_secs(120),
    )
    .await
    .debug_on_err(format!("rollback {ns}: helm uninstall"));
    crate::kubectl::run(&["delete", "namespace", ns, "--wait=false"])
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
    cfg: &Config,
    id: &str,
    instance_name: &str,
    config: &serde_json::Map<String, Value>,
    prefer_repo: Option<&str>,
) -> anyhow::Result<StagedInstall> {
    let tunnel_cfg =
        tunnel_config(cfg).map_err(|_| anyhow::anyhow!("could not read tunnel config"))?;
    let Some((repo, chart_dir)) = crate::charts::resolve_chart(id, prefer_repo).await else {
        anyhow::bail!("no chart named {id} in any configured repository");
    };
    let Some(meta) = read_chart(&chart_dir) else {
        anyhow::bail!("{id} is not a valid chart");
    };
    let ns = format!("yolab-{instance_name}");
    ensure_app_namespace(&ns, id, &repo, &meta.chart.version)
        .await
        .map_err(|e| anyhow::anyhow!("create namespace: {e}"))?;
    ensure_tunnel_credentials(&ns, &tunnel_cfg)
        .await
        .map_err(|e| anyhow::anyhow!("stage tunnel credentials: {e}"))?;
    let service_name = resolve_service_name(&meta.schema, config);
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
    let ns_v = match crate::kubectl::get_opt(&["get", "namespace", &ns, "-o", "json"]).await {
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
    let uischema = chart_uischema(&state.config.catalog_dir(), &id);
    let stored_config = match read_config(&ns).await {
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
        Some(incoming) => merge_credentials(incoming, &stored_config, &uischema),
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
        backup: read_definition_opt(&ns)
            .await
            .map(|d| d.backup)
            .unwrap_or_default(),
    };
    Sse::new(install::upgrade_stream(state.config.clone(), plan)).into_response()
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
    let mut def = read_definition(&ns).await?;
    def.backup = BackupPolicy {
        enabled: body.enabled,
        schedule: body.schedule,
    };
    let uischema = chart_uischema(&state.config.catalog_dir(), &def.app_id);
    write_definition(&ns, &def, &uischema).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn app_definition(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<AppDefinition>> {
    let ns = format!("yolab-{instance_name}");
    let def = read_definition(&ns).await?;
    Ok(Json(redact_definition(&def, &state.config.catalog_dir())))
}

pub async fn scan_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<ScanOutputsResponse>> {
    let ns = format!("yolab-{instance_name}");
    let ns_v = crate::kubectl::get_json(&["get", "namespace", &ns, "-o", "json"]).await?;
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let id = ann
        .get(ANN_APP_ID)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let outputs_spec = chart_outputs_spec(&state.config.catalog_dir(), &id);
    if outputs_spec.is_empty() {
        return Ok(Json(ScanOutputsResponse {
            outputs: normalize_outputs(&ann),
        }));
    }

    struct CompiledSpec {
        key: String,
        label: String,
        type_: String,
        re: Option<regex::Regex>,
    }
    let compiled: Vec<CompiledSpec> = outputs_spec
        .iter()
        .filter_map(|spec| {
            let key = spec["key"].as_str()?.to_string();
            Some(CompiledSpec {
                re: spec["pattern"]
                    .as_str()
                    .and_then(|p| regex::Regex::new(p).ok()),
                label: spec
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&key)
                    .to_string(),
                type_: spec
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("text")
                    .to_string(),
                key,
            })
        })
        .collect();

    let pods_v = crate::kubectl::get_json(&["get", "pods", "-n", &ns, "-o", "json"]).await?;
    let mut found: std::collections::HashMap<String, String> = Default::default();

    'outer: for pod in pods_v["items"].as_array().unwrap_or(&vec![]) {
        let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
        let empty = vec![];
        let init_containers = pod["spec"]["initContainers"].as_array().unwrap_or(&empty);
        let main_containers = pod["spec"]["containers"].as_array().unwrap_or(&empty);
        let containers: Vec<&str> = init_containers
            .iter()
            .chain(main_containers.iter())
            .filter_map(|c| c["name"].as_str())
            .collect();
        for container in containers {
            let logs = crate::kubectl::run(&[
                "logs",
                "-n",
                &ns,
                pod_name,
                "-c",
                container,
                &format!("--tail={LOGS_SCAN_TAIL}"),
            ])
            .await;
            let Ok(text) = logs else { continue };
            for line in text.lines() {
                for cs in &compiled {
                    if found.contains_key(&cs.key) {
                        continue;
                    }
                    if let Some(re) = &cs.re {
                        if let Some(cap) = re.captures(line).and_then(|c| c.get(1)) {
                            found.insert(cs.key.clone(), cap.as_str().to_string());
                        }
                    }
                }
            }
            if found.len() == compiled.len() {
                break 'outer;
            }
        }
    }

    if found.is_empty() {
        return Ok(Json(ScanOutputsResponse {
            outputs: normalize_outputs(&ann),
        }));
    }

    let outputs: Vec<AppOutput> = compiled
        .iter()
        .filter_map(|cs| {
            let value = found.get(&cs.key)?.clone();
            Some(AppOutput {
                key: cs.key.clone(),
                label: cs.label.clone(),
                value,
                type_: cs.type_.clone(),
            })
        })
        .collect();

    let outputs_json = serde_json::to_string(&outputs).unwrap_or_default();
    annotate_ns(&ns, ANN_OUTPUTS, &outputs_json).await;

    Ok(Json(ScanOutputsResponse { outputs }))
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

async fn claim_uninstall_lock(ns: &str) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let first_claim = crate::kubectl::run(&[
        "annotate",
        "namespace",
        ns,
        &format!("{ANN_UNINSTALLING}={now}"),
        "--overwrite=false",
    ])
    .await;
    match first_claim {
        Ok(_) => return Ok(true),
        Err(e) if crate::kubectl::is_not_found(&e) => return Ok(true),
        Err(_) => {}
    }

    let raw =
        crate::kubectl::run(&["get", "namespace", ns, "-o", "json", "--ignore-not-found"]).await?;
    if raw.trim().is_empty() {
        return Ok(true);
    }
    let existing: Value = serde_json::from_str(&raw)?;
    let ann = existing["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if uninstall_lock_is_fresh(&ann) {
        return Ok(false);
    }
    tracing::warn!("uninstall {ns}: reclaiming a stale uninstall lock");
    crate::kubectl::run(&[
        "annotate",
        "namespace",
        ns,
        &format!("{ANN_UNINSTALLING}={now}"),
        "--overwrite=true",
    ])
    .await?;
    Ok(true)
}

async fn delete_namespace_with_retry(ns: &str) {
    const ATTEMPTS: u32 = 4;
    let mut delay = std::time::Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        match crate::kubectl::run(&[
            "delete",
            "namespace",
            ns,
            "--ignore-not-found=true",
            "--wait=false",
        ])
        .await
        {
            Ok(_) => return,
            Err(e) if attempt == ATTEMPTS => {
                tracing::warn!(
                    "uninstall {ns}: delete namespace failed after {ATTEMPTS} attempts, \
                     leaving it for a future retry: {e}"
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
    State(_state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let ns = format!("yolab-{instance_name}");

    if !claim_uninstall_lock(&ns).await? {
        return Err(anyhow::anyhow!("uninstall for {instance_name} is already in progress").into());
    }

    let ns_owned = ns.clone();
    let instance_owned = instance_name.clone();
    let task = tokio::spawn(async move {
        run_teardown(&instance_owned, &ns_owned).await;
    });
    if let Err(e) = task.await {
        tracing::error!("uninstall {instance_name}: teardown task failed: {e}");
        return Err(anyhow::anyhow!("the uninstall did not finish: {e}").into());
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn namespace_is_terminating(ns: &str) -> bool {
    crate::kubectl::get_json(&["get", "namespace", ns, "-o", "json"])
        .await
        .map(|v| v["status"]["phase"] == "Terminating")
        .unwrap_or(false)
}

async fn run_teardown(instance_name: &str, ns: &str) {
    if namespace_is_terminating(ns).await {
        tracing::info!(
            "uninstall {instance_name}: namespace is already terminating — waiting for it \
             to finish rather than re-running helm"
        );
        delete_namespace_with_retry(ns).await;
        return;
    }

    const HELM_UNINSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let work = tokio::process::Command::new("helm")
        .args([
            "uninstall",
            instance_name,
            "-n",
            ns,
            "--ignore-not-found",
            "--wait",
        ])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(HELM_UNINSTALL_TIMEOUT, work).await;
    match out {
        Ok(Ok(o)) if !o.status.success() => {
            tracing::warn!(
                "uninstall {instance_name}: helm uninstall failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Ok(Err(e)) => tracing::warn!("uninstall {instance_name}: could not run helm: {e}"),
        Err(_) => tracing::warn!(
            "uninstall {instance_name}: helm uninstall timed out after {}s — deleting the namespace anyway",
            HELM_UNINSTALL_TIMEOUT.as_secs()
        ),
        _ => {}
    }

    delete_namespace_with_retry(ns).await;
}

fn abandoned_in(v: &Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .unwrap_or(&vec![])
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

async fn abandoned_uninstalls() -> anyhow::Result<Vec<(String, String)>> {
    let v = crate::kubectl::get_json(&[
        "get",
        "namespaces",
        "-l",
        "yolab.io/managed=true",
        "-o",
        "json",
    ])
    .await?;
    Ok(abandoned_in(&v))
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
        let abandoned = abandoned_uninstalls().await?;
        if abandoned.is_empty() {
            return Ok(crate::runtime::Tick::Idle("no abandoned uninstalls".into()));
        }
        for (ns, instance) in abandoned {
            tracing::warn!(
                "uninstall {instance}: claim is stale and nothing is driving it — \
                 finishing the teardown"
            );
            run_teardown(&instance, &ns).await;
        }
        Ok(crate::runtime::Tick::Done)
    }
}

pub async fn list_pods(Path(instance_name): Path<String>) -> Result<Json<Vec<PodInfo>>> {
    let v = crate::kubectl::get_json(&[
        "get",
        "pods",
        "-n",
        &format!("yolab-{instance_name}"),
        "-o",
        "json",
    ])
    .await?;
    Ok(Json(
        v["items"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
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
    Path((instance_name, pod_name)): Path<(String, String)>,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>> {
    let ns = format!("yolab-{instance_name}");
    let tail = format!("--tail={LOGS_FOLLOW_TAIL}");
    let stream = async_stream::stream! {
        let child = tokio::process::Command::new("kubectl")
            .args(["logs", "-n", &ns, &pod_name,
                   "--all-containers=true", "--follow", "--prefix=true",
                   &tail, "--max-log-requests=20"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let Ok(c) = child else {
            yield Ok(Event::default().data("[yolab] could not run kubectl to read the logs"));
            return;
        };
        let mut guard = KillOnDrop(c);
        use tokio::io::AsyncBufReadExt;
        let stdout = guard.0.stdout.take().unwrap();
        let stderr = guard.0.stderr.take().unwrap();
        let mut out = tokio::io::BufReader::new(stdout).lines();
        let mut err = tokio::io::BufReader::new(stderr).lines();
        loop {
            tokio::select! {
                line = out.next_line() => match line {
                    Ok(Some(l)) => yield Ok(Event::default().data(l)),
                    _ => break,
                },
                line = err.next_line() => match line {
                    Ok(Some(l)) => yield Ok(Event::default().data(format!("[yolab] {l}"))),
                    _ => continue,
                },
            }
        }
        while let Ok(Some(l)) = err.next_line().await {
            yield Ok(Event::default().data(format!("[yolab] {l}")));
        }
        guard.0.wait().await.debug_on_err("reap kubectl logs");
    };
    Sse::new(stream)
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
    fn normalize_outputs_new_format() {
        let ann = map(json!({
            ANN_OUTPUTS: r#"[{"key":"url","label":"Web URL","value":"https://x","type":"url"}]"#
        }));
        let out = normalize_outputs(&ann);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "url");
        assert_eq!(out[0].value, "https://x");
    }

    #[test]
    fn normalize_outputs_empty() {
        assert!(normalize_outputs(&serde_json::Map::new()).is_empty());
    }

    #[test]
    fn outputs_in_any_other_shape_are_not_guessed_at() {
        let ann = map(json!({ ANN_OUTPUTS: r#"[{"url":"https://x"}]"# }));
        assert!(normalize_outputs(&ann).is_empty());
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

        let list = serde_json::json!({ "items": [
            { "metadata": { "name": "yolab-minecraft",
                            "annotations": { ANN_UNINSTALLING: stale } } },
            { "metadata": { "name": "yolab-filebrowser",
                            "annotations": { ANN_UNINSTALLING: fresh } } },
            { "metadata": { "name": "yolab-vaultwarden",
                            "annotations": { "yolab.io/app-id": "vaultwarden" } } },
            { "metadata": { "name": "yolab-babybuddy" } },
        ]});

        assert_eq!(
            abandoned_in(&list),
            vec![("yolab-minecraft".to_string(), "minecraft".to_string())],
            "only the stale claim, and the instance name comes off the prefix"
        );
    }

    #[test]
    fn the_watchdog_ignores_namespaces_outside_the_yolab_prefix() {
        let stale = (chrono::Utc::now()
            - chrono::Duration::seconds(UNINSTALL_LOCK_TTL.as_secs() as i64 + 1))
        .to_rfc3339();
        let list = serde_json::json!({ "items": [
            { "metadata": { "name": "kube-system",
                            "annotations": { ANN_UNINSTALLING: stale } } },
        ]});
        assert!(abandoned_in(&list).is_empty());
    }

    #[test]
    fn an_empty_or_missing_list_is_handled() {
        assert!(abandoned_in(&serde_json::json!({})).is_empty());
        assert!(abandoned_in(&serde_json::json!({ "items": [] })).is_empty());
    }

    #[test]
    fn an_unparsable_lock_timestamp_is_not_fresh() {
        assert!(!uninstall_lock_is_fresh(&map(
            serde_json::json!({ ANN_UNINSTALLING: "not-a-timestamp" })
        )));
    }

    fn chart_dir_with(outputs: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let chart = dir.path().join("filebrowser");
        std::fs::create_dir_all(&chart).unwrap();
        std::fs::write(
            chart.join("Chart.yaml"),
            format!(
                "apiVersion: v2\nname: filebrowser\nversion: 0.1.0\nannotations:\n  \
                 {ANN_CHART_OUTPUTS}: |\n    {outputs}\n"
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn the_outputs_annotation_is_read_off_the_chart() {
        let dir = chart_dir_with(
            r#"[{"key":"file_explorer_url","label":"File explorer","type":"url"},
             {"key":"file_explorer_password","label":"Password","type":"text"}]"#,
        );
        let specs = chart_outputs_spec(dir.path(), "filebrowser");
        let keys: Vec<&str> = specs.iter().filter_map(|s| s["key"].as_str()).collect();
        assert!(keys.contains(&"file_explorer_url"));
        assert!(keys.contains(&"file_explorer_password"));
    }

    #[test]
    fn a_chart_without_outputs_yields_none() {
        let dir = chart_dir_with("[]");
        assert!(chart_outputs_spec(dir.path(), "filebrowser").is_empty());
        assert!(chart_outputs_spec(dir.path(), "not-in-the-catalog").is_empty());
        assert!(chart_outputs_spec(dir.path(), "").is_empty());
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
}
