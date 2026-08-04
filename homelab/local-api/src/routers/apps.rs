use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{config::Config, error::Result, proc::KillOnDrop, AppState};

const LABEL_MANAGED: &str = "yolab.io/managed";
/// Shared with backup_run.rs, which exports this annotation so a restored namespace
/// keeps its app identity — a single definition so the two can never drift apart.
pub(crate) const ANN_APP_ID: &str = "yolab.io/app-id";
/// Chart version this instance was installed from. Captured in backups alongside the
/// image digests, so a restore can say which packaging produced the data rather than
/// leaving the user to find out from a crash loop.
pub(crate) const ANN_CHART_VERSION: &str = "yolab.io/chart-version";
const ANN_CONFIG: &str = "yolab.io/config";
const ANN_OUTPUTS: &str = "yolab.io/outputs";
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
    pub status: String,
    pub outputs: Vec<AppOutput>,
    pub outputs_spec: Vec<OutputSpec>,
    pub config: serde_json::Map<String, Value>,
}

#[derive(Serialize)]
pub struct CatalogApp {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub category: String,
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
pub struct DescribeResponse {
    pub output: String,
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
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Overwrite a namespace annotation, logging (rather than swallowing) failures.
/// A silently-failed annotate loses an app's persisted config or outputs.
async fn annotate_ns(ns: &str, key: &str, value: &str) {
    match tokio::process::Command::new("kubectl")
        .args(["annotate", "namespace", ns, &format!("{key}={value}"), "--overwrite=true"])
        .output()
        .await
    {
        Ok(o) if !o.status.success() => {
            tracing::warn!("annotate {ns} {key} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Err(e) => tracing::warn!("annotate {ns} {key} spawn failed: {e}"),
        _ => {}
    }
}

fn tunnel_config(cfg: &Config) -> anyhow::Result<toml::Table> {
    let text = std::fs::read_to_string(&cfg.config_path)?;
    let table: toml::Table = toml::from_str(&text)?;
    table["tunnel"]
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))
}

// ── Chart metadata ────────────────────────────────────────────────────────────
//
// Apps are Helm charts. What used to be five files per app (app.toml, schema.json,
// uischema.json, outputs.json, manifest.yaml.j2) is now Chart.yaml + values.schema.json
// + templates/, with the YoLab-specific bits carried as chart annotations — the standard
// escape hatch for metadata Helm has no field for. Reading them here rather than from a
// bespoke layout is what lets a chart from someone else's repo work unmodified.

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
    version: String,
    #[serde(default, rename = "type")]
    type_: String,
    #[serde(default)]
    annotations: std::collections::HashMap<String, String>,
}

struct ChartMeta {
    chart: ChartYaml,
    /// The user-facing form schema: values.schema.json's `properties.config`. Nesting it
    /// under `config` keeps Helm able to validate the WHOLE values object (including the
    /// platform-injected `yolab` subtree) while leaving the form schema extractable
    /// exactly as the UI already expects it.
    schema: Value,
}

impl ChartMeta {
    fn ann(&self, key: &str) -> &str {
        self.chart.annotations.get(key).map(String::as_str).unwrap_or("")
    }
    /// Annotations hold JSON as a string (YAML block scalar); parse or fall back.
    fn ann_json(&self, key: &str) -> Value {
        serde_json::from_str(self.ann(key)).unwrap_or(Value::Null)
    }
    fn display_name(&self) -> String {
        let n = self.ann(ANN_DISPLAY_NAME);
        if n.is_empty() { self.chart.name.clone() } else { n.to_string() }
    }
}

fn read_chart(dir: &std::path::Path) -> Option<ChartMeta> {
    let chart: ChartYaml =
        serde_norway::from_str(&std::fs::read_to_string(dir.join("Chart.yaml")).ok()?).ok()?;
    // Library charts (yolab-common) are building blocks, not installable apps.
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

/// The chart's log-scraping output specs (`yolab.io/outputs`). Empty when the chart
/// declares none, or when the app was installed from a chart no longer in the catalog.
fn chart_outputs_spec(catalog_dir: &std::path::Path, id: &str) -> Vec<Value> {
    if id.is_empty() {
        return Vec::new();
    }
    read_chart(&catalog_dir.join(id))
        .map(|m| m.ann_json(ANN_CHART_OUTPUTS))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// The tunnel subdomain the user asked for: the value of whichever config field the
/// chart's schema marks `format: tunnel`. wg-register needs it to claim the subdomain.
fn resolve_service_name(schema: &Value, config: &serde_json::Map<String, Value>) -> String {
    schema["properties"]
        .as_object()
        .and_then(|props| {
            props.iter().find_map(|(k, v)| {
                (v["format"].as_str() == Some("tunnel")).then(|| k.clone())
            })
        })
        .and_then(|f| config.get(&f).cloned())
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Values file handed to Helm. Everything the user chose goes under `config`; everything
/// the platform injects goes under `yolab`, so a chart can never confuse the two and a
/// malicious chart's values cannot smuggle in a different account token.
fn build_values(
    config: &serde_json::Map<String, Value>,
    tunnel_cfg: &toml::Table,
    service_name: &str,
) -> String {
    serde_json::json!({
        "config": config,
        "yolab": {
            "platformApiUrl": tunnel_cfg.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or(""),
            "accountToken": tunnel_cfg.get("account_token").and_then(|v| v.as_str()).unwrap_or(""),
            "serviceName": service_name,
        },
    })
    // A values file is YAML, and JSON is valid YAML — so this needs no YAML serializer
    // and cannot produce the indentation bugs hand-built YAML is prone to.
    .to_string()
}

/// Runs a helm command, streaming stdout+stderr to the client as SSE.
///
/// Helm writes progress and errors to stderr, so both are forwarded — the old
/// `kubectl apply` streamer only forwarded stdout, which is why a failed apply surfaced
/// as a bare exit code with no explanation.
fn helm_stream(args: Vec<String>) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    async_stream::stream! {
        let child = tokio::process::Command::new("helm")
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut guard = match child {
            Ok(c) => KillOnDrop(c),
            Err(e) => {
                yield Ok(Event::default().data(format!("[ERROR] could not run helm: {e}")));
                return;
            }
        };
        use tokio::io::AsyncBufReadExt;
        let stdout = guard.0.stdout.take();
        let stderr = guard.0.stderr.take();
        let mut out = stdout.map(|s| tokio::io::BufReader::new(s).lines());
        let mut err = stderr.map(|s| tokio::io::BufReader::new(s).lines());
        let mut out_done = out.is_none();
        let mut err_done = err.is_none();
        while !out_done || !err_done {
            tokio::select! {
                l = async { out.as_mut().unwrap().next_line().await }, if !out_done => match l {
                    Ok(Some(line)) => yield Ok(Event::default().data(line)),
                    _ => out_done = true,
                },
                l = async { err.as_mut().unwrap().next_line().await }, if !err_done => match l {
                    Ok(Some(line)) => yield Ok(Event::default().data(line)),
                    _ => err_done = true,
                },
            }
        }
        let rc = guard.0.wait().await.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
        if rc != 0 {
            yield Ok(Event::default().data(format!("[ERROR] helm exited {rc}")));
        }
    }
}

/// Creates the app's namespace with the labels and annotations YoLab depends on.
///
/// Deliberately NOT part of the chart. `yolab.io/managed` is what the backup's PVC
/// inventory and cluster export select on, and `yolab.io/app-id` is what identifies the
/// app after a restore — leaving those to chart authors would mean a third-party chart
/// could silently opt itself out of being backed up.
async fn ensure_app_namespace(ns: &str, app_id: &str, chart_version: &str) -> anyhow::Result<()> {
    crate::kubectl::apply(
        &serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": ns,
                "labels": { LABEL_MANAGED: "true" },
                "annotations": {
                    ANN_APP_ID: app_id,
                    ANN_CHART_VERSION: chart_version,
                    "volsync.backube/privileged-movers": "true",
                },
            },
        })
        .to_string(),
    )
    .await
}

fn normalize_outputs(ann: &serde_json::Map<String, Value>) -> Vec<AppOutput> {
    let raw = ann.get(ANN_OUTPUTS).and_then(|v| v.as_str()).unwrap_or("");
    if raw.is_empty() {
        return vec![];
    }
    let Ok(outputs) = serde_json::from_str::<Vec<Value>>(raw) else { return vec![] };
    // Handle old format [{url, ipv6}]
    if outputs.first().map(|o| o.get("url").is_some() || o.get("ipv6").is_some()).unwrap_or(false) {
        let mut result = vec![];
        for o in &outputs {
            if let Some(url) = o["url"].as_str().filter(|s| !s.is_empty()) {
                result.push(AppOutput { key: "url".into(), label: "Web URL".into(), value: url.into(), type_: "url".into() });
            }
            if let Some(ip) = o["ipv6"].as_str().filter(|s| !s.is_empty()) {
                result.push(AppOutput { key: "ipv6".into(), label: "IPv6".into(), value: ip.into(), type_: "text".into() });
            }
        }
        return result;
    }
    outputs.into_iter().filter_map(|o| serde_json::from_value(o).ok()).collect()
}

/// Reject config scalars that could break out of a YAML scalar and inject
/// structure into the rendered manifest. Tera writes context string values
/// verbatim, so an embedded newline in e.g. a "domain" field could smuggle an
/// extra key/document into the applied manifest. All current catalog fields are
/// single-line scalars, so rejecting control characters has no false positives.
fn validate_config_values(config: &serde_json::Map<String, Value>) -> std::result::Result<(), String> {
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

// render_manifest + apply_manifest_stream lived here. Both are gone: Helm renders the
// chart and applies the result itself, so there is no hand-rolled template context to
// keep in sync with each app's variable names, and no separate "write a temp manifest,
// kubectl apply it, hope stderr wasn't important" path.

// ── Routes ────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct AccountTokenResponse {
    pub account_token: String,
}

pub async fn account_token(State(state): State<AppState>) -> Result<Json<AccountTokenResponse>> {
    let tunnel = tunnel_config(&state.config)?;
    let token = tunnel
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Json(AccountTokenResponse { account_token: token }))
}

/// Strip scheme and trailing slash from a dns_url, then drop the leading
/// subdomain label to yield the apex tunnel domain. A purely numeric first
/// label (an IP-like host) is kept as-is.
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
    Ok(Json(DomainResponse { domain: derive_domain(dns_url) }))
}

pub async fn catalog(State(state): State<AppState>) -> Json<Vec<CatalogApp>> {
    let catalog_dir = state.config.catalog_dir();
    let Ok(rd) = std::fs::read_dir(&catalog_dir) else { return Json(vec![]) };
    let mut apps = vec![];
    for entry in rd.flatten() {
        let Some(meta) = read_chart(&entry.path()) else { continue };
        apps.push(CatalogApp {
            id: meta.chart.name.clone(),
            name: meta.display_name(),
            description: meta.chart.description.clone(),
            icon: meta.ann(ANN_ICON).to_string(),
            category: meta.ann(ANN_CATEGORY).to_string(),
            schema: meta.schema.clone(),
            uischema: meta.ann_json(ANN_UISCHEMA),
        });
    }
    // read_dir order is filesystem-dependent; sort so the storefront is stable.
    apps.sort_by_key(|a| a.name.to_lowercase());
    Json(apps)
}

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let catalog_dir = state.config.catalog_dir();
    let (ns_out, pods_out) = tokio::join!(
        tokio::process::Command::new("kubectl")
            .args(["get", "namespaces", "-l", &format!("{LABEL_MANAGED}=true"), "-o", "json"])
            .output(),
        tokio::process::Command::new("kubectl")
            .args(["get", "pods", "--all-namespaces", "-o", "json"])
            .output(),
    );
    let v: Value = serde_json::from_slice(&ns_out?.stdout)?;

    // Build a pod-by-namespace index from the single bulk query so list_apps
    // requires only two kubectl calls regardless of app count.
    let pods_v: Value = pods_out.ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or_else(|| serde_json::json!({"items": []}));
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
        let ann = ns["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
        let name = ns["metadata"]["name"].as_str().unwrap_or("").trim_start_matches("yolab-").to_string();
        let phase = ns["status"]["phase"].as_str().unwrap_or("Active");
        let status = if phase == "Terminating" {
            "uninstalling".to_string()
        } else {
            let ns_full = format!("yolab-{name}");
            let items = pods_by_ns.get(ns_full.as_str()).map(|v| v.as_slice()).unwrap_or(&[]);
            if items.is_empty() {
                "starting".to_string()
            } else {
                let all_ready = items.iter().all(|p| {
                    p["status"]["conditions"].as_array()
                        .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
                        .unwrap_or(false)
                });
                if all_ready { "running" } else { "starting" }.to_string()
            }
        };

        let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let config: serde_json::Map<String, Value> = ann.get(ANN_CONFIG)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let outputs_spec = chart_outputs_spec(&catalog_dir, &id)
            .into_iter()
            .filter(|o| o["type"].as_str() != Some("hidden"))
            .filter_map(|o| Some(OutputSpec {
                key: o["key"].as_str()?.to_string(),
                label: o.get("label").and_then(|v| v.as_str()).unwrap_or(o["key"].as_str()?).to_string(),
                type_: o.get("type").and_then(|v| v.as_str()).unwrap_or("text").to_string(),
            }))
            .collect();

        apps.push(AppInfo { app_id: id, instance_name: name, status, outputs: normalize_outputs(&ann), outputs_spec, config });
    }
    Ok(Json(apps))
}

pub async fn install_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InstallRequest>,
) -> impl IntoResponse {
    if !body.instance_name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return (StatusCode::BAD_REQUEST, "instance_name must be lowercase alphanumeric and hyphens").into_response();
    }
    if let Err(e) = validate_config_values(&body.config) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response();
    }
    if !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::NOT_FOUND, format!("App '{id}' not found")).into_response();
    }

    let stream = async_stream::stream! {
        let Ok(tunnel_cfg) = tunnel_config(&state.config) else {
            yield Ok(Event::default().data("[ERROR] could not read tunnel config"));
            return;
        };
        let chart_dir = state.config.catalog_dir().join(&id);
        let Some(meta) = read_chart(&chart_dir) else {
            yield Ok(Event::default().data(format!("[ERROR] {id} is not a valid chart")));
            return;
        };

        let ns = format!("yolab-{}", body.instance_name);
        // Namespace first: the chart's resources are namespaced, and the labels/
        // annotations set here are what the backup layer selects on.
        yield Ok(Event::default().data("Preparing namespace..."));
        if let Err(e) = ensure_app_namespace(&ns, &id, &meta.chart.version).await {
            yield Ok(Event::default().data(format!("[ERROR] create namespace: {e}")));
            return;
        }

        let service_name = resolve_service_name(&meta.schema, &body.config);
        let values = build_values(&body.config, &tunnel_cfg, &service_name);
        let tmp = match tempfile::Builder::new().suffix(".json").tempfile() {
            Ok(t) => t,
            Err(e) => { yield Ok(Event::default().data(format!("[ERROR] staging values: {e}"))); return; }
        };
        if let Err(e) = std::fs::write(tmp.path(), &values) {
            yield Ok(Event::default().data(format!("[ERROR] write values: {e}")));
            return;
        }

        yield Ok(Event::default().data("Installing chart..."));
        // `upgrade --install` rather than `install`: a retry after a partial failure then
        // converges instead of erroring with "release already exists" and leaving the user
        // stuck with a half-installed app they can't retry or remove from the UI.
        let args: Vec<String> = vec![
            "upgrade".into(), "--install".into(),
            body.instance_name.clone(), chart_dir.to_string_lossy().to_string(),
            "-n".into(), ns.clone(),
            "--values".into(), tmp.path().to_string_lossy().to_string(),
        ];
        let s = helm_stream(args);
        tokio::pin!(s);
        use futures::StreamExt;
        while let Some(ev) = s.next().await { yield ev; }
        drop(tmp);

        // Wire up VolSync ReplicationSource(s) for any PVCs this app created.
        crate::routers::backups::setup_namespace_backup(&ns).await;
        // Persisted on the namespace (not only in Helm's release Secret) because the
        // backup's identity export reads namespace annotations.
        let config_json = serde_json::to_string(&body.config).unwrap_or_default();
        annotate_ns(&ns, ANN_CONFIG, &config_json).await;
        yield Ok(Event::default().data(format!("[DONE] {id} installed — run 'Scan outputs' once the pod is ready")));
    };

    Sse::new(stream).into_response()
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
    let Ok(ns_out) = tokio::process::Command::new("kubectl")
        .args(["get", "namespace", &ns, "-o", "json"])
        .output().await
    else {
        return (StatusCode::NOT_FOUND, "Instance not found").into_response();
    };
    let Ok(ns_v) = serde_json::from_slice::<Value>(&ns_out.stdout) else {
        return (StatusCode::NOT_FOUND, "Instance not found").into_response();
    };
    let ann = ns_v["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
    let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stored_config: serde_json::Map<String, Value> = ann.get(ANN_CONFIG)
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    // Caller may supply a new config; fall back to the stored one.
    let config = body.and_then(|b| b.0.config).unwrap_or(stored_config);

    if let Err(e) = validate_config_values(&config) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response();
    }

    if id.is_empty() || !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::BAD_REQUEST, "App not found in catalog").into_response();
    }

    let stream = async_stream::stream! {
        let Ok(tunnel_cfg) = tunnel_config(&state.config) else {
            yield Ok(Event::default().data("[ERROR] could not read tunnel config"));
            return;
        };
        let chart_dir = state.config.catalog_dir().join(&id);
        let Some(meta) = read_chart(&chart_dir) else {
            yield Ok(Event::default().data(format!("[ERROR] {id} is not a valid chart")));
            return;
        };

        let service_name = resolve_service_name(&meta.schema, &config);
        let values = build_values(&config, &tunnel_cfg, &service_name);
        let tmp = match tempfile::Builder::new().suffix(".json").tempfile() {
            Ok(t) => t,
            Err(e) => { yield Ok(Event::default().data(format!("[ERROR] staging values: {e}"))); return; }
        };
        if let Err(e) = std::fs::write(tmp.path(), &values) {
            yield Ok(Event::default().data(format!("[ERROR] write values: {e}")));
            return;
        }

        yield Ok(Event::default().data("Upgrading release..."));
        let args: Vec<String> = vec![
            "upgrade".into(), "--install".into(),
            instance_name.clone(), chart_dir.to_string_lossy().to_string(),
            "-n".into(), ns.clone(),
            "--values".into(), tmp.path().to_string_lossy().to_string(),
        ];
        let s = helm_stream(args);
        tokio::pin!(s);
        use futures::StreamExt;
        while let Some(ev) = s.next().await { yield ev; }
        drop(tmp);

        // No explicit `kubectl rollout restart` any more. Helm diffs the rendered
        // manifests and restarts only what actually changed — and charts that need a
        // restart on a config-only change (e.g. a password held in a Secret) carry a
        // checksum annotation on the pod template, which is the idiomatic way to say so.
        let config_json = serde_json::to_string(&config).unwrap_or_default();
        annotate_ns(&ns, ANN_CONFIG, &config_json).await;
        yield Ok(Event::default().data(format!("[DONE] {id} updated")));
    };

    Sse::new(stream).into_response()
}

pub async fn scan_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<ScanOutputsResponse>> {
    let ns = format!("yolab-{instance_name}");
    let ns_out = tokio::process::Command::new("kubectl")
        .args(["get", "namespace", &ns, "-o", "json"])
        .output().await?;
    let ns_v: Value = serde_json::from_slice(&ns_out.stdout)?;
    let ann = ns_v["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
    let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let outputs_spec = chart_outputs_spec(&state.config.catalog_dir(), &id);
    if outputs_spec.is_empty() {
        return Ok(Json(ScanOutputsResponse { outputs: normalize_outputs(&ann) }));
    }

    // Compile regex patterns once — recompiling inside the inner log-line loop
    // is O(patterns × lines) compilations which blows up on long logs.
    struct CompiledSpec {
        key: String,
        label: String,
        type_: String,
        re: Option<regex::Regex>,
    }
    let compiled: Vec<CompiledSpec> = outputs_spec.iter().filter_map(|spec| {
        let key = spec["key"].as_str()?.to_string();
        Some(CompiledSpec {
            re: spec["pattern"].as_str().and_then(|p| regex::Regex::new(p).ok()),
            label: spec.get("label").and_then(|v| v.as_str()).unwrap_or(&key).to_string(),
            type_: spec.get("type").and_then(|v| v.as_str()).unwrap_or("text").to_string(),
            key,
        })
    }).collect();

    let pods_out = tokio::process::Command::new("kubectl")
        .args(["get", "pods", "-n", &ns, "-o", "json"])
        .output().await?;
    let pods_v: Value = serde_json::from_slice(&pods_out.stdout)?;
    let mut found: std::collections::HashMap<String, String> = Default::default();

    'outer: for pod in pods_v["items"].as_array().unwrap_or(&vec![]) {
        let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
        let empty = vec![];
        let init_containers = pod["spec"]["initContainers"].as_array().unwrap_or(&empty);
        let main_containers = pod["spec"]["containers"].as_array().unwrap_or(&empty);
        let containers: Vec<&str> = init_containers.iter().chain(main_containers.iter())
            .filter_map(|c| c["name"].as_str()).collect();
        for container in containers {
            let logs = tokio::process::Command::new("kubectl")
                .args(["logs", "-n", &ns, pod_name, "-c", container,
                       &format!("--tail={LOGS_SCAN_TAIL}")])
                .output().await;
            let Ok(logs) = logs else { continue };
            let text = String::from_utf8_lossy(&logs.stdout);
            for line in text.lines() {
                for cs in &compiled {
                    if found.contains_key(&cs.key) { continue; }
                    if let Some(re) = &cs.re {
                        if let Some(cap) = re.captures(line).and_then(|c| c.get(1)) {
                            found.insert(cs.key.clone(), cap.as_str().to_string());
                        }
                    }
                }
            }
            // Stop as soon as all keys are found.
            if found.len() == compiled.len() { break 'outer; }
        }
    }

    if found.is_empty() {
        return Ok(Json(ScanOutputsResponse { outputs: normalize_outputs(&ann) }));
    }

    let outputs: Vec<AppOutput> = compiled.iter()
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

pub async fn uninstall_app(
    State(_state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let ns = format!("yolab-{instance_name}");

    // `helm uninstall` runs the chart's pre-delete hook (tunnel cleanup) and waits for
    // it before removing anything. That replaces rendering an uninstall template by
    // hand, applying it, and polling `kubectl wait job/uninstall --timeout=120s` — and
    // unlike that version, a hook that fails shows up in the output instead of being
    // silently skipped on its way to deleting the namespace anyway.
    let out = tokio::process::Command::new("helm")
        .args(["uninstall", &instance_name, "-n", &ns, "--ignore-not-found", "--wait"])
        .output()
        .await;
    match out {
        Ok(o) if !o.status.success() => {
            // Not fatal: the namespace delete below still tears the app down. But it
            // must be visible, because the thing that most commonly fails here is the
            // tunnel cleanup, which leaves an orphaned tunnel on the platform.
            tracing::warn!(
                "uninstall {instance_name}: helm uninstall failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => tracing::warn!("uninstall {instance_name}: could not run helm: {e}"),
        _ => {}
    }

    tokio::process::Command::new("kubectl")
        .args(["delete", "namespace", &ns, "--ignore-not-found=true", "--wait=false"])
        .output().await?;

    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn list_pods(
    Path(instance_name): Path<String>,
) -> Result<Json<Vec<PodInfo>>> {
    let out = tokio::process::Command::new("kubectl")
        .args(["get", "pods", "-n", &format!("yolab-{instance_name}"), "-o", "json"])
        .output().await?;
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(Json(
        v["items"].as_array().unwrap_or(&vec![]).iter().map(|p| PodInfo {
            name: p["metadata"]["name"].as_str().unwrap_or("").to_string(),
            phase: p["status"]["phase"].as_str().unwrap_or("Unknown").to_string(),
            ready: p["status"]["conditions"].as_array()
                .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
                .unwrap_or(false),
        }).collect()
    ))
}

pub async fn describe_pod(
    Path((instance_name, pod_name)): Path<(String, String)>,
) -> Result<Json<DescribeResponse>> {
    let out = tokio::process::Command::new("kubectl")
        .args(["describe", "pod", &pod_name, "-n", &format!("yolab-{instance_name}")])
        .output().await?;
    Ok(Json(DescribeResponse {
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }))
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
        let Ok(c) = child else { return; };
        let mut guard = KillOnDrop(c);
        use tokio::io::AsyncBufReadExt;
        let stdout = guard.0.stdout.take().unwrap();
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            yield Ok(Event::default().data(l));
        }
        let _ = guard.0.wait().await;
    };
    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn derive_domain_drops_subdomain() {
        assert_eq!(derive_domain("https://yolab.10.demycode.ovh"), "10.demycode.ovh");
        assert_eq!(derive_domain("http://node1.example.com/"), "example.com");
    }

    #[test]
    fn derive_domain_keeps_numeric_first_label() {
        // A purely numeric first label (IP-ish) is kept whole.
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
    fn normalize_outputs_legacy_format() {
        // Old shape: [{url, ipv6}] gets expanded into url + ipv6 rows.
        let ann = map(json!({
            ANN_OUTPUTS: r#"[{"url":"https://x","ipv6":"fd00::1"}]"#
        }));
        let out = normalize_outputs(&ann);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, "url");
        assert_eq!(out[1].key, "ipv6");
    }

    #[test]
    fn normalize_outputs_empty() {
        assert!(normalize_outputs(&serde_json::Map::new()).is_empty());
    }
}

