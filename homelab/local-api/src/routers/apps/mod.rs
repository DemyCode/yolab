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

mod definition;
mod uninstall;

pub(crate) use definition::*;
pub(crate) use uninstall::*;

const LABEL_MANAGED: &str = "yolab.io/managed";
pub(crate) const ANN_APP_ID: &str = "yolab.io/app-id";
pub(crate) const ANN_CHART_VERSION: &str = "yolab.io/chart-version";
pub(crate) const ANN_CHART_REPO: &str = "yolab.io/chart-repo";

const ANN_CONFIG: &str = "yolab.io/config";
const ANN_BACKUP: &str = "yolab.io/backup";
const ANN_UNINSTALLING: &str = "yolab.io/uninstalling";
const ANN_INSTALL_FAILED: &str = "yolab.io/install-failed";
const UNINSTALL_LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const LOGS_FOLLOW_TAIL: u32 = 100;

#[derive(Serialize)]
pub struct AppInfo {
    pub app_id: String,
    pub instance_name: String,
    pub instance_id: Option<String>,
    pub chart_version: String,
    pub status: String,
    pub detail: String,
    pub technical: String,
    pub since: Option<String>,
    pub retry_at: Option<String>,
    pub outputs: Vec<crate::outputs::ShownOutput>,
    pub config: serde_json::Map<String, Value>,
    pub backup: AppBackupStatus,
}

pub(crate) struct InstalledApp {
    pub namespace: String,
    pub app_id: String,
    pub settings: serde_json::Map<String, Value>,
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
    pub github: String,
    pub tagline: String,
    pub collections: Vec<String>,
    pub stars: Option<u64>,
    pub pushed_at: Option<String>,
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
                let empty = serde_json::Map::new();
                let req = c["resources"]["requests"].as_object().unwrap_or(&empty);
                let limits = c["resources"]["limits"].as_object().unwrap_or(&empty);
                if let Some(cpu) = req.get("cpu").and_then(|v| v.as_str()) {
                    resources.cpu_millicores += parse_cpu_millicores(cpu) * replicas;
                }
                if let Some(mem) = req.get("memory").and_then(|v| v.as_str()) {
                    resources.memory_bytes += crate::quantity::bytes(mem) * replicas;
                }
                let gpus = req
                    .keys()
                    .chain(limits.keys())
                    .filter(|k| is_gpu_resource(k))
                    .collect::<std::collections::BTreeSet<_>>();
                for k in gpus {
                    let n = req
                        .get(k)
                        .or_else(|| limits.get(k))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u64>().ok());
                    if let Some(n) = n {
                        resources.gpu += n * replicas;
                    }
                }
            }
        }
    }
    resources
}

fn is_gpu_resource(name: &str) -> bool {
    name.ends_with("/gpu")
        || name.starts_with("nvidia.com/gpu-")
        || matches!(name, "yolab.io/dri" | "yolab.io/kfd")
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

fn tunnel_config(cfg: &Config) -> anyhow::Result<toml::Table> {
    cfg.tunnel_table()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))
}

const ANN_DISPLAY_NAME: &str = "yolab.io/display-name";
const ANN_ICON: &str = "yolab.io/icon";
const ANN_CATEGORY: &str = "yolab.io/category";
const ANN_GITHUB: &str = "yolab.io/github";
const ANN_TAGLINE: &str = "yolab.io/tagline";
const ANN_COLLECTIONS: &str = "yolab.io/collections";
const ANN_DISABLED: &str = "yolab.io/disabled";

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
    fn in_store(&self) -> bool {
        self.ann(ANN_DISABLED).trim().is_empty()
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
    let app = crate::appschema::AppSchema::new(schema);
    Some(ChartMeta { chart, app })
}

pub(crate) fn app_schema(catalog_dir: &std::path::Path, id: &str) -> crate::appschema::AppSchema {
    let found = (!id.is_empty())
        .then(|| read_chart(&catalog_dir.join(id)))
        .flatten();
    match found {
        Some(meta) => meta.app,
        None => crate::appschema::AppSchema::new(Value::Null),
    }
}

pub(crate) async fn installed_schema(
    client: &Client,
    ns: &str,
    app_id: &str,
    fallback_catalog: &std::path::Path,
) -> crate::appschema::AppSchema {
    match crate::saved_chart::read_schema(client, ns).await {
        Ok(Some(schema)) => crate::appschema::AppSchema::new(schema),
        _ => app_schema(fallback_catalog, app_id),
    }
}

pub(crate) async fn chart_schema(
    client: &Client,
    app_id: &str,
) -> Option<crate::appschema::AppSchema> {
    let (_, dir) = crate::charts::resolve_chart(client, app_id, None).await?;
    read_chart(&dir).map(|meta| meta.app)
}

fn resolve_service_name(schema: &Value, config: &serde_json::Map<String, Value>) -> String {
    fn tunnel_field(props: Option<&serde_json::Map<String, Value>>) -> Option<(String, Value)> {
        props?.iter().find_map(|(k, v)| {
            (v["format"].as_str() == Some("tunnel")).then(|| (k.clone(), v.clone()))
        })
    }

    let nested = schema["properties"]["config"]["properties"].as_object();
    let top = schema["properties"].as_object();
    let behind_switch = [
        &schema["properties"]["config"]["dependencies"][YOLAB_SWITCH]["oneOf"],
        &schema["dependencies"][YOLAB_SWITCH]["oneOf"],
    ]
    .into_iter()
    .filter_map(Value::as_array)
    .flatten()
    .find_map(|branch| tunnel_field(branch["properties"].as_object()));

    let Some((field, spec)) = tunnel_field(nested)
        .or(behind_switch)
        .or_else(|| tunnel_field(top))
    else {
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

pub(crate) const YOLAB_SWITCH: &str = "yolab_enabled";
pub(crate) const YOLAB_TOKEN_FIELD: &str = "yolab_token";
const EXPLORER_YOLAB_SWITCH: &str = "file_explorer_yolab_enabled";
const TUNNEL_SECRET: &str = "yolab-tunnel-credentials";
const LABEL_TOKEN_SOURCE: &str = "yolab.io/token-source";
const TOKEN_FROM_FORM: &str = "form";

pub(crate) fn take_yolab_token(config: &mut serde_json::Map<String, Value>) -> Option<String> {
    let token = config.remove(YOLAB_TOKEN_FIELD)?;
    let token = token.as_str()?.trim();
    (!token.is_empty() && token != REDACTED).then(|| token.to_string())
}

pub(crate) fn way_in_refused(
    config_schema: &Value,
    config: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    if config_schema["properties"][YOLAB_SWITCH].is_null() {
        return None;
    }
    let on = |key: &str| config.get(key) == Some(&Value::Bool(true));
    let yolab = config.get(YOLAB_SWITCH) != Some(&Value::Bool(false));
    if !(yolab || on("tor_enabled") || on("tailscale_enabled")) {
        return Some(
            "Turn on at least one way to reach this app: the YoLab address, Tor or Tailscale.",
        );
    }
    let explorer_yolab = match config.get(EXPLORER_YOLAB_SWITCH) {
        Some(v) => v == &Value::Bool(true),
        None => yolab,
    };
    let explorer_reachable =
        explorer_yolab || on("file_explorer_tor_enabled") || on("file_explorer_tailscale_enabled");
    (on("file_explorer_enabled") && !explorer_reachable).then_some(
        "Turn on at least one way to reach the file explorer: its YoLab address, Tor or Tailscale. Or switch the file explorer off.",
    )
}

fn box_token(tunnel_cfg: &toml::Table) -> &str {
    tunnel_cfg
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn tunnel_secret(ns: &str, token: &str, source: &str) -> Value {
    crate::k8s::secret_manifest(
        TUNNEL_SECRET,
        ns,
        &[("account-token", token)],
        &[
            ("app.kubernetes.io/managed-by", "yolab"),
            (LABEL_TOKEN_SOURCE, source),
        ],
    )
}

pub(crate) async fn store_form_token(client: &Client, ns: &str, token: &str) -> anyhow::Result<()> {
    crate::k8s::apply(client, &tunnel_secret(ns, token, TOKEN_FROM_FORM)).await
}

fn keeps_form_token(existing: Option<&Value>) -> bool {
    existing.is_some_and(|s| {
        s["metadata"]["labels"][LABEL_TOKEN_SOURCE].as_str() == Some(TOKEN_FROM_FORM)
    })
}

async fn ensure_tunnel_credentials(
    client: &Client,
    ns: &str,
    tunnel_cfg: &toml::Table,
) -> anyhow::Result<()> {
    let existing = crate::k8s::get(
        client,
        &crate::k8s::reference("v1", "Secret", ns, TUNNEL_SECRET),
    )
    .await?;
    if keeps_form_token(existing.as_ref()) {
        return Ok(());
    }
    crate::k8s::apply(client, &tunnel_secret(ns, box_token(tunnel_cfg), "box")).await
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

fn catalog_entry_from(
    repo: String,
    meta: ChartMeta,
    stars: &std::collections::HashMap<String, crate::github::RepoStats>,
) -> CatalogApp {
    let github = meta.ann(ANN_GITHUB).to_string();
    let known = stars.get(&github).filter(|s| !s.archived || s.stars > 0);
    CatalogApp {
        id: meta.chart.name.clone(),
        repo,
        name: meta.display_name(),
        description: meta.chart.description.clone(),
        home: meta.chart.home.clone(),
        icon: meta.ann(ANN_ICON).to_string(),
        category: meta.ann(ANN_CATEGORY).to_string(),
        tagline: meta.ann(ANN_TAGLINE).to_string(),
        collections: meta
            .ann(ANN_COLLECTIONS)
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
        stars: known.map(|s| s.stars),
        pushed_at: known.and_then(|s| s.pushed_at.clone()),
        github,
        chart_version: meta.chart.version.clone(),
        schema: meta.app.config(),
    }
}

pub async fn refresh_catalog_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let (refreshed, note) = match crate::charts::fetch_newest(
        &b.host,
        std::path::Path::new(crate::charts::CACHE_DIR),
        &crate::charts::list_repos(&b.kube).await,
        &id,
        None,
    )
    .await
    {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };

    let stars = crate::github::read_all(&b.kube).await.unwrap_or_default();
    let entry = crate::charts::chart_sources(&b.kube)
        .await
        .into_iter()
        .find_map(|(repo, dir)| {
            let m = read_chart(&dir.join(&id))?;
            Some(catalog_entry_from(repo, m, &stars))
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

    let b = state.backend().await?;
    let client = b.kube;
    for repo in crate::charts::list_repos(&client).await {
        if let Err(e) = crate::charts::sync_repo(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repo,
        )
        .await
        {
            tracing::warn!("catalog: {} not refreshed: {e:#}", repo.name);
        }
    }
    let stars = crate::github::read_all(&client).await.unwrap_or_default();
    for (repo, dir) in crate::charts::chart_sources(&client).await {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Some(meta) = read_chart(&entry.path()) else {
                continue;
            };
            if !seen.insert(meta.chart.name.clone()) || !meta.in_store() {
                continue;
            }
            apps.push(catalog_entry_from(repo.clone(), meta, &stars));
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

pub(crate) fn is_finished_pod(pod: &Value) -> bool {
    matches!(
        pod["status"]["phase"].as_str(),
        Some("Succeeded") | Some("Failed")
    )
}

pub(crate) fn explain_app_state(pods: &[&Value]) -> String {
    if pods.is_empty() {
        return "Installing…".into();
    }

    let mut waiting: Vec<String> = Vec::new();
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

        for key in ["initContainerStatuses", "containerStatuses"] {
            for cs in pod["status"][key].as_array().into_iter().flatten() {
                if let Some(reason) = cs["state"]["waiting"]["reason"].as_str() {
                    waiting.push(reason.to_string());
                }
                if cs["state"]["running"].is_object() && cs["ready"] == false {
                    running_not_ready = true;
                }
            }
        }
    }

    let has = |r: &str| waiting.iter().any(|reason| reason == r);

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
        return "Getting ready…".into();
    }

    String::new()
}

const EXPLAINED_WAITS: [&str; 2] = ["ContainerCreating", "PodInitializing"];

pub(crate) fn unexplained_waits(pods: &[&Value]) -> String {
    let mut reasons: Vec<&str> = pods
        .iter()
        .flat_map(|pod| {
            ["initContainerStatuses", "containerStatuses"]
                .into_iter()
                .flat_map(move |key| pod["status"][key].as_array().into_iter().flatten())
        })
        .filter_map(|cs| cs["state"]["waiting"]["reason"].as_str())
        .filter(|reason| !EXPLAINED_WAITS.contains(reason))
        .collect();
    reasons.dedup();
    reasons.join(", ")
}

pub(crate) fn waiting_since(pods: &[&Value]) -> Option<String> {
    pods.iter()
        .filter_map(|p| p["metadata"]["creationTimestamp"].as_str())
        .min()
        .map(str::to_string)
}

pub(crate) fn install_failure_headline(reason: &str) -> String {
    let said = reason.to_lowercase();
    if said.contains("timed out") || said.contains("deadline exceeded") {
        "It took too long to start, so the installation was stopped.".into()
    } else if said.contains("no space") || said.contains("insufficient") {
        "There is not enough room on the server for it.".into()
    } else {
        "Something went wrong while setting it up.".into()
    }
}

fn go_duration(text: &str) -> Option<chrono::Duration> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0f64;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total += number * seconds;
        rest = &rest[unit..];
    }
    Some(chrono::Duration::milliseconds(
        (total * 1000.0).round() as i64
    ))
}

fn next_restart(waiting: &Value, last: &Value) -> Option<String> {
    let delay = waiting["message"]
        .as_str()?
        .strip_prefix("back-off ")?
        .split_whitespace()
        .next()
        .and_then(go_duration)?;
    let ended = chrono::DateTime::parse_from_rfc3339(last["finishedAt"].as_str()?).ok()?;
    Some(
        (ended.with_timezone(&chrono::Utc) + delay)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

const CANNOT_START: [&str; 7] = [
    "ImagePullBackOff",
    "ErrImagePull",
    "ErrImageNeverPull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
    "InvalidImageName",
];

const FAILURE_LOG_LINES: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerFailure {
    pub pod: String,
    pub container: String,
    pub said: String,
    pub exit_code: Option<i64>,
    pub previous: bool,
    pub reason: String,
    pub retry_at: Option<String>,
}

impl ContainerFailure {
    pub(crate) fn headline(&self) -> String {
        let said = self.said.to_lowercase();
        let mentions = |words: &[&str]| words.iter().any(|w| said.contains(w));
        match self.reason.as_str() {
            "ImagePullBackOff" | "ErrImagePull" | "ErrImageNeverPull" => {
                if mentions(&["not found", "manifest unknown", "does not exist"]) {
                    "The version of its program it asks for could not be found online.".into()
                } else if mentions(&["unauthorized", "denied", "forbidden"]) {
                    "Its program could not be downloaded: the place it comes from refused access."
                        .into()
                } else {
                    "Its program could not be downloaded. Check that the server is connected to the internet.".into()
                }
            }
            "InvalidImageName" => "It points to a program that does not exist.".into(),
            "CreateContainerConfigError" => {
                "A setting or password it needs to start is missing.".into()
            }
            "CreateContainerError" | "RunContainerError" => {
                "Its program could not be started.".into()
            }
            "OOMKilled" => "It ran out of memory and was stopped.".into(),
            _ => "It starts, then stops right away.".into(),
        }
    }

    pub(crate) fn describe(&self, said: &str) -> String {
        match (said.trim(), self.exit_code) {
            ("", Some(code)) => format!(
                "{} stopped with exit code {code} and left no message",
                self.container
            ),
            ("", None) => format!("{} stopped and left no message", self.container),
            (said, _) => format!("{}: {said}", self.container),
        }
    }
}

fn failure_of(pod: &str, cs: &Value) -> Option<ContainerFailure> {
    let text = |v: &Value| v["message"].as_str().unwrap_or("").trim().to_string();
    let failure = |said: String, ended: &Value, previous: bool, reason: &str| ContainerFailure {
        pod: pod.to_string(),
        container: cs["name"].as_str().unwrap_or("").to_string(),
        said,
        exit_code: ended["exitCode"].as_i64(),
        previous,
        reason: reason.to_string(),
        retry_at: None,
    };
    let waiting = &cs["state"]["waiting"];
    let reason = waiting["reason"].as_str().unwrap_or("");
    if reason == "CrashLoopBackOff" {
        let last = &cs["lastState"]["terminated"];
        let why = match last["reason"].as_str() {
            Some("OOMKilled") => "OOMKilled",
            _ => reason,
        };
        return Some(ContainerFailure {
            retry_at: next_restart(waiting, last),
            ..failure(text(last), last, true, why)
        });
    }
    if CANNOT_START.contains(&reason) {
        let said = text(waiting);
        let said = if said.is_empty() {
            reason.to_string()
        } else {
            said
        };
        return Some(failure(said, &Value::Null, false, reason));
    }
    let ended = &cs["state"]["terminated"];
    if ended["exitCode"].as_i64().is_some_and(|code| code != 0) {
        let why = ended["reason"].as_str().unwrap_or("");
        return Some(failure(text(ended), ended, false, why));
    }
    None
}

pub(crate) fn container_failure(pods: &[&Value]) -> Option<ContainerFailure> {
    pods.iter().find_map(|pod| {
        let name = pod["metadata"]["name"].as_str().unwrap_or("");
        ["initContainerStatuses", "containerStatuses"]
            .iter()
            .flat_map(|key| pod["status"][key].as_array().into_iter().flatten())
            .find_map(|cs| failure_of(name, cs))
    })
}

pub(crate) fn has_come_up(deployments: &[&Value]) -> bool {
    !deployments.is_empty()
        && deployments.iter().all(|d| {
            d["status"]["conditions"].as_array().is_some_and(|cs| {
                cs.iter()
                    .any(|c| c["type"] == "Progressing" && c["reason"] == "NewReplicaSetAvailable")
            })
        })
}

fn last_lines(log: &str) -> String {
    log.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn explain_failure(client: &Client, ns: &str, failure: &ContainerFailure) -> String {
    if !failure.said.is_empty() {
        return failure.describe(&failure.said);
    }
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client.clone(), ns);
    let params = kube::api::LogParams {
        container: Some(failure.container.clone()),
        previous: failure.previous,
        tail_lines: Some(FAILURE_LOG_LINES),
        ..Default::default()
    };
    let said = match pods.logs(&failure.pod, &params).await {
        Ok(log) => last_lines(&log),
        Err(e) => {
            tracing::debug!(
                "{ns}: could not read why {} stopped: {e}",
                failure.container
            );
            String::new()
        }
    };
    failure.describe(&said)
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
) -> Option<(String, Option<String>)> {
    let pvc = pvcs_by_ns
        .get(namespace)?
        .iter()
        .find(|pvc| is_cloning(pvc))?;
    let name = pvc["metadata"]["name"].as_str().unwrap_or("");
    let said = match clone_percent(events, namespace, name) {
        Some(percent) => format!("Copying this app's files… {percent}%"),
        None => "Copying this app's files…".to_string(),
    };
    let started = pvc["metadata"]["creationTimestamp"]
        .as_str()
        .map(str::to_string);
    Some((said, started))
}

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let client = &state.kube.client().await?;
    let catalog_dir = state.config.catalog_dir();
    let backup_status = crate::routers::backup::app_backup_status(client).await;
    let schemas = crate::saved_chart::all_schemas(client)
        .await
        .unwrap_or_default();
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let everything = kube::api::ListParams::default();
    let (ns_out, pods_out, deployments_out, pvcs_out, events_out, mut remembered) = tokio::join!(
        crate::k8s::list(client, "v1", "Namespace", None, &managed),
        crate::k8s::list(client, "v1", "Pod", None, &everything),
        crate::k8s::list(client, "apps/v1", "Deployment", None, &everything),
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
    let all_deployment_items = deployments_out.unwrap_or_default();
    let mut deployments_by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
    for deployment in &all_deployment_items {
        if let Some(ns) = deployment["metadata"]["namespace"].as_str() {
            deployments_by_ns.entry(ns).or_default().push(deployment);
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
        let mut technical = String::new();
        let mut since: Option<String> = None;
        let mut retry_at: Option<String> = None;
        let status = if phase == "Terminating" || uninstall_lock_is_fresh(&ann) {
            since = ns["metadata"]["deletionTimestamp"]
                .as_str()
                .map(str::to_string);
            "uninstalling".to_string()
        } else if let Some(reason) = install_failure(&ann) {
            detail = install_failure_headline(&reason);
            technical = reason;
            "failed".to_string()
        } else if let Some((copying, started)) =
            copying_data(&ns_full, &pvcs_by_ns, &all_event_items)
        {
            detail = copying;
            since = started;
            "copying".to_string()
        } else {
            let items: Vec<&Value> = pods_by_ns
                .get(ns_full.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|p| {
                    !is_backup_mover_pod(p) && !is_terminating_pod(p) && !is_finished_pod(p)
                })
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
            if all_ready {
                "running".to_string()
            } else if let Some(failure) = container_failure(&items) {
                detail = failure.headline();
                technical = explain_failure(client, &ns_full, &failure).await;
                retry_at = failure.retry_at.clone();
                let deployments = deployments_by_ns
                    .get(ns_full.as_str())
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                if has_come_up(deployments) {
                    "stopped"
                } else {
                    "failed"
                }
                .to_string()
            } else {
                detail = explain_app_state(&items);
                technical = unexplained_waits(&items);
                since = waiting_since(&items).or_else(|| {
                    ns["metadata"]["creationTimestamp"]
                        .as_str()
                        .map(str::to_string)
                });
                "starting".to_string()
            }
        };

        let id = ann
            .get(ANN_APP_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let config = saved_settings(&ann);
        let schema = schemas
            .get(&format!("yolab-{name}"))
            .cloned()
            .map(crate::appschema::AppSchema::new)
            .unwrap_or_else(|| app_schema(&catalog_dir, &id));
        let outputs = listed_outputs(
            &schema,
            remembered
                .remove(&format!("yolab-{name}"))
                .unwrap_or_default(),
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
            chart_version: ann
                .get(ANN_CHART_VERSION)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            instance_name: name,
            status,
            detail,
            technical,
            since,
            retry_at,
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
    let mut body = body;
    let form_token = take_yolab_token(&mut body.config);
    if let Err(e) = validate_config_values(&body.config) {
        return refuse(format!("invalid config: {e}"));
    }
    let sources = match install::resolve_sources(body.source.as_ref()) {
        Ok(s) => s,
        Err(e) => return refuse(e),
    };
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let source = match install::source(&b, &sources.config).await {
        Ok(s) => s,
        Err(e) => return refuse(format!("{e}")),
    };
    let saved = source.as_ref().and_then(|s| s.schema.clone());
    let app = match saved {
        Some(schema) => crate::appschema::AppSchema::new(schema),
        None => match chart_schema(&b.kube, &id).await {
            Some(app) => app,
            None if source.is_some() => crate::appschema::AppSchema::new(Value::Null),
            None => {
                return (StatusCode::NOT_FOUND, format!("App '{id}' not found")).into_response()
            }
        },
    };
    if let Some(why) = way_in_refused(&app.config(), &body.config) {
        return refuse(why.into());
    }
    let Some(instance_name) = unique_instance_name(&b.kube, &body.instance_name).await else {
        return refuse("could not derive a unique name for this app".into());
    };
    let plan = match install::plan(
        &id,
        &instance_name,
        body.config,
        source.as_ref(),
        sources.data,
        &app,
    ) {
        Ok(p) => p,
        Err(e) => return refuse(e),
    };

    let opened = match plan.chart.recorded() {
        None => open_app_namespace(
            &b.kube,
            &id,
            &instance_name,
            &ChartAt::Catalog { repo: None },
        )
        .await
        .map(|_| ()),
        Some((repo, version)) => {
            ensure_app_namespace(
                &b.kube,
                &format!("yolab-{instance_name}"),
                &id,
                repo,
                version,
            )
            .await
        }
    };
    if let Err(e) = opened {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
    }
    if let Some(token) = form_token.as_deref() {
        if let Err(e) = store_form_token(&b.kube, &format!("yolab-{instance_name}"), token).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
    }
    install::start(b, state.config.clone(), plan, state.http.clone());
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "instance_name": instance_name })),
    )
        .into_response()
}

pub(crate) fn install_failure(ann: &serde_json::Map<String, Value>) -> Option<String> {
    ann.get(ANN_INSTALL_FAILED)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

async fn set_install_failed(client: &Client, ns: &str, value: Value) -> anyhow::Result<()> {
    let mut patch = namespace_ref(ns);
    patch["metadata"]["annotations"] = serde_json::json!({ ANN_INSTALL_FAILED: value });
    match crate::k8s::merge_patch(client, &patch).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(()),
        other => other,
    }
}

pub(crate) async fn mark_install_failed(client: &Client, ns: &str, reason: &str) {
    tracing::warn!("{ns}: install failed, kept for inspection: {reason}");
    set_install_failed(client, ns, Value::String(reason.to_string()))
        .await
        .warn_on_err(format!("{ns}: could not mark the install as failed"));
}

pub(crate) async fn clear_install_failed(client: &Client, ns: &str) {
    set_install_failed(client, ns, Value::Null)
        .await
        .warn_on_err(format!("{ns}: could not clear the failed-install mark"));
}

pub(crate) struct StagedInstall {
    pub(crate) ns: String,
    pub(crate) chart_repo: String,
    pub(crate) chart_version: String,
    pub(crate) service_name: String,
    pub(crate) chart_dir: std::path::PathBuf,
    pub(crate) values: tempfile::NamedTempFile,
}

pub(crate) enum ChartAt<'a> {
    Catalog {
        repo: Option<&'a str>,
    },
    Dir {
        repo: &'a str,
        dir: &'a std::path::Path,
    },
}

async fn open_app_namespace(
    client: &Client,
    id: &str,
    instance_name: &str,
    chart: &ChartAt<'_>,
) -> anyhow::Result<(String, String, std::path::PathBuf, ChartMeta)> {
    let (repo, chart_dir) = match chart {
        ChartAt::Catalog { repo } => crate::charts::resolve_chart(client, id, *repo)
            .await
            .ok_or_else(|| anyhow::anyhow!("no chart named {id} in any configured repository"))?,
        ChartAt::Dir { repo, dir } => (repo.to_string(), dir.to_path_buf()),
    };
    let Some(meta) = read_chart(&chart_dir) else {
        anyhow::bail!("{id} is not a valid chart");
    };
    let ns = format!("yolab-{instance_name}");
    ensure_app_namespace(client, &ns, id, &repo, &meta.chart.version)
        .await
        .map_err(|e| anyhow::anyhow!("create namespace: {e}"))?;
    Ok((ns, repo, chart_dir, meta))
}

pub(crate) async fn stage_install(
    client: &Client,
    cfg: &Config,
    id: &str,
    instance_name: &str,
    config: &serde_json::Map<String, Value>,
    chart: &ChartAt<'_>,
) -> anyhow::Result<StagedInstall> {
    let tunnel_cfg =
        tunnel_config(cfg).map_err(|_| anyhow::anyhow!("could not read tunnel config"))?;
    let (ns, repo, chart_dir, meta) = open_app_namespace(client, id, instance_name, chart).await?;
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
    #[serde(default)]
    pub keep_version: bool,
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
    let app = installed_schema(&client, &ns, &id, &state.config.catalog_dir()).await;
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

    let (incoming, keep_version) = match body {
        Some(Json(b)) => (b.config, b.keep_version),
        None => (None, false),
    };
    let mut config = match incoming {
        Some(incoming) => merge_credentials(incoming, &stored_config, &app),
        None => stored_config,
    };
    let form_token = take_yolab_token(&mut config);

    if let Err(e) = validate_config_values(&config) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response();
    }
    let repo = annotation(ANN_CHART_REPO);
    if id.is_empty()
        || crate::charts::resolve_chart(&client, &id, repo.as_deref())
            .await
            .is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            "this app's chart is in none of the chart repositories",
        )
            .into_response();
    }
    if let Some(why) = way_in_refused(&app.config(), &config) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if let Some(token) = form_token.as_deref() {
        if let Err(e) = store_form_token(&client, &ns, token).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
    }

    let stored = read_definition_opt(&client, &ns).await;
    let plan = install::UpgradePlan {
        app_id: id,
        release: stored
            .as_ref()
            .map(|d| d.release().to_string())
            .unwrap_or_else(|| instance_name.clone()),
        instance_name,
        config,
        chart_repo: annotation(ANN_CHART_REPO),
        backup: stored.map(|d| d.backup).unwrap_or_default(),
        keep_version,
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
    let app = installed_schema(&client, &ns, &def.app_id, &state.config.catalog_dir()).await;
    write_definition(&client, &ns, &def, &app).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn app_definition(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<AppDefinition>> {
    let ns = format!("yolab-{instance_name}");
    let client = state.kube.client().await?;
    let def = read_definition(&client, &ns).await?;
    let app = installed_schema(&client, &ns, &def.app_id, &state.config.catalog_dir()).await;
    Ok(Json(redact_definition(&def, &app)))
}

pub async fn app_settings_schema(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<Value>> {
    let ns = format!("yolab-{instance_name}");
    let client = state.kube.client().await?;
    let def = read_definition(&client, &ns).await?;
    let app = installed_schema(&client, &ns, &def.app_id, &state.config.catalog_dir()).await;
    Ok(Json(app.config()))
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
    remembered: crate::outputs::Remembered,
    settings: &serde_json::Map<String, Value>,
) -> Vec<crate::outputs::ShownOutput> {
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
            })
        })
        .collect())
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ServiceInstance {
    pub instance: String,
    pub app_id: String,
    pub title: String,
    pub url: String,
}

fn instances_of(
    kind: &str,
    apps: Vec<(String, String, crate::appschema::AppSchema)>,
) -> Vec<ServiceInstance> {
    let mut found: Vec<ServiceInstance> = apps
        .into_iter()
        .flat_map(|(namespace, app_id, schema)| {
            schema
                .provides()
                .into_iter()
                .filter(|p| p.kind == kind)
                .map(|p| ServiceInstance {
                    instance: namespace
                        .strip_prefix("yolab-")
                        .unwrap_or(&namespace)
                        .to_string(),
                    app_id: app_id.clone(),
                    title: p.title.clone(),
                    url: p.url(&namespace),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort_by(|a, b| a.instance.cmp(&b.instance));
    found
}

pub async fn list_services(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Result<Json<Vec<ServiceInstance>>> {
    let client = state.kube.client().await?;
    let catalog = state.config.catalog_dir();
    let mut apps = Vec::new();
    for app in installed_apps(&client).await? {
        let schema = installed_schema(&client, &app.namespace, &app.app_id, &catalog).await;
        apps.push((app.namespace, app.app_id, schema));
    }
    Ok(Json(instances_of(&kind, apps)))
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
    let app = installed_schema(client, ns, id, catalog_dir).await;
    let settings = saved_settings(&ann);

    let remembered = if rescan_first {
        crate::outputs::rescan(client, ns, &app, &settings).await?
    } else {
        crate::outputs::read_remembered(client, ns)
            .await?
            .unwrap_or_default()
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
    #[test]
    fn an_update_fetches_the_newest_chart_unless_asked_to_keep_the_current_one() {
        let plain: super::UpdateRequest =
            serde_json::from_value(serde_json::json!({ "config": {} })).unwrap();
        assert!(!plain.keep_version);
        let keep: super::UpdateRequest =
            serde_json::from_value(serde_json::json!({ "config": {}, "keep_version": true }))
                .unwrap();
        assert!(keep.keep_version);
    }

    #[test]
    fn installed_instances_of_a_service_kind_are_listed_with_their_internal_address() {
        use crate::appschema::AppSchema;
        use serde_json::json;
        let ollama = || {
            AppSchema::new(json!({
                "x-yolab-provides": { "ollama": { "title": "Ollama API", "service": "ollama", "port": 11434 } }
            }))
        };
        let apps = vec![
            ("yolab-gpu-box".to_string(), "ollama".to_string(), ollama()),
            (
                "yolab-notes".to_string(),
                "memos".to_string(),
                AppSchema::new(serde_json::Value::Null),
            ),
            ("yolab-ai".to_string(), "ollama".to_string(), ollama()),
        ];
        let found = super::instances_of("ollama", apps);
        assert_eq!(
            found,
            vec![
                super::ServiceInstance {
                    instance: "ai".into(),
                    app_id: "ollama".into(),
                    title: "Ollama API".into(),
                    url: "http://ollama.yolab-ai.svc.cluster.local:11434".into(),
                },
                super::ServiceInstance {
                    instance: "gpu-box".into(),
                    app_id: "ollama".into(),
                    title: "Ollama API".into(),
                    url: "http://ollama.yolab-gpu-box.svc.cluster.local:11434".into(),
                },
            ]
        );
        assert!(super::instances_of("electrum", vec![]).is_empty());
    }

    #[test]
    fn a_disabled_chart_is_left_out_of_the_store_but_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let chart = |ann: &str| {
            std::fs::write(
                dir.path().join("Chart.yaml"),
                format!("apiVersion: v2\nname: notes\nversion: 0.1.0\nannotations:\n{ann}"),
            )
            .unwrap();
            super::read_chart(dir.path()).expect("a disabled chart still reads")
        };
        assert!(chart("  yolab.io/tagline: Notes\n").in_store());
        assert!(!chart("  yolab.io/disabled: upstream archived\n").in_store());
        assert!(chart("  yolab.io/disabled: \"\"\n").in_store());
    }

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

    #[test]
    fn a_pod_that_ran_to_completion_is_finished() {
        assert!(is_finished_pod(&json!({"status": {"phase": "Succeeded"}})));
        assert!(is_finished_pod(&json!({"status": {"phase": "Failed"}})));
    }

    #[test]
    fn a_pod_that_is_still_going_is_not_finished() {
        for phase in ["Pending", "Running", "Unknown"] {
            assert!(
                !is_finished_pod(&json!({"status": {"phase": phase}})),
                "{phase}"
            );
        }
        assert!(!is_finished_pod(&json!({})));
    }

    #[test]
    fn a_completed_rebase_job_pod_does_not_hold_the_app_at_starting() {
        let rebase = json!({
            "metadata": {"name": "yolab-rebase-2c0b3ada-c9bkc"},
            "status": {"phase": "Succeeded", "conditions": [
                {"type": "Ready", "status": "False", "reason": "PodCompleted"}
            ]}
        });
        let app = json!({
            "metadata": {"name": "filebrowser-6558bdc759-9lxvk"},
            "status": {"phase": "Running", "conditions": [
                {"type": "Ready", "status": "True"}
            ]}
        });

        let counted: Vec<&Value> = [&rebase, &app]
            .into_iter()
            .filter(|p| !is_backup_mover_pod(p) && !is_terminating_pod(p) && !is_finished_pod(p))
            .collect();

        assert_eq!(counted.len(), 1, "only the app's own pod is evidence");
        assert_eq!(
            counted[0]["metadata"]["name"],
            "filebrowser-6558bdc759-9lxvk"
        );
    }

    fn waiting_pod(kind: &str, reason: &str, restarts: i64) -> Value {
        json!({"status": {"phase": "Pending", kind: [
            {"restartCount": restarts, "state": {"waiting": {"reason": reason}}}
        ]}})
    }

    fn crashing_pod(kind: &str, container: &str, message: &str) -> Value {
        json!({"metadata": {"name": "filebrowser-7d9c-x2x"},
               "status": {"phase": "Pending", kind: [
            {"name": container, "restartCount": 4,
             "state": {"waiting": {"reason": "CrashLoopBackOff"}},
             "lastState": {"terminated": {"exitCode": 1, "reason": "Error", "message": message}}}
        ]}})
    }

    #[test]
    fn a_crash_loop_is_reported_with_what_the_container_said_when_it_stopped() {
        let pod = crashing_pod(
            "initContainerStatuses",
            "wg-register",
            "filebrowser.6.yolab.io is already used by another app on this account\n",
        );
        let failure = container_failure(&[&pod]).unwrap();
        assert_eq!(failure.pod, "filebrowser-7d9c-x2x");
        assert_eq!(failure.container, "wg-register");
        assert!(failure.previous, "the reason belongs to the run that ended");
        assert_eq!(
            failure.describe(&failure.said),
            "wg-register: filebrowser.6.yolab.io is already used by another app on this account"
        );
    }

    #[test]
    fn a_crash_that_left_no_message_is_left_for_the_logs_to_explain() {
        let pod = crashing_pod("containerStatuses", "app", "");
        let failure = container_failure(&[&pod]).unwrap();
        assert!(failure.said.is_empty());
        assert_eq!(failure.exit_code, Some(1));
    }

    #[test]
    fn a_container_that_said_nothing_anywhere_still_names_itself_and_its_exit_code() {
        let pod = crashing_pod("containerStatuses", "app", "");
        let failure = container_failure(&[&pod]).unwrap();
        assert_eq!(
            failure.describe(""),
            "app stopped with exit code 1 and left no message"
        );
    }

    #[test]
    fn an_init_container_that_just_failed_is_a_failure_before_it_is_restarted() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending",
        "initContainerStatuses": [
            {"name": "init-db", "restartCount": 0,
             "state": {"terminated": {"exitCode": 1, "message": "could not initialise /db/filebrowser.db"}}}
        ]}});
        let failure = container_failure(&[&pod]).unwrap();
        assert!(
            !failure.previous,
            "the reason is in the run that just ended"
        );
        assert_eq!(
            failure.describe(&failure.said),
            "init-db: could not initialise /db/filebrowser.db"
        );
    }

    #[test]
    fn a_finished_init_container_is_not_a_failure() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Running",
        "initContainerStatuses": [
            {"name": "fix-perms", "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}
        ],
        "containerStatuses": [
            {"name": "app", "ready": false, "state": {"running": {}}}
        ]}});
        assert_eq!(container_failure(&[&pod]), None);
    }

    #[test]
    fn a_container_that_cannot_even_start_says_what_kubernetes_said() {
        for reason in CANNOT_START {
            let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending",
            "containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": reason,
                    "message": "secret \"filebrowser-admin\" not found"}}}
            ]}});
            let failure = container_failure(&[&pod]).expect(reason);
            assert_eq!(
                failure.describe(&failure.said),
                "app: secret \"filebrowser-admin\" not found",
                "{reason}"
            );
        }
    }

    #[test]
    fn a_container_that_cannot_start_without_a_message_still_gives_the_reason() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending",
        "containerStatuses": [
            {"name": "app", "state": {"waiting": {"reason": "ErrImagePull"}}}
        ]}});
        let failure = container_failure(&[&pod]).unwrap();
        assert_eq!(failure.describe(&failure.said), "app: ErrImagePull");
    }

    #[test]
    fn something_broken_outranks_something_merely_slow() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending", "containerStatuses": [
            {"name": "sidecar", "restartCount": 0, "state": {"waiting": {"reason": "ContainerCreating"}}},
            {"name": "app", "restartCount": 9, "state": {"waiting": {"reason": "CrashLoopBackOff"}},
             "lastState": {"terminated": {"exitCode": 2, "message": "bad config"}}}
        ]}});
        assert_eq!(container_failure(&[&pod]).unwrap().container, "app");
    }

    #[test]
    fn slow_states_are_not_failures() {
        for reason in ["ContainerCreating", "PodInitializing"] {
            let pod = waiting_pod("containerStatuses", reason, 0);
            assert_eq!(container_failure(&[&pod]), None, "{reason}");
        }
    }

    #[test]
    fn only_the_last_lines_that_say_something_are_kept_from_a_log() {
        assert_eq!(
            last_lines("Creating DNS record 'x'...\n\nERROR: POST returned HTTP 409: taken   \n"),
            "Creating DNS record 'x'...\nERROR: POST returned HTTP 409: taken"
        );
        assert_eq!(last_lines(""), "");
    }

    fn deployment(reason: &str) -> Value {
        json!({"status": {"conditions": [
            {"type": "Available", "status": "False", "reason": "MinimumReplicasUnavailable"},
            {"type": "Progressing", "status": "True", "reason": reason}
        ]}})
    }

    #[test]
    fn an_app_whose_rollout_once_completed_has_come_up() {
        let web = deployment("NewReplicaSetAvailable");
        let gateway = deployment("NewReplicaSetAvailable");
        assert!(has_come_up(&[&web, &gateway]));
    }

    #[test]
    fn an_app_with_any_rollout_still_in_progress_has_not_come_up() {
        let web = deployment("NewReplicaSetAvailable");
        for reason in [
            "ReplicaSetUpdated",
            "NewReplicaSetCreated",
            "ProgressDeadlineExceeded",
        ] {
            let gateway = deployment(reason);
            assert!(!has_come_up(&[&web, &gateway]), "{reason}");
        }
    }

    #[test]
    fn an_app_with_no_deployments_yet_has_not_come_up() {
        assert!(!has_come_up(&[]));
        assert!(!has_come_up(&[&json!({"status": {}})]));
    }

    #[tokio::test]
    async fn a_failure_with_no_message_is_explained_by_the_last_lines_of_its_log() {
        use crate::k8s::testing::api_server;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};
        let (server, kube) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces/yolab-notes/pods/notes-0/log"))
            .and(query_param("container", "app"))
            .and(query_param("previous", "true"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("starting\npanic: config.yaml: no such file\n"),
            )
            .mount(&server)
            .await;
        let failure = ContainerFailure {
            pod: "notes-0".into(),
            container: "app".into(),
            said: String::new(),
            exit_code: Some(2),
            previous: true,
            reason: "CrashLoopBackOff".into(),
            retry_at: None,
        };
        assert_eq!(
            explain_failure(&kube, "yolab-notes", &failure).await,
            "app: starting\npanic: config.yaml: no such file"
        );
    }

    #[tokio::test]
    async fn a_failure_whose_log_cannot_be_read_still_names_the_container() {
        let failure = ContainerFailure {
            pod: "notes-0".into(),
            container: "app".into(),
            said: String::new(),
            exit_code: Some(2),
            previous: true,
            reason: "CrashLoopBackOff".into(),
            retry_at: None,
        };
        assert_eq!(
            explain_failure(&crate::k8s::testing::unreachable(), "yolab-notes", &failure).await,
            "app stopped with exit code 2 and left no message"
        );
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
            copying_data("yolab-notes", &by_ns, &events)
                .map(|(said, _)| said)
                .as_deref(),
            Some("Copying this app's files… 37%")
        );
    }

    #[test]
    fn a_copy_before_the_first_percentage_still_reads_as_copying() {
        let pvc = cloning("minecraft-9hr2-data", "Pending");
        let mut by_ns: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
        by_ns.insert("yolab-notes", vec![&pvc]);
        assert_eq!(
            copying_data("yolab-notes", &by_ns, &[])
                .map(|(said, _)| said)
                .as_deref(),
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
    fn an_unknown_reason_is_said_plainly_and_kept_for_the_curious() {
        let pod = waiting_pod("containerStatuses", "SomeFutureReason", 0);
        let said = explain_app_state(&[&pod]);
        assert!(!said.contains("SomeFutureReason"), "{said}");
        assert_eq!(unexplained_waits(&[&pod]), "SomeFutureReason");
    }

    #[test]
    fn reasons_already_put_into_words_are_not_repeated_as_jargon() {
        let pod = waiting_pod("containerStatuses", "ContainerCreating", 0);
        assert_eq!(unexplained_waits(&[&pod]), "");
    }

    #[test]
    fn the_wait_is_counted_from_the_oldest_part() {
        let a = json!({"metadata": {"creationTimestamp": "2026-10-01T10:05:00Z"}});
        let b = json!({"metadata": {"creationTimestamp": "2026-10-01T10:00:00Z"}});
        assert_eq!(
            waiting_since(&[&a, &b]).as_deref(),
            Some("2026-10-01T10:00:00Z")
        );
        assert_eq!(waiting_since(&[]), None);
    }

    #[test]
    fn go_durations_are_read_as_kubernetes_writes_them() {
        assert_eq!(go_duration("5m0s"), Some(chrono::Duration::seconds(300)));
        assert_eq!(go_duration("1m20s"), Some(chrono::Duration::seconds(80)));
        assert_eq!(go_duration("40s"), Some(chrono::Duration::seconds(40)));
        assert_eq!(
            go_duration("1.5s"),
            Some(chrono::Duration::milliseconds(1500))
        );
        assert_eq!(go_duration("soon"), None);
        assert_eq!(go_duration(""), None);
    }

    #[test]
    fn a_crash_loop_knows_when_it_tries_again() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Running", "containerStatuses": [
            {"name": "app", "restartCount": 6,
             "state": {"waiting": {"reason": "CrashLoopBackOff",
                "message": "back-off 2m40s restarting failed container=app pod=p_yolab-notes(1234)"}},
             "lastState": {"terminated": {"exitCode": 1, "finishedAt": "2026-10-01T10:00:00Z"}}}
        ]}});
        let failure = container_failure(&[&pod]).unwrap();
        assert_eq!(failure.retry_at.as_deref(), Some("2026-10-01T10:02:40Z"));
        assert_eq!(failure.headline(), "It starts, then stops right away.");
    }

    #[test]
    fn a_crash_loop_without_a_back_off_message_has_no_guessed_time() {
        let pod = crashing_pod("containerStatuses", "app", "");
        assert_eq!(container_failure(&[&pod]).unwrap().retry_at, None);
    }

    #[test]
    fn running_out_of_memory_is_named() {
        let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Running", "containerStatuses": [
            {"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}},
             "lastState": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}
        ]}});
        let failure = container_failure(&[&pod]).unwrap();
        assert!(
            failure.headline().contains("memory"),
            "{}",
            failure.headline()
        );
    }

    #[test]
    fn kubernetes_reasons_never_reach_the_headline() {
        for reason in CANNOT_START {
            let pod = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending",
            "containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": reason}}}
            ]}});
            let headline = container_failure(&[&pod]).expect(reason).headline();
            assert!(!headline.contains(reason), "{reason}: {headline}");
            assert!(!headline.contains("app:"), "{reason}: {headline}");
        }
    }

    #[test]
    fn a_missing_image_is_told_apart_from_no_internet() {
        let pull = |message: &str| ContainerFailure {
            pod: "p".into(),
            container: "app".into(),
            said: message.into(),
            exit_code: None,
            previous: false,
            reason: "ErrImagePull".into(),
            retry_at: None,
        };
        assert!(pull("manifest unknown")
            .headline()
            .contains("could not be found"));
        assert!(pull("dial tcp: i/o timeout")
            .headline()
            .contains("internet"));
    }

    #[test]
    fn an_install_that_timed_out_says_so_plainly() {
        assert!(install_failure_headline("helm: context deadline exceeded").contains("too long"));
        assert_eq!(
            install_failure_headline("helm said no"),
            "Something went wrong while setting it up."
        );
    }

    #[test]
    fn no_pods_at_all_means_it_is_still_being_installed() {
        assert_eq!(explain_app_state(&[]), "Installing…");
    }

    #[test]
    fn a_stuck_init_container_is_not_hidden() {
        let pod = crashing_pod("initContainerStatuses", "init-db", "");
        assert_eq!(container_failure(&[&pod]).unwrap().container, "init-db");
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
            &settings,
        );
        assert_eq!(keys_of(&rows), vec!["url", "password"]);
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

    #[test]
    fn a_gpu_from_any_of_the_cluster_device_plugins_is_counted_once() {
        let items = vec![json!({ "spec": { "template": { "spec": { "containers": [
            { "resources": { "limits": { "nvidia.com/gpu-all": "1" } } },
            { "resources": { "requests": { "yolab.io/kfd": "1" }, "limits": { "yolab.io/kfd": "1" } } },
            { "resources": { "limits": { "yolab.io/dri": "1", "yolab.io/uinput": "1" } } }
        ] } } } })];
        assert_eq!(requested_resources(&items).gpu, 3);
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
        async fn an_installed_app_is_read_with_the_schema_it_was_installed_with() {
            let (server, kube) = api_server().await;
            let saved = json!({ "properties": { "config": { "properties": {
                "pin": { "type": "string", "writeOnly": true }
            }}}});
            Mock::given(method("GET"))
                .and(path(
                    "/api/v1/namespaces/yolab-notes/configmaps/yolab-schema",
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(crate::saved_chart::schema_manifest("yolab-notes", &saved)),
                )
                .mount(&server)
                .await;
            let empty_catalog = tempfile::tempdir().unwrap();
            let app = installed_schema(&kube, "yolab-notes", "notes", empty_catalog.path()).await;
            assert_eq!(
                app.credentials(),
                std::collections::HashSet::from(["pin".to_string()])
            );
        }

        #[tokio::test]
        async fn an_app_installed_before_schemas_were_kept_falls_back_to_its_chart_cache() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path(
                    "/api/v1/namespaces/yolab-notes/configmaps/yolab-schema",
                ))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            let catalog = tempfile::tempdir().unwrap();
            let chart = catalog.path().join("notes");
            std::fs::create_dir_all(&chart).unwrap();
            std::fs::write(chart.join("Chart.yaml"), "name: notes\nversion: 1.0.0\n").unwrap();
            std::fs::write(
                chart.join("values.schema.json"),
                r#"{"properties":{"config":{"properties":{"key":{"type":"string","writeOnly":true}}}}}"#,
            )
            .unwrap();
            let app = installed_schema(&kube, "yolab-notes", "notes", catalog.path()).await;
            assert_eq!(
                app.credentials(),
                std::collections::HashSet::from(["key".to_string()])
            );
        }

        async fn volumes_are(server: &MockServer, pvs: Vec<Value>) {
            Mock::given(method("GET"))
                .and(path("/api/v1/persistentvolumes"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(list("PersistentVolume", pvs)),
                )
                .mount(server)
                .await;
        }

        fn pv(name: &str, claim_namespace: &str) -> Value {
            json!({
                "metadata": { "name": name },
                "spec": { "claimRef": { "namespace": claim_namespace, "name": "data" } },
                "status": { "phase": "Released" }
            })
        }

        #[test]
        fn an_apps_volumes_are_the_ones_its_claims_were_bound_to() {
            let pvs = vec![
                pv("pvc-notes", "yolab-notes"),
                pv("pvc-notes-2", "yolab-notes-2"),
                json!({ "metadata": { "name": "pvc-unbound" }, "spec": {} }),
            ];
            assert_eq!(volumes_of(&pvs, "yolab-notes"), vec!["pvc-notes"]);
        }

        #[tokio::test]
        async fn an_uninstall_whose_volume_outlives_it_says_the_data_is_still_there() {
            let (server, kube) = api_server().await;
            volumes_are(
                &server,
                vec![
                    pv("pvc-notes", "yolab-notes"),
                    pv("pvc-other", "yolab-other"),
                ],
            )
            .await;
            let e = wait_for_volumes_deleted(
                &kube,
                "yolab-notes",
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(e.contains("pvc-notes"), "{e}");
            assert!(!e.contains("pvc-other"), "{e}");
            assert!(e.contains("data is still on the disks"), "{e}");
        }

        #[tokio::test]
        async fn volumes_that_cannot_be_listed_are_not_taken_as_deleted() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/persistentvolumes"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            let e = wait_for_volumes_deleted(
                &kube,
                "yolab-notes",
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(e.contains("could not be checked"), "{e}");
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
            volumes_are(&server, vec![]).await;
            let b = Backend {
                kube,
                host: FakeHost::new().ok("helm uninstall", ""),
            };

            run_teardown(&b, "notes", "yolab-notes").await.unwrap();
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
            volumes_are(&server, vec![]).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            run_teardown(&b, "notes", "yolab-notes").await.unwrap();
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
            volumes_are(&server, vec![]).await;
            let b = Backend {
                kube,
                host: FakeHost::new().fail("helm uninstall", "timed out"),
            };
            run_teardown(&b, "notes", "yolab-notes").await.unwrap();
        }

        #[tokio::test]
        async fn a_failed_install_is_marked_with_its_reason_and_nothing_is_deleted() {
            let (server, kube) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path(NS_PATH))
                .and(body_partial_json(json!({
                    "metadata": { "annotations": { ANN_INSTALL_FAILED: "helm said no" } }
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(ns(json!({}), "Active")))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            mark_install_failed(&kube, "yolab-notes", "helm said no").await;
        }

        #[tokio::test]
        async fn a_successful_update_clears_the_failed_mark() {
            let (server, kube) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path(NS_PATH))
                .and(body_partial_json(json!({
                    "metadata": { "annotations": { ANN_INSTALL_FAILED: null } }
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(ns(json!({}), "Active")))
                .expect(1)
                .mount(&server)
                .await;
            clear_install_failed(&kube, "yolab-notes").await;
        }

        #[tokio::test]
        async fn an_install_that_failed_before_its_namespace_existed_has_nothing_to_mark() {
            let (server, kube) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path(NS_PATH))
                .respond_with(ResponseTemplate::new(404).set_body_json(gone()))
                .mount(&server)
                .await;
            set_install_failed(&kube, "yolab-notes", json!("too early"))
                .await
                .expect("a namespace that was never created is not an error");
        }

        #[test]
        fn a_marked_namespace_reports_why_its_install_failed() {
            let ann = json!({ ANN_INSTALL_FAILED: "helm said no" });
            assert_eq!(
                install_failure(ann.as_object().unwrap()).as_deref(),
                Some("helm said no")
            );
            assert_eq!(install_failure(&serde_json::Map::new()), None);
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

            let def = read_definition(&kube, "yolab-notes").await.unwrap();
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
            let e = read_definition(&kube, "yolab-notes").await.unwrap_err();
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

    fn switched_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "yolab_enabled": {"type": "boolean", "default": true},
                "tor_enabled": {"type": "boolean"},
                "tailscale_enabled": {"type": "boolean"}
            },
            "dependencies": {
                "yolab_enabled": {"oneOf": [
                    {"properties": {"yolab_enabled": {"const": false}}, "required": ["yolab_enabled"]},
                    {"properties": {
                        "yolab_enabled": {"const": true},
                        "subdomain": {"type": "string", "format": "tunnel", "default": "jellyfin"},
                        "yolab_token": {"type": "string", "format": "yolab-token", "writeOnly": true}
                    }, "required": ["subdomain"]}
                ]}
            }
        })
    }

    fn json_cfg(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_subdomain_behind_the_yolab_switch_names_the_service() {
        assert_eq!(
            resolve_service_name(&switched_schema(), &cfg(&[("subdomain", "films")])),
            "films"
        );
        let whole = serde_json::json!({"properties": {"config": switched_schema()}});
        assert_eq!(
            resolve_service_name(&whole, &cfg(&[("subdomain", "films")])),
            "films"
        );
    }

    #[test]
    fn with_the_yolab_address_off_the_service_keeps_its_default_name() {
        let off = json_cfg(serde_json::json!({"yolab_enabled": false}));
        assert_eq!(resolve_service_name(&switched_schema(), &off), "jellyfin");
    }

    #[test]
    fn the_form_token_never_stays_in_the_saved_settings() {
        let mut c = json_cfg(serde_json::json!({"subdomain": "x", "yolab_token": "  tok-123 "}));
        assert_eq!(take_yolab_token(&mut c).as_deref(), Some("tok-123"));
        assert!(!c.contains_key("yolab_token"));
    }

    #[test]
    fn an_empty_or_untouched_token_field_means_this_box_s_account() {
        for value in [
            serde_json::json!(""),
            serde_json::json!("   "),
            serde_json::json!(REDACTED),
        ] {
            let mut c = json_cfg(serde_json::json!({"yolab_token": value}));
            assert_eq!(take_yolab_token(&mut c), None);
            assert!(!c.contains_key("yolab_token"));
        }
        let mut none = cfg(&[("subdomain", "x")]);
        assert_eq!(take_yolab_token(&mut none), None);
    }

    #[test]
    fn an_app_with_every_way_in_switched_off_is_refused() {
        let schema = switched_schema();
        let refused = |v: Value| way_in_refused(&schema, &json_cfg(v)).is_some();
        assert!(refused(serde_json::json!({"yolab_enabled": false})));
        assert!(refused(
            serde_json::json!({"yolab_enabled": false, "tor_enabled": false})
        ));
        assert!(!refused(
            serde_json::json!({"yolab_enabled": false, "tor_enabled": true})
        ));
        assert!(!refused(
            serde_json::json!({"yolab_enabled": false, "tailscale_enabled": true})
        ));
        assert!(!refused(serde_json::json!({"yolab_enabled": true})));
        assert!(!refused(
            serde_json::json!({"subdomain": "from-before-the-switch"})
        ));
    }

    #[test]
    fn a_file_explorer_with_every_way_in_switched_off_is_refused() {
        let schema = switched_schema();
        let refused = |v: Value| way_in_refused(&schema, &json_cfg(v)).is_some();
        assert!(refused(serde_json::json!({
            "file_explorer_enabled": true,
            "file_explorer_yolab_enabled": false,
        })));
        assert!(refused(serde_json::json!({
            "yolab_enabled": false,
            "tor_enabled": true,
            "file_explorer_enabled": true,
        })));
        assert!(!refused(serde_json::json!({
            "file_explorer_enabled": true,
            "file_explorer_yolab_enabled": false,
            "file_explorer_tor_enabled": true,
        })));
        assert!(!refused(serde_json::json!({
            "file_explorer_enabled": true,
            "file_explorer_yolab_enabled": false,
            "file_explorer_tailscale_enabled": true,
        })));
        assert!(!refused(serde_json::json!({
            "file_explorer_enabled": false,
            "file_explorer_yolab_enabled": false,
        })));
    }

    #[test]
    fn a_file_explorer_from_before_its_switches_follows_the_app_s_yolab_address() {
        let schema = switched_schema();
        let refused = |v: Value| way_in_refused(&schema, &json_cfg(v)).is_some();
        assert!(!refused(serde_json::json!({"file_explorer_enabled": true})));
        assert!(!refused(serde_json::json!({
            "yolab_enabled": false,
            "tor_enabled": true,
            "file_explorer_enabled": true,
            "file_explorer_yolab_enabled": true,
        })));
    }

    #[test]
    fn apps_without_the_switch_are_not_judged() {
        let no_switch = serde_json::json!({"properties": {"subdomain": {"format": "tunnel"}}});
        assert_eq!(way_in_refused(&no_switch, &cfg(&[])), None);
    }

    #[test]
    fn a_token_someone_typed_in_survives_upgrades_and_the_box_s_does_not_block_rotation() {
        let typed = tunnel_secret("yolab-x", "tok-typed", TOKEN_FROM_FORM);
        let boxed = tunnel_secret("yolab-x", "tok-box", "box");
        assert!(keeps_form_token(Some(&typed)));
        assert!(!keeps_form_token(Some(&boxed)));
        assert!(!keeps_form_token(None));
        assert_eq!(typed["metadata"]["name"], "yolab-tunnel-credentials");
        assert_eq!(typed["stringData"]["account-token"], "tok-typed");
    }
}
