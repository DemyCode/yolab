use std::{convert::Infallible, path::PathBuf, sync::Arc};

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
const ANN_APP_ID: &str = "yolab.io/app-id";
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

fn tunnel_config(cfg: &Config) -> anyhow::Result<toml::Table> {
    let text = std::fs::read_to_string(&cfg.config_path)?;
    let table: toml::Table = toml::from_str(&text)?;
    table["tunnel"]
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))
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

fn render_manifest(
    catalog_dir: &PathBuf,
    id: &str,
    instance_name: &str,
    config: &serde_json::Map<String, Value>,
    tunnel_cfg: &toml::Table,
    template_file: &str,
    extra_vars: Option<&serde_json::Map<String, Value>>,
) -> anyhow::Result<String> {
    let app_dir = catalog_dir.join(id);
    let schema_path = app_dir.join("schema.json");
    let schema: Value = if schema_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&schema_path)?)?
    } else {
        Value::Null
    };

    // Find tunnel field from schema
    let tunnel_field = schema["properties"]
        .as_object()
        .and_then(|props| {
            props.iter().find_map(|(k, v)| {
                if v["format"].as_str() == Some("tunnel") { Some(k.clone()) } else { None }
            })
        });
    let service_name = tunnel_field
        .as_ref()
        .and_then(|f| config.get(f))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let template_text = std::fs::read_to_string(app_dir.join(template_file))?;
    let mut tera = tera::Tera::default();
    tera.add_raw_template("t", &template_text)?;

    let mut ctx = tera::Context::new();
    ctx.insert("instance_name", instance_name);
    ctx.insert("app_id", id);
    ctx.insert("platform_api_url", tunnel_cfg.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or(""));
    ctx.insert("account_token", tunnel_cfg.get("account_token").and_then(|v| v.as_str()).unwrap_or(""));
    ctx.insert("service_name", service_name);
    for (k, v) in config {
        ctx.insert(k, v);
    }
    if let Some(extra) = extra_vars {
        for (k, v) in extra {
            ctx.insert(k, v);
        }
    }

    Ok(tera.render("t", &ctx)?)
}

async fn apply_manifest_stream(
    rendered: &str,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
    std::fs::write(tmp.path(), rendered).unwrap();
    let path = tmp.path().to_string_lossy().to_string();

    async_stream::stream! {
        let _tmp = tmp;
        let child = tokio::process::Command::new("kubectl")
            .args(["apply", "-f", &path])
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
        let rc = guard.0.wait().await.map(|s| s.code().unwrap_or(1)).unwrap_or(1);
        if rc != 0 {
            yield Ok(Event::default().data(format!("[ERROR] kubectl apply failed (exit {rc})")));
        }
    }
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub async fn tunnel_domain(State(state): State<AppState>) -> Result<Json<DomainResponse>> {
    let tunnel = tunnel_config(&state.config)?;
    let dns_url = tunnel.get("dns_url").and_then(|v| v.as_str()).unwrap_or("");
    let host = dns_url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    let parts: Vec<&str> = host.split('.').collect();
    let domain = if parts.len() > 1 && !parts[0].chars().all(|c| c.is_ascii_digit()) {
        parts[1..].join(".")
    } else {
        host.to_string()
    };
    Ok(Json(DomainResponse { domain }))
}

pub async fn catalog(State(state): State<AppState>) -> Json<Vec<CatalogApp>> {
    let catalog_dir = state.config.catalog_dir();
    let Ok(rd) = std::fs::read_dir(&catalog_dir) else { return Json(vec![]) };
    let mut apps = vec![];
    for entry in rd.flatten() {
        let app_dir = entry.path();
        let toml_path = app_dir.join("app.toml");
        if !toml_path.exists() { continue; }
        let Ok(text) = std::fs::read_to_string(&toml_path) else { continue };
        let Ok(table) = toml::from_str::<toml::Table>(&text) else { continue };
        let Some(meta) = table.get("app").and_then(|v| v.as_table()) else { continue };
        let schema = app_dir.join("schema.json");
        let uischema = app_dir.join("uischema.json");
        apps.push(CatalogApp {
            id: meta.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            name: meta.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            description: meta.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            icon: meta.get("icon").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            category: meta.get("category").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            schema: schema.exists().then(|| serde_json::from_str(&std::fs::read_to_string(&schema).unwrap_or_default()).unwrap_or(Value::Null)).unwrap_or(Value::Null),
            uischema: uischema.exists().then(|| serde_json::from_str(&std::fs::read_to_string(&uischema).unwrap_or_default()).unwrap_or(Value::Null)).unwrap_or(Value::Null),
        });
    }
    Json(apps)
}

pub async fn list_apps(State(state): State<AppState>) -> Result<Json<Vec<AppInfo>>> {
    let catalog_dir = state.config.catalog_dir();
    let out = tokio::process::Command::new("kubectl")
        .args(["get", "namespaces", "-l", &format!("{LABEL_MANAGED}=true"), "-o", "json"])
        .output().await?;
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let mut apps = vec![];
    for ns in v["items"].as_array().unwrap_or(&vec![]) {
        let ann = ns["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
        let name = ns["metadata"]["name"].as_str().unwrap_or("").trim_start_matches("yolab-").to_string();
        let phase = ns["status"]["phase"].as_str().unwrap_or("Active");
        let status = if phase == "Terminating" {
            "uninstalling".to_string()
        } else {
            let pods = tokio::process::Command::new("kubectl")
                .args(["get", "pods", "-n", &format!("yolab-{name}"), "-o", "json"])
                .output().await.ok();
            let status = pods.and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .and_then(|v| v["items"].as_array().cloned())
                .map(|items| {
                    if items.is_empty() { return "starting".to_string(); }
                    let all_ready = items.iter().all(|p| {
                        p["status"]["conditions"].as_array()
                            .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
                            .unwrap_or(false)
                    });
                    if all_ready { "running" } else { "starting" }.to_string()
                })
                .unwrap_or_else(|| "starting".to_string());
            status
        };

        let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let config: serde_json::Map<String, Value> = ann.get(ANN_CONFIG)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let outputs_spec_path = if !id.is_empty() { Some(catalog_dir.join(&id).join("outputs.json")) } else { None };
        let outputs_spec = outputs_spec_path
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
            .unwrap_or_default()
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
    if !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::NOT_FOUND, format!("App '{id}' not found")).into_response();
    }

    let stream = async_stream::stream! {
        let Ok(tunnel_cfg) = tunnel_config(&state.config) else {
            yield Ok(Event::default().data("[ERROR] could not read tunnel config"));
            return;
        };
        yield Ok(Event::default().data("Rendering manifest..."));
        let Ok(rendered) = render_manifest(&state.config.catalog_dir(), &id, &body.instance_name, &body.config, &tunnel_cfg, "manifest.yaml.j2", None) else {
            yield Ok(Event::default().data("[ERROR] template render failed"));
            return;
        };
        yield Ok(Event::default().data("Applying manifests to cluster..."));
        let apply_stream = apply_manifest_stream(&rendered).await;
        tokio::pin!(apply_stream);
        use futures::StreamExt;
        while let Some(ev) = apply_stream.next().await { yield ev; }
        let _ = tokio::process::Command::new("kubectl")
            .args(["annotate", "namespace", &format!("yolab-{}", body.instance_name),
                   &format!("{ANN_CONFIG}={}", serde_json::to_string(&body.config).unwrap()),
                   "--overwrite=true"])
            .output().await;
        yield Ok(Event::default().data(format!("[DONE] {id} installed — run 'Scan outputs' once the pod is ready")));
    };

    Sse::new(stream).into_response()
}

pub async fn update_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
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
    let config: serde_json::Map<String, Value> = ann.get(ANN_CONFIG)
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if id.is_empty() || !state.config.catalog_dir().join(&id).exists() {
        return (StatusCode::BAD_REQUEST, "App not found in catalog").into_response();
    }

    let stream = async_stream::stream! {
        let Ok(tunnel_cfg) = tunnel_config(&state.config) else {
            yield Ok(Event::default().data("[ERROR] could not read tunnel config"));
            return;
        };
        yield Ok(Event::default().data("Rendering manifest..."));
        let Ok(rendered) = render_manifest(&state.config.catalog_dir(), &id, &instance_name, &config, &tunnel_cfg, "manifest.yaml.j2", None) else {
            yield Ok(Event::default().data("[ERROR] template render failed"));
            return;
        };
        yield Ok(Event::default().data("Applying updated manifests..."));
        let apply_stream = apply_manifest_stream(&rendered).await;
        tokio::pin!(apply_stream);
        use futures::StreamExt;
        while let Some(ev) = apply_stream.next().await { yield ev; }
        yield Ok(Event::default().data("Restarting deployments..."));
        let child = tokio::process::Command::new("kubectl")
            .args(["rollout", "restart", "deployment", "-n", &ns])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        if let Ok(c) = child {
            let mut guard = KillOnDrop(c);
            use tokio::io::AsyncBufReadExt;
            if let Some(stdout) = guard.0.stdout.take() {
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    yield Ok(Event::default().data(l));
                }
            }
        }
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
    let outputs_json = state.config.catalog_dir().join(&id).join("outputs.json");
    if !outputs_json.exists() {
        return Ok(Json(ScanOutputsResponse { outputs: normalize_outputs(&ann) }));
    }
    let outputs_spec: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&outputs_json)?)?;

    let pods_out = tokio::process::Command::new("kubectl")
        .args(["get", "pods", "-n", &ns, "-o", "json"])
        .output().await?;
    let pods_v: Value = serde_json::from_slice(&pods_out.stdout)?;
    let mut found: std::collections::HashMap<String, String> = Default::default();

    for pod in pods_v["items"].as_array().unwrap_or(&vec![]) {
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
                for spec in &outputs_spec {
                    let key = spec["key"].as_str().unwrap_or("");
                    if found.contains_key(key) { continue; }
                    if let Some(pattern) = spec["pattern"].as_str() {
                        if let Ok(re) = regex::Regex::new(pattern) {
                            if let Some(cap) = re.captures(line).and_then(|c| c.get(1)) {
                                found.insert(key.to_string(), cap.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    if found.is_empty() {
        return Ok(Json(ScanOutputsResponse { outputs: normalize_outputs(&ann) }));
    }

    let outputs: Vec<AppOutput> = outputs_spec.iter()
        .filter_map(|spec| {
            let key = spec["key"].as_str()?;
            let value = found.get(key)?.clone();
            Some(AppOutput {
                key: key.to_string(),
                label: spec.get("label").and_then(|v| v.as_str()).unwrap_or(key).to_string(),
                value,
                type_: spec.get("type").and_then(|v| v.as_str()).unwrap_or("text").to_string(),
            })
        })
        .collect();

    let _ = tokio::process::Command::new("kubectl")
        .args(["annotate", "namespace", &ns,
               &format!("{ANN_OUTPUTS}={}", serde_json::to_string(&outputs).unwrap()),
               "--overwrite=true"])
        .output().await;

    Ok(Json(ScanOutputsResponse { outputs }))
}

pub async fn uninstall_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let ns = format!("yolab-{instance_name}");
    if let Ok(out) = tokio::process::Command::new("kubectl")
        .args(["get", "namespace", &ns, "-o", "json", "--ignore-not-found=true"])
        .output().await
    {
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            let ann = v["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
            let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let uninstall_j2 = state.config.catalog_dir().join(&id).join("uninstall.yaml.j2");
            if !id.is_empty() && uninstall_j2.exists() {
                let config: serde_json::Map<String, Value> = ann.get(ANN_CONFIG)
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let outputs = normalize_outputs(&ann);
                let mut extra: serde_json::Map<String, Value> = Default::default();
                for o in &outputs {
                    extra.insert(format!("output_{}", o.key), Value::String(o.value.clone()));
                }
                if let Ok(tunnel_cfg) = tunnel_config(&state.config) {
                    if let Ok(rendered) = render_manifest(
                        &state.config.catalog_dir(), &id, &instance_name,
                        &config, &tunnel_cfg, "uninstall.yaml.j2", Some(&extra),
                    ) {
                        let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
                        std::fs::write(tmp.path(), &rendered).ok();
                        let _ = tokio::process::Command::new("kubectl")
                            .args(["apply", "-f", &tmp.path().to_string_lossy()])
                            .output().await;
                        let _ = tokio::process::Command::new("kubectl")
                            .args(["wait", "job/uninstall", "-n", &ns,
                                   "--for=condition=complete", "--timeout=120s"])
                            .output().await;
                    }
                }
            }
        }
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
