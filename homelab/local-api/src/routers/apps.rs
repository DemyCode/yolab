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
/// Which repository the chart came from.
///
/// `app-id` alone is ambiguous the moment more than one repo is configured — two repos
/// can both ship a chart called `gitea`, and `app-id` is what the backup identity export
/// and the restore path use to decide what an app *is*. Recording the repo now, while
/// nothing has been installed yet, avoids a migration later against live data.
pub(crate) const ANN_CHART_REPO: &str = "yolab.io/chart-repo";

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
    /// Repository this chart came from. The UI uses it to distinguish the curated
    /// catalog from charts a user added themselves — which matters, because a chart can
    /// create arbitrary cluster objects, so "who published this" is a security fact and
    /// not decoration.
    pub repo: String,
    pub name: String,
    pub description: String,
    /// The project's own website, from Chart.yaml's `home`. Empty for a chart that
    /// does not declare one (including uploaded ones), and the UI simply omits the link.
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
    /// Helm's own field for the project's website. Surfaced to the storefront because
    /// a one-line description cannot explain what most of these apps are — the honest
    /// answer to "what is Karakeep?" is the project's own page, and a name with no way
    /// to look it up is a name someone will not install.
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
    /// The user-facing form schema: values.schema.json's `properties.config`. Nesting it
    /// under `config` keeps Helm able to validate the WHOLE values object (including the
    /// platform-injected `yolab` subtree) while leaving the form schema extractable
    /// exactly as the UI already expects it.
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
    /// Annotations hold JSON as a string (YAML block scalar); parse or fall back.
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
///
/// `schema` here is ALREADY the config subtree — `read_chart` stores
/// `values.schema.json`'s `properties.config` — so the tunnel field sits at
/// `properties.<field>`. Both that and the fully-nested shape are accepted, so
/// this keeps working if a caller ever passes the whole values schema.
///
/// (An earlier version of this comment claimed the opposite. The empty
/// serviceName that prompted it came from the UI posting `config: {}`, because
/// InstallPage unwrapped `properties.config` a second time and rendered no
/// fields at all — not from the lookup path being wrong.)
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

    // The user's answer, falling back to the schema's declared default.
    //
    // The fallback is not belt-and-braces — it is the normal path. An install
    // submitted with `config: {}` (seen live) leaves no subdomain at all, and an
    // empty serviceName means wg-register registers no DNS record, YOLAB_FQDN
    // comes out blank, and the app's Caddy dies on a Caddyfile whose site block
    // collapsed to a bare `{`. A schema that declares `"default": "qbittorrent"`
    // is stating what to use when the field is absent; ignoring that turned a
    // missing optional answer into a broken install.
    config
        .get(&field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| spec["default"].as_str().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_string()
}

/// Values file handed to Helm. Everything the user chose goes under `config`; everything
/// the platform injects goes under `yolab`, so a chart can never confuse the two and a
/// malicious chart's values cannot smuggle in a different account token.
fn build_values(
    config: &serde_json::Map<String, Value>,
    tunnel_cfg: &toml::Table,
    service_name: &str,
) -> String {
    // No accountToken here on purpose. Chart values are persisted verbatim in the Helm
    // release Secret and echoed by `helm get values`, so passing the token as a value
    // would put the account's master credential in one more durable, readable place —
    // and into every backup. It reaches the one container that needs it through a
    // namespace Secret instead (see ensure_tunnel_credentials).
    serde_json::json!({
        "config": config,
        "yolab": {
            "platformApiUrl": tunnel_cfg.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or(""),
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
fn helm_stream(
    args: Vec<String>,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
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
/// Puts the platform account token in the app's namespace as a Secret.
///
/// Created by local-api, not by the chart: a chart must not get to choose where its
/// credentials come from, or a hostile one could point the reference at a Secret it
/// controls. Only `yolab-common.wgRegisterInit` and the pre-delete hook reference it, so
/// the app's own containers never receive it in their environment.
///
/// This is a containment measure, not a fix. The token is still the whole account — it
/// can read the raw B2 credentials from /storage/s3, manage tunnels and DNS, and it
/// doubles as the x-yolab-cluster header that bypasses local-api auth entirely. Anything
/// that can read Secrets in the namespace can still reach it. The real fix is minting a
/// per-app credential scoped to "register one tunnel for one service", which needs a new
/// endpoint on yolab-external.
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
    .await
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
    .await
}

fn normalize_outputs(ann: &serde_json::Map<String, Value>) -> Vec<AppOutput> {
    let raw = ann.get(ANN_OUTPUTS).and_then(|v| v.as_str()).unwrap_or("");
    if raw.is_empty() {
        return vec![];
    }
    let Ok(outputs) = serde_json::from_str::<Vec<Value>>(raw) else {
        return vec![];
    };
    // Handle old format [{url, ipv6}]
    if outputs
        .first()
        .map(|o| o.get("url").is_some() || o.get("ipv6").is_some())
        .unwrap_or(false)
    {
        let mut result = vec![];
        for o in &outputs {
            if let Some(url) = o["url"].as_str().filter(|s| !s.is_empty()) {
                result.push(AppOutput {
                    key: "url".into(),
                    label: "Web URL".into(),
                    value: url.into(),
                    type_: "url".into(),
                });
            }
            if let Some(ip) = o["ipv6"].as_str().filter(|s| !s.is_empty()) {
                result.push(AppOutput {
                    key: "ipv6".into(),
                    label: "IPv6".into(),
                    value: ip.into(),
                    type_: "text".into(),
                });
            }
        }
        return result;
    }
    outputs
        .into_iter()
        .filter_map(|o| serde_json::from_value(o).ok())
        .collect()
}

/// Reject config scalars that could break out of a YAML scalar and inject
/// structure into the rendered manifest. Tera writes context string values
/// verbatim, so an embedded newline in e.g. a "domain" field could smuggle an
/// extra key/document into the applied manifest. All current catalog fields are
/// single-line scalars, so rejecting control characters has no false positives.
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
    Ok(Json(AccountTokenResponse {
        account_token: token,
    }))
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
    Ok(Json(DomainResponse {
        domain: derive_domain(dns_url),
    }))
}

/// The storefront: every chart across every configured source.
///
/// Sources are visited in resolution order (synced repos first, the bundled directory
/// last), and the first chart seen for a given id wins — so a published fix supersedes the
/// copy shipped in the system closure without anyone rebuilding the OS.
/// One chart's storefront entry. Shared by the full listing and the single-chart
/// refresh so the two can never drift into describing the same chart differently.
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

/// Re-pull one chart, then return its freshly-read catalog entry.
///
/// Called by the install page before it renders the form. The background sync is
/// hourly, so a chart published minutes ago still shows its previous schema —
/// and a field you just added is simply absent, which looks like a broken change
/// rather than a stale copy.
///
/// Failure is deliberately not an error: if the registry is unreachable, the
/// cached chart is still perfectly installable and the form should render from
/// it rather than refusing to open. The response says whether the refresh
/// actually happened so the UI can tell "current" from "possibly stale".
pub async fn refresh_catalog_app(
    State(state): State<AppState>,
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
            // Not in this repo, or this repo is unreachable — try the next one.
            Err(e) => note = e.to_string(),
        }
    }

    let catalog_dir = state.config.catalog_dir();
    let entry = crate::charts::chart_sources(&catalog_dir)
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

pub async fn catalog(State(state): State<AppState>) -> Json<Vec<CatalogApp>> {
    let catalog_dir = state.config.catalog_dir();
    let mut apps: Vec<CatalogApp> = vec![];
    let mut seen: std::collections::HashSet<String> = Default::default();

    for (repo, dir) in crate::charts::chart_sources(&catalog_dir).await {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Some(meta) = read_chart(&entry.path()) else {
                continue;
            };
            // An id can legitimately exist in several repos; the earlier source wins, and
            // the UI shows which repo it came from so "gitea from someone else's repo"
            // can never masquerade as the curated one.
            if !seen.insert(meta.chart.name.clone()) {
                continue;
            }
            apps.push(catalog_entry_from(repo.clone(), meta));
        }
    }
    // read_dir order is filesystem-dependent; sort so the storefront is stable.
    apps.sort_by_key(|a| a.name.to_lowercase());
    Json(apps)
}

// ── Chart repositories ────────────────────────────────────────────────────────

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
    // Sync immediately so the storefront reflects the new repo without a second call.
    let repos = crate::charts::list_repos().await;
    if let Some(r) = repos.iter().find(|r| r.name == body.name) {
        if let Err(e) = crate::charts::sync_repo(r).await {
            // The repo is registered but unusable — surface it rather than leaving an
            // empty section in the UI with no explanation.
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

/// Refreshes every repo. Also runs on a timer (see `run_chart_sync`) so a node picks up
/// newly published apps on its own.
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

/// True when a pod sitting in an app's namespace belongs to YoLab's backup
/// machinery rather than to the app.
///
/// VolSync schedules its movers INTO the namespace of the volume they copy, so
/// every app namespace temporarily grows a `volsync-src-…` pod during a backup and
/// a `volsync-dst-…` pod during a restore. They are ours. Counting them as the
/// app's own pods had two visible effects:
///
///   - the app card flipped to "Starting…" for the pull-and-init window of every
///     backup, and again between the mover finishing (phase Succeeded, so its Ready
///     condition is False) and Kubernetes garbage-collecting it;
///   - the app's pod list showed `volsync-src-volsync-filebrowser-data-4k46c`
///     alongside the app's own containers, which is an implementation detail with
///     no meaning to whoever is looking at it.
///
/// Neither is wrong about the pod. Both are wrong about whose pod it is.
///
/// Matched on the name prefix, which is deterministic and already relied on
/// elsewhere to find the mover for progress reporting, OR on VolSync's own label,
/// so a rename upstream does not silently reopen this.
pub(crate) fn is_backup_mover_pod(pod: &Value) -> bool {
    pod["metadata"]["name"]
        .as_str()
        .is_some_and(|n| n.starts_with("volsync-"))
        || pod["metadata"]["labels"]["app.kubernetes.io/created-by"].as_str() == Some("volsync")
}

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let catalog_dir = state.config.catalog_dir();
    let ns_selector = format!("{LABEL_MANAGED}=true");
    let ns_args = ["get", "namespaces", "-l", &ns_selector, "-o", "json"];
    let pod_args = ["get", "pods", "--all-namespaces", "-o", "json"];
    let (ns_out, pods_out) = tokio::join!(
        crate::kubectl::get_json(&ns_args),
        crate::kubectl::get_json(&pod_args),
    );
    let v: Value = ns_out?;

    // Build a pod-by-namespace index from the single bulk query so list_apps
    // requires only two kubectl calls regardless of app count.
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
        let status = if phase == "Terminating" {
            "uninstalling".to_string()
        } else {
            let ns_full = format!("yolab-{name}");
            // The app's own pods only — a backup running in this namespace must not
            // make the app look like it is restarting.
            let items: Vec<&Value> = pods_by_ns
                .get(ns_full.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter(|p| !is_backup_mover_pod(p))
                .copied()
                .collect();
            if items.is_empty() {
                "starting".to_string()
            } else {
                let all_ready = items.iter().all(|p| {
                    p["status"]["conditions"]
                        .as_array()
                        .map(|cs| {
                            cs.iter()
                                .any(|c| c["type"] == "Ready" && c["status"] == "True")
                        })
                        .unwrap_or(false)
                });
                if all_ready { "running" } else { "starting" }.to_string()
            }
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

        apps.push(AppInfo {
            app_id: id,
            instance_name: name,
            status,
            outputs: normalize_outputs(&ann),
            outputs_spec,
            config,
        });
    }
    Ok(Json(apps))
}

pub async fn install_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InstallRequest>,
) -> impl IntoResponse {
    if !body
        .instance_name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return (
            StatusCode::BAD_REQUEST,
            "instance_name must be lowercase alphanumeric and hyphens",
        )
            .into_response();
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
        let Some((repo, chart_dir)) = crate::charts::resolve_chart(&state.config.catalog_dir(), &id, None).await else {
            yield Ok(Event::default().data(format!("[ERROR] no chart named {id} in any configured repository")));
            return;
        };
        let Some(meta) = read_chart(&chart_dir) else {
            yield Ok(Event::default().data(format!("[ERROR] {id} is not a valid chart")));
            return;
        };

        let ns = format!("yolab-{}", body.instance_name);
        // Namespace first: the chart's resources are namespaced, and the labels/
        // annotations set here are what the backup layer selects on.
        yield Ok(Event::default().data("Preparing namespace..."));
        if let Err(e) = ensure_app_namespace(&ns, &id, &repo, &meta.chart.version).await {
            yield Ok(Event::default().data(format!("[ERROR] create namespace: {e}")));
            return;
        }
        // The one container that needs the account token reads it from here.
        if let Err(e) = ensure_tunnel_credentials(&ns, &tunnel_cfg).await {
            yield Ok(Event::default().data(format!("[ERROR] stage tunnel credentials: {e}")));
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
        // --dependency-update resolves the chart's declared dependencies if they are not
        // already vendored. Charts pulled from the registry arrive self-contained (the
        // packaged tarball includes charts/yolab-common), so this is a no-op for them; it
        // only does work for a chart resolved from the bundled source directory, whose
        // charts/ is a build artifact and therefore gitignored.
        //
        // This replaces a boot-time systemd unit that vendored every bundled chart up
        // front. That unit could not earn its keep once the library became an oci://
        // dependency: it needed the network to do its job, so it failed in exactly the
        // situation its fallback existed for, and listing the catalog never needed
        // dependencies at all — only rendering does.
        let args: Vec<String> = vec![
            "upgrade".into(), "--install".into(), "--dependency-update".into(),
            body.instance_name.clone(), chart_dir.to_string_lossy().to_string(),
            "-n".into(), ns.clone(),
            "--values".into(), tmp.path().to_string_lossy().to_string(),
        ];
        let s = helm_stream(args);
        tokio::pin!(s);
        use futures::StreamExt;
        while let Some(ev) = s.next().await { yield ev; }
        drop(tmp);

        // Wire up VolSync ReplicationSource(s) for any PVCs this app created. Best-effort:
        // the hourly replication-source reconciler self-heals a failure here within the
        // hour, but the person installing should still be told it didn't happen yet.
        if let Err(e) = crate::routers::backups::setup_namespace_backup(&ns).await {
            yield Ok(Event::default().data(format!(
                "[WARN] backup was not wired up for this app yet ({e}) — it will be picked up automatically within the hour"
            )));
        }
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
    let Ok(ns_v) = crate::kubectl::get_json(&["get", "namespace", &ns, "-o", "json"]).await else {
        return (StatusCode::NOT_FOUND, "Instance not found").into_response();
    };
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let id = ann
        .get(ANN_APP_ID)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stored_config: serde_json::Map<String, Value> = ann
        .get(ANN_CONFIG)
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
        let installed_repo = ann.get(ANN_CHART_REPO).and_then(|v| v.as_str()).map(String::from);
        let Some((_, chart_dir)) = crate::charts::resolve_chart(&state.config.catalog_dir(), &id, installed_repo.as_deref()).await else {
            yield Ok(Event::default().data(format!("[ERROR] chart {id} is no longer available in {:?}", installed_repo)));
            return;
        };
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
        // --dependency-update resolves the chart's declared dependencies if they are not
        // already vendored. Charts pulled from the registry arrive self-contained (the
        // packaged tarball includes charts/yolab-common), so this is a no-op for them; it
        // only does work for a chart resolved from the bundled source directory, whose
        // charts/ is a build artifact and therefore gitignored.
        //
        // This replaces a boot-time systemd unit that vendored every bundled chart up
        // front. That unit could not earn its keep once the library became an oci://
        // dependency: it needed the network to do its job, so it failed in exactly the
        // situation its fallback existed for, and listing the catalog never needed
        // dependencies at all — only rendering does.
        let args: Vec<String> = vec![
            "upgrade".into(), "--install".into(), "--dependency-update".into(),
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

    // Compile regex patterns once — recompiling inside the inner log-line loop
    // is O(patterns × lines) compilations which blows up on long logs.
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
            // Stop as soon as all keys are found.
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
    // Bounded: the pre-delete hook calls out to the platform to tear down the tunnel,
    // which must not be allowed to hang this request forever if that call stalls.
    const HELM_UNINSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let work = tokio::process::Command::new("helm")
        .args([
            "uninstall",
            &instance_name,
            "-n",
            &ns,
            "--ignore-not-found",
            "--wait",
        ])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(HELM_UNINSTALL_TIMEOUT, work).await;
    match out {
        Ok(Ok(o)) if !o.status.success() => {
            // Not fatal: the namespace delete below still tears the app down. But it
            // must be visible, because the thing that most commonly fails here is the
            // tunnel cleanup, which leaves an orphaned tunnel on the platform.
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

    crate::kubectl::run(&[
        "delete",
        "namespace",
        &ns,
        "--ignore-not-found=true",
        "--wait=false",
    ])
    .await?;

    Ok(Json(serde_json::json!({"ok": true})))
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

pub async fn describe_pod(
    Path((instance_name, pod_name)): Path<(String, String)>,
) -> Result<Json<DescribeResponse>> {
    // Not routed through crate::kubectl::run: unlike every other caller, this handler
    // wants kubectl's combined stdout+stderr verbatim even on a nonzero exit (e.g. "pod
    // not found" is itself the useful description), not an error. Still bounded and
    // kill_on_drop for the same reason every other call site is.
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("kubectl")
            .args([
                "describe",
                "pod",
                &pod_name,
                "-n",
                &format!("yolab-{instance_name}"),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("kubectl describe pod: timed out"))??;
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
        let Ok(c) = child else {
            yield Ok(Event::default().data("[yolab] could not run kubectl to read the logs"));
            return;
        };
        let mut guard = KillOnDrop(c);
        use tokio::io::AsyncBufReadExt;
        let stdout = guard.0.stdout.take().unwrap();
        // stderr was piped and then never read, so every reason kubectl declines to
        // show logs — a container still initialising, a pod that has gone away, a name
        // that no longer exists — arrived as an empty stream and an empty panel. The
        // explanation existed; nothing carried it to the person reading.
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
                    // stderr closing is normal and says nothing about stdout.
                    _ => continue,
                },
            }
        }
        // Drain whatever kubectl said on its way out, so a failure that only appears
        // at exit is still reported rather than swallowed by the loop ending.
        while let Ok(Some(l)) = err.next_line().await {
            yield Ok(Event::default().data(format!("[yolab] {l}")));
        }
        let _ = guard.0.wait().await;
    };
    Sse::new(stream)
}

#[cfg(test)]
mod tests {

    // ── is_backup_mover_pod ───────────────────────────────────────────────────
    //
    // VolSync runs its movers inside the namespace of the volume they copy, so an
    // app namespace grows one during every backup. Treating it as the app's own pod
    // made the app card flash "Starting…" on a backup and listed the mover next to
    // the app's containers.

    fn pod(name: &str) -> Value {
        json!({"metadata": {"name": name}})
    }

    #[test]
    fn a_backup_mover_is_not_one_of_the_apps_pods() {
        // The exact name observed live in yolab-filebrowser during a backup.
        assert!(is_backup_mover_pod(&pod(
            "volsync-src-volsync-filebrowser-data-4k46c"
        )));
        // And the restore-side mover, which appears while an app is scaled to zero.
        assert!(is_backup_mover_pod(&pod(
            "volsync-dst-volsync-vaultwarden-data-abc12"
        )));
    }

    /// The label is a second, independent signal so an upstream rename of the pod
    /// prefix does not silently put the movers back on the app card.
    #[test]
    fn the_volsync_label_is_enough_on_its_own() {
        assert!(is_backup_mover_pod(&json!({
            "metadata": {
                "name": "some-future-mover-name",
                "labels": {"app.kubernetes.io/created-by": "volsync"}
            }
        })));
    }

    /// The app's own pods must survive the filter, including ones whose names merely
    /// mention sync or backup.
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

    /// A pod with no labels at all must not panic the label check.
    #[test]
    fn a_pod_without_labels_is_handled() {
        assert!(!is_backup_mover_pod(
            &json!({"metadata": {"name": "app-1"}})
        ));
        assert!(!is_backup_mover_pod(&json!({})));
    }

    // ── resolve_service_name ──────────────────────────────────────────────────
    //
    // This decides whether an app gets a DNS record at all. When it returns "",
    // wg-register skips registration, writes an empty YOLAB_FQDN, and the
    // generated Caddyfile collapses to a bare `{` — Caddy then fails with
    // "unrecognized global option: reverse_proxy", which names neither the
    // subdomain nor the schema. Every app install was broken this way.

    /// The real shape a chart's values.schema.json has: the tunnel field sits
    /// under properties.config.properties, not at the top level.
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

    /// The exact regression: top-level properties are `config` and `yolab`,
    /// neither of which carries format:tunnel, so a top-level-only search
    /// silently yields "".
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

    /// A flat schema must keep working.
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

    /// Declared in the schema but absent from the user's answers: still empty,
    /// but must not panic.
    /// The path that actually broke installs: the UI submitted `config: {}`, so
    /// there is no answer at all. The schema declares a default precisely for
    /// this, and using it is what keeps the install working instead of silently
    /// producing an app with no DNS name.
    #[test]
    fn a_missing_answer_falls_back_to_the_schema_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[])),
            "qbittorrent"
        );
    }

    /// An explicitly empty string is as absent as a missing key — it must not
    /// win over the default, or it reintroduces the blank-FQDN failure.
    #[test]
    fn an_empty_answer_also_falls_back_to_the_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[("subdomain", "")])),
            "qbittorrent"
        );
    }

    /// A real answer still wins over the default.
    #[test]
    fn an_explicit_answer_beats_the_default() {
        assert_eq!(
            resolve_service_name(&real_schema(), &cfg(&[("subdomain", "torrents")])),
            "torrents"
        );
    }

    /// No answer AND no default: still empty, and still no panic.
    #[test]
    fn no_answer_and_no_default_yields_empty() {
        let schema = serde_json::json!({
            "properties": {"config": {"properties": {"subdomain": {"format": "tunnel"}}}}
        });
        assert_eq!(resolve_service_name(&schema, &cfg(&[])), "");
    }

    /// A non-string answer is unusable, so it falls back to the default rather
    /// than yielding "". Returning empty here would mean no DNS record and a
    /// gateway that crash-loops on a blank FQDN — a worse outcome than using
    /// the subdomain the schema nominated.
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
    fn derive_domain_drops_subdomain() {
        assert_eq!(
            derive_domain("https://yolab.10.demycode.ovh"),
            "10.demycode.ovh"
        );
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
