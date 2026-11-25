use axum::{
    extract::Path as AxPath, extract::State, http::StatusCode, response::IntoResponse, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::error::{discard_dir, remove_dir_if_present};
use crate::host::Host;
use crate::AppState;

pub const CUSTOM_REPO: &str = "custom";

fn custom_dir() -> PathBuf {
    crate::charts::cache_root().join(CUSTOM_REPO)
}

const CLUSTER_SCOPED: &[&str] = &[
    "Namespace",
    "Node",
    "PersistentVolume",
    "ClusterRole",
    "ClusterRoleBinding",
    "CustomResourceDefinition",
    "StorageClass",
    "MutatingWebhookConfiguration",
    "ValidatingWebhookConfiguration",
    "APIService",
    "PriorityClass",
    "IngressClass",
    "CSIDriver",
    "ValidatingAdmissionPolicy",
    "ValidatingAdmissionPolicyBinding",
];

#[derive(Debug, PartialEq)]
pub struct Rejection {
    pub reason: String,
}

fn reject(reason: impl Into<String>) -> Rejection {
    Rejection {
        reason: reason.into(),
    }
}

const WG_SIDECAR_CONTAINER: &str = "wireguard";
const WG_SIDECAR_IMAGE_PREFIX: &str = "ghcr.io/demycode/wg-sidecar:";

fn is_tunnel_sidecar(container: &Value) -> bool {
    container["name"].as_str() == Some(WG_SIDECAR_CONTAINER)
        && container["image"]
            .as_str()
            .is_some_and(|i| i.starts_with(WG_SIDECAR_IMAGE_PREFIX))
}

fn scan_for_escapes(node: &Value, allow_sidecar: bool, out: &mut Vec<String>) {
    scan_inner(node, false, allow_sidecar, out)
}

fn scan_inner(node: &Value, privileged_ok: bool, allow_sidecar: bool, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            for (k, v) in map {
                match k.as_str() {
                    "hostNetwork" | "hostPID" | "hostIPC" if v == &Value::Bool(true) => {
                        out.push(format!("{k}: true shares the machine's own namespace"));
                    }
                    "hostPath" => {
                        out.push("hostPath mounts a directory from the machine itself".into());
                    }
                    "hostPort" => {
                        out.push("hostPort binds a port on the machine itself".into());
                    }
                    "privileged" if v == &Value::Bool(true) && !privileged_ok => {
                        out.push("privileged: true removes the container boundary".into());
                    }
                    "nodeName" => {
                        out.push("nodeName pins this to one machine by name".into());
                    }
                    _ => {}
                }
                if matches!(
                    k.as_str(),
                    "containers" | "initContainers" | "ephemeralContainers"
                ) {
                    if let Value::Array(items) = v {
                        for item in items {
                            scan_inner(
                                item,
                                allow_sidecar && is_tunnel_sidecar(item),
                                allow_sidecar,
                                out,
                            );
                        }
                        continue;
                    }
                }
                scan_inner(v, privileged_ok, allow_sidecar, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                scan_inner(v, privileged_ok, allow_sidecar, out);
            }
        }
        _ => {}
    }
}

pub fn validate_manifest(yaml: &str) -> Result<usize, Rejection> {
    if yaml.trim().is_empty() {
        return Err(reject("there is nothing here to install"));
    }
    if yaml.len() > 512 * 1024 {
        return Err(reject("that manifest is larger than 512 KB"));
    }

    let mut count = 0usize;
    for doc in serde_norway::Deserializer::from_str(yaml) {
        let v: Value = match serde::Deserialize::deserialize(doc) {
            Ok(v) => v,
            Err(e) => return Err(reject(format!("this is not valid YAML: {e}"))),
        };
        if v.is_null() {
            continue;
        }
        let Some(kind) = v["kind"].as_str() else {
            return Err(reject("every document needs a `kind` — this looks like Kubernetes YAML is missing, or a Docker Compose file was pasted instead"));
        };
        if v["apiVersion"].as_str().is_none() {
            return Err(reject(format!(
                "the {kind} document is missing `apiVersion`"
            )));
        }
        if CLUSTER_SCOPED.contains(&kind) {
            return Err(reject(format!(
                "a {kind} is not scoped to one app — it would apply to the whole cluster, so it cannot be installed as an app"
            )));
        }
        if let Some(ns) = v["metadata"]["namespace"].as_str() {
            return Err(reject(format!(
                "remove `namespace: {ns}` — YoLab puts this app in its own namespace, and a manifest that names another one would install somewhere it does not own"
            )));
        }
        let mut escapes = Vec::new();
        scan_for_escapes(&v, false, &mut escapes);
        if let Some(first) = escapes.first() {
            return Err(reject(format!(
                "the {kind} document is not allowed: {first}"
            )));
        }
        count += 1;
    }

    if count == 0 {
        return Err(reject("there is nothing here to install"));
    }
    Ok(count)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomApp {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub service: String,
}

#[derive(Deserialize)]
pub struct SaveCustomReq {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    pub yaml: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub service: String,
}

const CUSTOM_LIBRARY_VERSION: &str = "0.1.43";

const CHART_TEMPLATE: &str = r#"{{ include "yolab-common.caddyConfigMap" . }}
---
{{ include "yolab-common.uninstallHook" . }}
---
{{ include "yolab-common.homeVolume" . }}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: gateway
  namespace: {{ .Release.Namespace }}
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: gateway
  template:
    metadata:
      labels:
        app: gateway
    spec:
      initContainers:
      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
"#;

const USER_TEMPLATE: &str = "{{ .Files.Get \"user-manifest.yaml\" }}\n";

async fn write_chart_at(root: &std::path::Path, app: &CustomApp, yaml: &str) -> anyhow::Result<()> {
    let dir = root.join(&app.id);
    remove_dir_if_present(&dir).await?;

    let upstream = match (app.port, app.service.as_str()) {
        (Some(p), "") => format!("localhost:{p}"),
        (Some(p), svc) => format!("{svc}:{p}"),
        (None, _) => String::new(),
    };

    let chart_yaml = format!(
        "apiVersion: v2\nname: {id}\ndescription: {desc}\ntype: application\nversion: 0.1.0\n\ndependencies:\n  - name: yolab-common\n    version: \"{library}\"\n    repository: \"oci://ghcr.io/demycode/charts\"\n\nannotations:\n  yolab.io/display-name: {display}\n  yolab.io/icon: {icon}\n  yolab.io/category: \"custom\"\n",
        library = CUSTOM_LIBRARY_VERSION,
        id = app.id,
        desc = Value::from(app.description.as_str()),
        display = Value::from(app.display_name.as_str()),
        icon = Value::from(app.icon.as_str()),
    );

    let values = format!(
        "config:\n  subdomain: {id}\nyolab:\n  platformApiUrl: \"\"\n  accountToken: \"\"\n  serviceName: \"\"\n  gateway:\n    upstream: {upstream}\n",
        id = app.id,
        upstream = if upstream.is_empty() { "\"\"".to_string() } else { upstream.clone() },
    );

    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["config"],
        "properties": {
            "config": {
                "title": app.display_name,
                "type": "object",
                "required": ["subdomain"],
                "properties": {
                    "subdomain": {
                        "type": "string", "title": "Subdomain",
                        "format": "tunnel", "default": app.id
                    }
                }
            }
        }
    });

    let mut files = vec![
        ("Chart.yaml", chart_yaml),
        ("values.yaml", values),
        ("values.schema.json", format!("{schema:#}")),
        ("templates/user.yaml", USER_TEMPLATE.to_string()),
        ("user-manifest.yaml", yaml.to_string()),
        ("yolab-custom.json", serde_json::json!(app).to_string()),
    ];
    if !upstream.is_empty() {
        files.push(("templates/gateway.yaml", CHART_TEMPLATE.to_string()));
    }
    std::fs::create_dir_all(dir.join("templates")).and_then(|()| {
        files
            .iter()
            .try_for_each(|(name, body)| std::fs::write(dir.join(name), body))
    })?;
    Ok(())
}

pub async fn list_custom() -> Json<Vec<CustomApp>> {
    let mut out = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(custom_dir()).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let meta = entry.path().join("yolab-custom.json");
            if let Ok(text) = tokio::fs::read_to_string(&meta).await {
                if let Ok(app) = serde_json::from_str::<CustomApp>(&text) {
                    out.push(app);
                }
            }
        }
    }
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(out)
}

pub async fn save_custom(
    State(_s): State<AppState>,
    Json(req): Json<SaveCustomReq>,
) -> impl IntoResponse {
    if !valid_id(&req.id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "the id may use lowercase letters, digits and hyphens, and cannot start or end with a hyphen"}),
            ),
        );
    }
    if req.display_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "give it a name people will read"})),
        );
    }
    let docs = match validate_manifest(&req.yaml) {
        Ok(n) => n,
        Err(r) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": r.reason})),
            )
        }
    };
    let app = CustomApp {
        id: req.id,
        display_name: req.display_name,
        icon: req.icon,
        description: req.description,
        port: req.port,
        service: req.service,
    };
    if let Err(e) = write_chart_at(&custom_dir(), &app, &req.yaml).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "documents": docs, "app": app})),
    )
}

pub async fn delete_custom(AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !valid_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "unknown app"})),
        );
    }
    match tokio::fs::remove_dir_all(custom_dir().join(&id)).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[derive(Debug, PartialEq)]
enum Archive {
    Zip,
    TarGz,
}

fn sniff(bytes: &[u8]) -> Option<Archive> {
    match bytes {
        [0x50, 0x4B, 0x03, 0x04, ..] | [0x50, 0x4B, 0x05, 0x06, ..] => Some(Archive::Zip),
        [0x1F, 0x8B, ..] => Some(Archive::TarGz),
        _ => None,
    }
}

fn find_chart_root(base: &std::path::Path, depth: usize) -> Option<PathBuf> {
    if base.join("Chart.yaml").is_file() {
        return Some(base.to_path_buf());
    }
    if depth == 0 {
        return None;
    }
    let mut found = None;
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        if !e.path().is_dir() {
            continue;
        }
        if e.file_name().to_string_lossy().starts_with("__") {
            continue;
        }
        if let Some(hit) = find_chart_root(&e.path(), depth - 1) {
            if found.is_some() {
                return None;
            }
            found = Some(hit);
        }
    }
    found
}

#[derive(Debug, Deserialize)]
struct ChartYaml {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    annotations: std::collections::HashMap<String, String>,
    #[serde(default)]
    dependencies: Vec<ChartDep>,
}

#[derive(Debug, Deserialize)]
struct ChartDep {
    name: String,
}

fn shipped(charts_dir: &std::path::Path, name: &str) -> bool {
    charts_dir.join(name).join("Chart.yaml").is_file()
        || std::fs::read_dir(charts_dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let file = e.file_name().to_string_lossy().to_string();
                file.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .is_some_and(|rest| rest.ends_with(".tgz"))
            })
        })
}

fn vendored(
    meta: &ChartYaml,
    catalog_dir: &std::path::Path,
    root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let charts_dir = root.join("charts");
    meta.dependencies
        .iter()
        .filter(|d| valid_id(&d.name))
        .filter(|d| {
            crate::routers::apps::LIBRARIES.contains(&d.name.as_str())
                || !shipped(&charts_dir, &d.name)
        })
        .map(|d| catalog_dir.join(&d.name))
        .filter(|dir| dir.join("Chart.yaml").is_file())
        .collect()
}

fn is_own_tree_folder(v: &Value, release_ns: &str) -> bool {
    v["kind"].as_str() == Some("PersistentVolume")
        && v["spec"]["claimRef"]["namespace"].as_str() == Some(release_ns)
        && crate::tree::folder_of(v).is_some()
}

fn validate_rendered(rendered: &str, release_ns: &str) -> Result<(), Rejection> {
    for doc in serde_norway::Deserializer::from_str(rendered) {
        let v: Value = match serde::Deserialize::deserialize(doc) {
            Ok(v) => v,
            Err(e) => {
                return Err(reject(format!(
                    "the chart rendered something that is not valid YAML: {e}"
                )))
            }
        };
        if v.is_null() {
            continue;
        }
        let kind = v["kind"].as_str().unwrap_or("object");
        if CLUSTER_SCOPED.contains(&kind) && !is_own_tree_folder(&v, release_ns) {
            return Err(reject(format!(
                "this chart creates a {kind}, which applies to the whole cluster rather than to one app"
            )));
        }
        if let Some(ns) = v["metadata"]["namespace"].as_str() {
            if ns != release_ns {
                return Err(reject(format!(
                    "this chart puts a {kind} in namespace \"{ns}\", which is not its own"
                )));
            }
        }
        let mut escapes = Vec::new();
        scan_for_escapes(&v, true, &mut escapes);
        if let Some(first) = escapes.first() {
            return Err(reject(format!(
                "this chart's {kind} is not allowed: {first}"
            )));
        }
    }
    Ok(())
}

const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

async fn run<H: Host>(host: &H, cmd: &str, args: &[&str]) -> Result<String, Rejection> {
    let out = host
        .run_cmd_bounded(cmd, args, RUN_TIMEOUT)
        .await
        .map_err(|e| reject(format!("could not run {cmd}: {e}")))?;
    if !out.success {
        return Err(reject(out.stderr.trim().to_string()));
    }
    Ok(out.stdout)
}

pub async fn upload_chart(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    upload(
        &state.host,
        &state.config.catalog_dir(),
        &custom_dir(),
        &body,
    )
    .await
}

async fn upload<H: Host>(
    host: &H,
    catalog_dir: &std::path::Path,
    custom_root: &std::path::Path,
    body: &[u8],
) -> (StatusCode, Json<Value>) {
    upload_in(host, catalog_dir, custom_root, body, &std::env::temp_dir()).await
}

async fn upload_in<H: Host>(
    host: &H,
    catalog_dir: &std::path::Path,
    custom_root: &std::path::Path,
    body: &[u8],
    scratch: &std::path::Path,
) -> (StatusCode, Json<Value>) {
    let bad = |msg: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        )
    };
    let internal = |msg: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": msg })),
        )
    };

    let Some(kind) = sniff(body) else {
        return bad("that file is not a .zip or a .tgz — package a chart with `helm package`, or zip the chart folder".into());
    };

    let tmp = scratch.join(format!(
        "yolab-chart-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    discard_dir(&tmp).await;
    let archive = tmp.join("upload");
    let dest = tmp.join("x");
    if let Err(e) = std::fs::create_dir_all(&dest).and_then(|()| std::fs::write(&archive, body)) {
        discard_dir(&tmp).await;
        return internal(format!("could not make a folder to unpack into: {e}"));
    }
    let extracted = match kind {
        Archive::TarGz => {
            run(
                host,
                "tar",
                &[
                    "-xzf",
                    archive.to_str().unwrap_or(""),
                    "-C",
                    dest.to_str().unwrap_or(""),
                    "--no-same-owner",
                ],
            )
            .await
        }
        Archive::Zip => {
            run(
                host,
                "unzip",
                &[
                    "-q",
                    "-o",
                    archive.to_str().unwrap_or(""),
                    "-d",
                    dest.to_str().unwrap_or(""),
                ],
            )
            .await
        }
    };
    if let Err(r) = extracted {
        discard_dir(&tmp).await;
        return bad(format!("could not unpack that archive: {}", r.reason));
    }

    let Some(root) = find_chart_root(&dest, 3) else {
        discard_dir(&tmp).await;
        return bad("no Chart.yaml in there — this needs to be a Helm chart. If the archive holds two charts, upload them separately.".into());
    };

    let chart_text = match tokio::fs::read_to_string(root.join("Chart.yaml")).await {
        Ok(t) => t,
        Err(e) => {
            discard_dir(&tmp).await;
            return bad(format!("Chart.yaml could not be read: {e}"));
        }
    };
    let meta: ChartYaml = match serde_norway::from_str(&chart_text) {
        Ok(m) => m,
        Err(e) => {
            discard_dir(&tmp).await;
            return bad(format!("Chart.yaml is not valid: {e}"));
        }
    };
    if !valid_id(&meta.name) {
        discard_dir(&tmp).await;
        return bad(format!(
            "\"{}\" cannot be used as an app id — lowercase letters, digits and hyphens only",
            meta.name
        ));
    }

    for lib in vendored(&meta, catalog_dir, &root) {
        let charts_dir = root.join("charts");
        if let Err(e) = tokio::fs::create_dir_all(&charts_dir).await {
            discard_dir(&tmp).await;
            return internal(format!("could not make the chart's charts folder: {e}"));
        }
        if let Err(r) = run(
            host,
            "cp",
            &[
                "-r",
                lib.to_str().unwrap_or(""),
                charts_dir.to_str().unwrap_or(""),
            ],
        )
        .await
        {
            discard_dir(&tmp).await;
            return internal(format!(
                "could not add {} to the chart: {}",
                lib.display(),
                r.reason
            ));
        }
    }

    let release_ns = format!("yolab-{}", meta.name);
    let rendered = match run(
        host,
        "helm",
        &[
            "template",
            &meta.name,
            root.to_str().unwrap_or(""),
            "--namespace",
            &release_ns,
        ],
    )
    .await
    {
        Ok(r) => r,
        Err(r) => {
            discard_dir(&tmp).await;
            return bad(format!("this chart does not render: {}", r.reason));
        }
    };
    if let Err(r) = validate_rendered(&rendered, &release_ns) {
        discard_dir(&tmp).await;
        return bad(r.reason);
    }

    let final_dir = custom_root.join(&meta.name);
    if let Err(e) = tokio::fs::create_dir_all(custom_root).await {
        discard_dir(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        );
    }
    if let Err(e) = remove_dir_if_present(&final_dir).await {
        discard_dir(&tmp).await;
        return internal(format!(
            "could not remove the previous copy of this chart: {e}"
        ));
    }
    if let Err(r) = run(
        host,
        "cp",
        &[
            "-r",
            root.to_str().unwrap_or(""),
            final_dir.to_str().unwrap_or(""),
        ],
    )
    .await
    {
        discard_dir(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": r.reason })),
        );
    }

    let app = CustomApp {
        id: meta.name.clone(),
        display_name: meta
            .annotations
            .get("yolab.io/display-name")
            .cloned()
            .unwrap_or_else(|| meta.name.clone()),
        icon: meta
            .annotations
            .get("yolab.io/icon")
            .cloned()
            .unwrap_or_default(),
        description: meta.description.clone(),
        port: None,
        service: String::new(),
    };
    let details = serde_json::json!(app).to_string();
    if let Err(e) = tokio::fs::write(final_dir.join("yolab-custom.json"), details).await {
        discard_dir(&tmp).await;
        return internal(format!("could not save this app's details: {e}"));
    }
    discard_dir(&tmp).await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "app": app,
            "has_form": final_dir.join("values.schema.json").is_file(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const POD: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: hello
spec:
  selector:
    matchLabels: { app: hello }
  template:
    metadata:
      labels: { app: hello }
    spec:
      containers:
        - name: hello
          image: nginx:1.27
"#;

    #[test]
    fn an_ordinary_manifest_is_accepted() {
        assert_eq!(validate_manifest(POD), Ok(2 - 1));
    }

    fn chart_at(root: &std::path::Path, name: &str) {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(
            root.join(name).join("Chart.yaml"),
            format!("name: {name}\n"),
        )
        .unwrap();
    }

    fn uploaded(deps: &[&str]) -> ChartYaml {
        let deps: String = deps.iter().map(|d| format!("  - name: {d}\n")).collect();
        serde_norway::from_str(&format!("name: watch\ndependencies:\n{deps}")).unwrap()
    }

    fn names(dirs: Vec<std::path::PathBuf>) -> Vec<String> {
        dirs.iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn an_uploaded_pack_gets_the_libraries_and_the_catalog_apps_it_is_built_from() {
        let catalog = tempfile::tempdir().unwrap();
        for name in ["yolab-common", "yolab-stack", "jellyfin", "sonarr"] {
            chart_at(catalog.path(), name);
        }
        let upload = tempfile::tempdir().unwrap();
        let meta = uploaded(&["yolab-stack", "jellyfin", "sonarr", "not-in-catalog"]);
        assert_eq!(
            names(vendored(&meta, catalog.path(), upload.path())),
            vec!["yolab-stack", "jellyfin", "sonarr"]
        );
    }

    #[test]
    fn an_app_the_upload_ships_itself_is_kept_but_the_libraries_are_always_ours() {
        let catalog = tempfile::tempdir().unwrap();
        for name in ["yolab-common", "jellyfin", "sonarr"] {
            chart_at(catalog.path(), name);
        }
        let upload = tempfile::tempdir().unwrap();
        let charts = upload.path().join("charts");
        chart_at(&charts, "jellyfin");
        std::fs::write(charts.join("sonarr-0.2.0.tgz"), b"").unwrap();
        std::fs::write(charts.join("yolab-common-0.1.0.tgz"), b"").unwrap();
        let meta = uploaded(&["yolab-common", "jellyfin", "sonarr"]);
        assert_eq!(
            names(vendored(&meta, catalog.path(), upload.path())),
            vec!["yolab-common"]
        );
    }

    #[test]
    fn a_dependency_name_that_could_leave_the_catalog_is_never_copied() {
        let catalog = tempfile::tempdir().unwrap();
        let upload = tempfile::tempdir().unwrap();
        chart_at(upload.path(), "secret");
        let meta = uploaded(&["../secret"]);
        assert!(vendored(&meta, catalog.path(), upload.path()).is_empty());
    }

    #[test]
    fn several_documents_are_counted() {
        let two = format!("{POD}---{POD}");
        assert_eq!(validate_manifest(&two), Ok(2));
    }

    #[test]
    fn empty_documents_are_skipped_not_rejected() {
        let padded = format!("---\n{POD}---\n");
        assert_eq!(validate_manifest(&padded), Ok(1));
    }

    #[test]
    fn cluster_scoped_kinds_are_refused() {
        for kind in [
            "ClusterRoleBinding",
            "CustomResourceDefinition",
            "Namespace",
            "PersistentVolume",
            "StorageClass",
        ] {
            let doc = format!("apiVersion: v1\nkind: {kind}\nmetadata:\n  name: x\n");
            let err = validate_manifest(&doc).unwrap_err();
            assert!(
                err.reason.contains("whole cluster"),
                "{kind}: {}",
                err.reason
            );
        }
    }

    #[test]
    fn a_manifest_naming_another_namespace_is_refused() {
        let doc =
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n  namespace: kube-system\n";
        let err = validate_manifest(doc).unwrap_err();
        assert!(err.reason.contains("kube-system"), "{}", err.reason);
    }

    #[test]
    fn container_escapes_are_found_however_deeply_nested() {
        let cases = [
            ("hostNetwork: true", "spec:\n  template:\n    spec:\n      hostNetwork: true\n"),
            ("hostPath", "spec:\n  template:\n    spec:\n      volumes:\n        - name: v\n          hostPath:\n            path: /etc\n"),
            ("hostPort", "spec:\n  template:\n    spec:\n      containers:\n        - name: c\n          ports:\n            - hostPort: 80\n"),
            ("privileged", "spec:\n  template:\n    spec:\n      containers:\n        - name: c\n          securityContext:\n            privileged: true\n"),
            ("nodeName", "spec:\n  template:\n    spec:\n      nodeName: node1\n"),
        ];
        for (label, body) in cases {
            let doc =
                format!("apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\n{body}");
            assert!(
                validate_manifest(&doc).is_err(),
                "{label} should be refused, but was accepted"
            );
        }
    }

    #[test]
    fn privileged_false_is_fine() {
        let doc = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec:\n  template:\n    spec:\n      containers:\n        - name: c\n          securityContext:\n            privileged: false\n";
        assert!(validate_manifest(doc).is_ok());
    }

    #[test]
    fn a_docker_compose_file_is_named_rather_than_called_invalid_yaml() {
        let compose = "services:\n  web:\n    image: nginx\n    ports:\n      - \"80:80\"\n";
        let err = validate_manifest(compose).unwrap_err();
        assert!(err.reason.contains("Compose"), "{}", err.reason);
    }

    #[test]
    fn broken_yaml_says_so() {
        let err = validate_manifest("kind: [unclosed\n").unwrap_err();
        assert!(err.reason.contains("not valid YAML"), "{}", err.reason);
    }

    #[test]
    fn an_empty_manifest_is_refused() {
        for empty in ["", "   \n", "---\n---\n"] {
            assert!(validate_manifest(empty).is_err(), "{empty:?}");
        }
    }

    #[test]
    fn a_document_without_apiversion_is_refused() {
        let err = validate_manifest("kind: ConfigMap\nmetadata:\n  name: x\n").unwrap_err();
        assert!(err.reason.contains("apiVersion"), "{}", err.reason);
    }

    #[test]
    fn an_id_is_held_to_the_shape_of_a_release_name() {
        for ok in ["hello", "my-app-2"] {
            assert!(valid_id(ok));
        }
        for bad in [
            "",
            "-x",
            "x-",
            "Upper",
            "has space",
            "under_score",
            "a/b",
            &"a".repeat(41),
        ] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_generated_chart_renders_and_ships_the_manifest_untouched() {
        let helm = which("helm").expect("helm is on PATH wherever the tests run");
        let catalog = test_catalog();
        let lib = catalog.path().join("yolab-common");

        let manifest = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: rules\ndata:\n  alert.yaml: |\n    expr: up == 0\n    annotations:\n      summary: \"{{ $labels.instance }} is down\"\n";
        let app = CustomApp {
            id: "my-thing".into(),
            display_name: "My Thing".into(),
            icon: "🔧".into(),
            description: "".into(),
            port: Some(8080),
            service: String::new(),
        };

        let scratch = tempfile::tempdir().unwrap();
        let tmp = scratch.path().to_path_buf();
        write_chart_at(&tmp, &app, manifest)
            .await
            .expect("chart should be written");
        let chart = tmp.join("my-thing");

        tokio::fs::create_dir_all(chart.join("charts"))
            .await
            .unwrap();
        let status = std::process::Command::new("cp")
            .args([
                "-r",
                lib.to_str().unwrap(),
                chart.join("charts").to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let out = std::process::Command::new(helm)
            .args([
                "template",
                "rel",
                chart.to_str().unwrap(),
                "--namespace",
                "yolab-my-thing",
            ])
            .output()
            .expect("helm should run");
        assert!(
            out.status.success(),
            "helm template failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rendered = String::from_utf8_lossy(&out.stdout);

        assert!(
            rendered.contains("{{ $labels.instance }} is down"),
            "the manifest was evaluated instead of shipped:\n{rendered}"
        );
        assert!(
            rendered.contains("name: gateway"),
            "no gateway in:\n{rendered}"
        );
        assert!(
            rendered.contains("localhost:8080"),
            "upstream missing in:\n{rendered}"
        );
        assert!(
            rendered.contains("rootPath: \"/tree/my-thing\""),
            "its data is not its folder of the tree in:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn an_app_with_no_port_gets_no_gateway() {
        let scratch = tempfile::tempdir().unwrap();
        let tmp = scratch.path().to_path_buf();
        let app = CustomApp {
            id: "worker".into(),
            display_name: "Worker".into(),
            icon: String::new(),
            description: String::new(),
            port: None,
            service: String::new(),
        };
        write_chart_at(&tmp, &app, POD).await.unwrap();
        assert!(!tmp.join("worker/templates/gateway.yaml").exists());
        assert!(tmp.join("worker/templates/user.yaml").exists());
    }

    #[test]
    fn archives_are_recognised_by_their_bytes_not_their_name() {
        assert_eq!(sniff(b"PK\x03\x04rest"), Some(Archive::Zip));
        assert_eq!(sniff(b"PK\x05\x06rest"), Some(Archive::Zip));
        assert_eq!(sniff(b"\x1f\x8b\x08rest"), Some(Archive::TarGz));
        for not in [&b"not an archive"[..], b"", b"PK", b"\x1f"] {
            assert!(sniff(not).is_none(), "{not:?} is not an archive");
        }
    }

    #[test]
    fn the_chart_root_is_found_however_it_is_nested() {
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().to_path_buf();

        let flat = base.join("flat");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join("Chart.yaml"), "name: x").unwrap();
        assert_eq!(find_chart_root(&flat, 3), Some(flat.clone()));

        let nested = base.join("nested");
        std::fs::create_dir_all(nested.join("mychart")).unwrap();
        std::fs::write(nested.join("mychart/Chart.yaml"), "name: x").unwrap();
        assert_eq!(find_chart_root(&nested, 3), Some(nested.join("mychart")));

        let mac = base.join("mac");
        std::fs::create_dir_all(mac.join("__MACOSX")).unwrap();
        std::fs::create_dir_all(mac.join("real")).unwrap();
        std::fs::write(mac.join("__MACOSX/Chart.yaml"), "name: junk").unwrap();
        std::fs::write(mac.join("real/Chart.yaml"), "name: x").unwrap();
        assert_eq!(find_chart_root(&mac, 3), Some(mac.join("real")));

        let two = base.join("two");
        std::fs::create_dir_all(two.join("a")).unwrap();
        std::fs::create_dir_all(two.join("b")).unwrap();
        std::fs::write(two.join("a/Chart.yaml"), "name: a").unwrap();
        std::fs::write(two.join("b/Chart.yaml"), "name: b").unwrap();
        assert_eq!(find_chart_root(&two, 3), None);
    }

    #[test]
    fn a_chart_may_name_its_own_namespace_but_no_other() {
        let ok = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\n  namespace: yolab-mine\n";
        assert!(validate_rendered(ok, "yolab-mine").is_ok());

        let err = validate_rendered(ok, "yolab-other").unwrap_err();
        assert!(err.reason.contains("yolab-mine"), "{}", err.reason);
    }

    #[test]
    fn a_rendered_chart_is_held_to_the_same_boundary_as_pasted_yaml() {
        let cluster = "apiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRoleBinding\nmetadata:\n  name: x\n";
        assert!(validate_rendered(cluster, "yolab-x")
            .unwrap_err()
            .reason
            .contains("whole cluster"));

        let host = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec:\n  template:\n    spec:\n      volumes:\n        - name: v\n          hostPath:\n            path: /\n";
        assert!(validate_rendered(host, "yolab-x").is_err());
    }

    fn tree_volume(namespace: &str, root_path: &str) -> String {
        format!(
            "apiVersion: v1\nkind: PersistentVolume\nmetadata:\n  name: {namespace}.x-data\nspec:\n  claimRef:\n    namespace: {namespace}\n    name: x-data\n  csi:\n    driver: cephfs.csi.ceph.com\n    volumeHandle: {namespace}.x-data\n    nodeStageSecretRef:\n      name: yolab-cephfs-static\n      namespace: ceph-csi\n    volumeAttributes:\n      clusterID: yolab\n      fsName: yolab-fs\n      staticVolume: \"true\"\n      rootPath: {root_path}\n"
        )
    }

    #[test]
    fn a_chart_may_keep_its_data_in_a_folder_of_the_tree() {
        assert!(validate_rendered(&tree_volume("yolab-x", "/tree/x"), "yolab-x").is_ok());
        assert!(
            validate_rendered(&tree_volume("yolab-x", "/tree/radarr/movies"), "yolab-x").is_ok()
        );
        assert!(validate_rendered(&tree_volume("yolab-x", "/tree"), "yolab-x").is_ok());
    }

    #[test]
    fn a_volume_outside_the_tree_or_for_another_service_is_refused() {
        for (namespace, root_path) in [
            ("yolab-x", "/volumes/csi/csi-vol-1"),
            ("yolab-x", "/"),
            ("yolab-x", "/tree/../volumes"),
            ("yolab-other", "/tree/x"),
        ] {
            let err = validate_rendered(&tree_volume(namespace, root_path), "yolab-x").unwrap_err();
            assert!(
                err.reason.contains("whole cluster"),
                "{root_path}: {}",
                err.reason
            );
        }
    }

    #[test]
    fn a_pasted_manifest_may_not_make_a_folder_of_the_tree() {
        assert!(validate_manifest(&tree_volume("yolab-x", "/tree/x")).is_err());
    }

    #[tokio::test]
    async fn a_real_packaged_chart_survives_the_whole_pipeline() {
        let helm = which("helm").expect("helm is on PATH wherever the tests run");
        let charts = test_catalog();
        let catalog = charts.path();
        let scratch = tempfile::tempdir().unwrap();
        let tmp = scratch.path().to_path_buf();
        std::fs::create_dir_all(&tmp).unwrap();

        let src = tmp.join("pairdrop");
        std::process::Command::new("cp")
            .args([
                "-r",
                catalog.join("pairdrop").to_str().unwrap(),
                src.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        std::fs::create_dir_all(src.join("charts")).unwrap();
        std::process::Command::new("cp")
            .args([
                "-r",
                catalog.join("yolab-common").to_str().unwrap(),
                src.join("charts").to_str().unwrap(),
            ])
            .status()
            .unwrap();

        let out = std::process::Command::new(&helm)
            .args([
                "package",
                src.to_str().unwrap(),
                "--destination",
                tmp.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "helm package: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let tgz = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "tgz"))
            .expect("helm package should have produced a .tgz");

        let bytes = std::fs::read(&tgz).unwrap();
        assert_eq!(sniff(&bytes), Some(Archive::TarGz));

        let dest = tmp.join("x");
        std::fs::create_dir_all(&dest).unwrap();
        let status = std::process::Command::new("tar")
            .args([
                "-xzf",
                tgz.to_str().unwrap(),
                "-C",
                dest.to_str().unwrap(),
                "--no-same-owner",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let root = find_chart_root(&dest, 3).expect("Chart.yaml should be found in the package");
        let meta: ChartYaml =
            serde_norway::from_str(&std::fs::read_to_string(root.join("Chart.yaml")).unwrap())
                .unwrap();
        assert_eq!(meta.name, "pairdrop");
        assert!(valid_id(&meta.name));
        assert_eq!(
            meta.annotations
                .get("yolab.io/display-name")
                .map(String::as_str),
            Some("PairDrop")
        );

        let ns = format!("yolab-{}", meta.name);
        let rendered = std::process::Command::new(&helm)
            .args([
                "template",
                &meta.name,
                root.to_str().unwrap(),
                "--namespace",
                &ns,
            ])
            .output()
            .unwrap();
        assert!(
            rendered.status.success(),
            "helm template: {}",
            String::from_utf8_lossy(&rendered.stderr)
        );
        let text = String::from_utf8_lossy(&rendered.stdout);

        validate_rendered(&text, &ns).expect("a catalog chart must satisfy the upload rules");

        assert!(root.join("values.schema.json").is_file());
    }

    fn pod_with(containers: &str) -> String {
        format!("apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec:\n  template:\n    spec:\n      containers:\n{containers}")
    }

    #[test]
    fn the_real_tunnel_sidecar_may_be_privileged() {
        let doc = pod_with(
            "        - name: wireguard\n          image: ghcr.io/demycode/wg-sidecar:latest@sha256:d7706338f231b0e54a8ac6c4a2940f5d9d8c2ac017a69dd378250359ee3d98c1\n          securityContext:\n            privileged: true\n",
        );
        assert!(validate_rendered(&doc, "yolab-x").is_ok());
    }

    #[test]
    fn a_container_merely_named_wireguard_may_not_be_privileged() {
        let doc = pod_with(
            "        - name: wireguard\n          image: attacker/anything:latest\n          securityContext:\n            privileged: true\n",
        );
        let err = validate_rendered(&doc, "yolab-x").unwrap_err();
        assert!(err.reason.contains("privileged"), "{}", err.reason);
    }

    #[test]
    fn a_sibling_of_the_sidecar_may_not_be_privileged() {
        let doc = pod_with(
            "        - name: wireguard\n          image: ghcr.io/demycode/wg-sidecar:latest@sha256:d7706338f231b0e54a8ac6c4a2940f5d9d8c2ac017a69dd378250359ee3d98c1\n          securityContext:\n            privileged: true\n        - name: app\n          image: nginx:1.27\n          securityContext:\n            privileged: true\n",
        );
        assert!(validate_rendered(&doc, "yolab-x").is_err());
    }

    #[test]
    fn pasted_yaml_gets_no_sidecar_exception() {
        let doc = pod_with(
            "        - name: wireguard\n          image: ghcr.io/demycode/wg-sidecar:latest@sha256:d7706338f231b0e54a8ac6c4a2940f5d9d8c2ac017a69dd378250359ee3d98c1\n          securityContext:\n            privileged: true\n",
        );
        assert!(validate_rendered(&doc, "yolab-x").is_ok());
        assert!(
            validate_manifest(&doc).is_err(),
            "a pasted manifest has no gateway of its own and needs no privileged container"
        );
    }

    fn test_catalog() -> tempfile::TempDir {
        let in_repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/catalog");
        let from = std::env::var_os("YOLAB_TEST_CATALOG")
            .map(std::path::PathBuf::from)
            .unwrap_or(in_repo);
        let dir = tempfile::tempdir().unwrap();
        for chart in ["yolab-common", "pairdrop"] {
            let copied = std::process::Command::new("cp")
                .args(["-rL", "--no-preserve=mode"])
                .arg(from.join(chart))
                .arg(dir.path().join(chart))
                .status()
                .unwrap();
            assert!(
                copied.success(),
                "{chart} could not be copied from {}",
                from.display()
            );
        }
        dir
    }

    fn which(bin: &str) -> Result<String, ()> {
        std::env::var("PATH")
            .ok()
            .and_then(|p| {
                p.split(':')
                    .map(|d| std::path::Path::new(d).join(bin))
                    .find(|c| c.is_file())
                    .map(|c| c.to_string_lossy().into_owned())
            })
            .ok_or(())
    }

    #[test]
    fn a_manifest_containing_go_templates_is_accepted() {
        let doc = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: dash\ndata:\n  rule: \"{{ $labels.instance }} is down\"\n";
        assert!(validate_manifest(doc).is_ok());
    }

    fn pod_spec(spec: &str) -> String {
        format!("apiVersion: v1\nkind: Pod\nmetadata:\n  name: p\nspec:\n{spec}")
    }

    #[test]
    fn sharing_a_machine_namespace_is_refused_but_turning_it_off_is_fine() {
        let err = validate_manifest(&pod_spec("  hostPID: true\n")).unwrap_err();
        assert!(err.reason.contains("hostPID: true"), "{}", err.reason);
        assert_eq!(validate_manifest(&pod_spec("  hostIPC: false\n")), Ok(1));
    }

    #[test]
    fn containers_that_are_not_a_list_are_still_scanned() {
        let err = validate_manifest(&pod_spec("  containers:\n    hostPort: 80\n")).unwrap_err();
        assert!(err.reason.contains("hostPort"), "{}", err.reason);
    }

    #[test]
    fn a_manifest_over_512_kb_is_refused() {
        let big = pod_spec(&format!("  x: \"{}\"\n", "a".repeat(512 * 1024)));
        assert_eq!(
            validate_manifest(&big),
            Err(reject("that manifest is larger than 512 KB"))
        );
    }

    #[test]
    fn an_empty_rendered_document_is_skipped_and_broken_output_is_refused() {
        assert!(validate_rendered(&format!("---\n{POD}---\n"), "yolab-x").is_ok());
        let err = validate_rendered("kind: [unclosed\n", "yolab-x").unwrap_err();
        assert!(err.reason.contains("not valid YAML"), "{}", err.reason);
    }

    #[test]
    fn files_and_chartless_folders_are_passed_over_and_the_search_stops_at_its_depth() {
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path();
        std::fs::write(base.join("README.md"), "hi").unwrap();
        std::fs::create_dir_all(base.join("empty")).unwrap();
        std::fs::create_dir_all(base.join("deep/er")).unwrap();
        std::fs::write(base.join("deep/er/Chart.yaml"), "name: x").unwrap();
        assert_eq!(find_chart_root(base, 1), None);
        assert_eq!(find_chart_root(base, 2), Some(base.join("deep/er")));
    }

    fn web_app() -> CustomApp {
        CustomApp {
            id: "web".into(),
            display_name: "Web".into(),
            icon: String::new(),
            description: "a \"quoted\" one".into(),
            port: Some(80),
            service: "frontend".into(),
        }
    }

    #[tokio::test]
    async fn a_named_service_is_the_gateways_upstream_and_the_details_are_kept() {
        let scratch = tempfile::tempdir().unwrap();
        write_chart_at(scratch.path(), &web_app(), POD)
            .await
            .unwrap();
        let read =
            |name: &str| std::fs::read_to_string(scratch.path().join("web").join(name)).unwrap();
        assert!(read("values.yaml").contains("upstream: frontend:80"));
        assert!(read("Chart.yaml").contains(r#"description: "a \"quoted\" one""#));
        assert!(read("templates/gateway.yaml").contains("name: gateway"));
        assert_eq!(read("user-manifest.yaml"), POD);
        let details: CustomApp = serde_json::from_str(&read("yolab-custom.json")).unwrap();
        assert_eq!(details.service, "frontend");
        let schema: Value = serde_json::from_str(&read("values.schema.json")).unwrap();
        assert_eq!(
            schema["properties"]["config"]["properties"]["subdomain"]["default"],
            "web"
        );
    }

    #[tokio::test]
    async fn a_chart_folder_that_cannot_be_cleared_or_made_is_an_error() {
        let in_the_way = tempfile::NamedTempFile::new().unwrap();
        assert!(write_chart_at(in_the_way.path(), &web_app(), POD)
            .await
            .is_err());
        let unwritable = std::path::Path::new("/proc/yolab-custom");
        assert!(write_chart_at(unwritable, &web_app(), POD).await.is_err());
    }

    #[tokio::test]
    async fn the_custom_list_is_sorted_by_name_and_skips_folders_without_readable_details() {
        let _cache = crate::charts::testing::cache_in_temp();
        let root = custom_dir();
        for (id, name) in [("zeta", "Alpha"), ("alpha", "Zulu")] {
            std::fs::create_dir_all(root.join(id)).unwrap();
            let details = serde_json::json!({ "id": id, "display_name": name });
            std::fs::write(root.join(id).join("yolab-custom.json"), details.to_string()).unwrap();
        }
        std::fs::create_dir_all(root.join("bare")).unwrap();
        std::fs::create_dir_all(root.join("garbled")).unwrap();
        std::fs::write(root.join("garbled/yolab-custom.json"), "{").unwrap();
        let Json(apps) = list_custom().await;
        let names: Vec<&str> = apps.iter().map(|a| a.display_name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Zulu"]);
    }

    #[tokio::test]
    async fn an_archive_that_will_not_unpack_is_refused_and_nothing_is_kept() {
        let host = crate::host::fake::FakeHost::new()
            .fail("tar -xzf", "gzip: stdin: unexpected end of file");
        let custom = tempfile::tempdir().unwrap();
        let (status, Json(body)) =
            upload(&host, custom.path(), custom.path(), b"\x1f\x8b\x08broken").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("unexpected end of file"),
            "{body}"
        );
        assert_eq!(std::fs::read_dir(custom.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_archive_without_a_chart_is_refused_before_helm_runs() {
        let host = crate::host::fake::FakeHost::new().ok("unzip", "");
        let custom = tempfile::tempdir().unwrap();
        let (status, Json(body)) =
            upload(&host, custom.path(), custom.path(), b"PK\x03\x04empty").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("no Chart.yaml"),
            "{body}"
        );
        assert!(!host.ran("helm"));
    }

    #[tokio::test]
    async fn something_that_is_no_archive_never_reaches_a_tool() {
        let host = crate::host::fake::FakeHost::new();
        let custom = tempfile::tempdir().unwrap();
        let (status, _) = upload(&host, custom.path(), custom.path(), b"hello").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(host.calls().is_empty());
    }

    mod uploads {
        use super::*;
        use crate::host::fake::FakeHost;

        const TGZ: &[u8] = b"\x1f\x8b\x08 a chart";

        fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap().flatten() {
                let target = to.join(entry.file_name());
                if entry.path().is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    std::fs::copy(entry.path(), target).unwrap();
                }
            }
        }

        fn copying(host: FakeHost) -> FakeHost {
            host.effect("cp -r", |_: &str, args: &[&str]| {
                let from = std::path::Path::new(args[1]);
                let into = std::path::Path::new(args[2]);
                let to = if into.is_dir() {
                    into.join(from.file_name().unwrap())
                } else {
                    into.to_path_buf()
                };
                copy_tree(from, &to);
            })
        }

        fn unpacking(files: Vec<(&'static str, &'static str)>) -> FakeHost {
            FakeHost::new()
                .ok("tar -xzf", "")
                .effect("tar -xzf", move |_: &str, args: &[&str]| {
                    let dest = std::path::Path::new(args[3]).join("mychart");
                    std::fs::create_dir_all(&dest).unwrap();
                    for (name, body) in &files {
                        std::fs::write(dest.join(name), body).unwrap();
                    }
                })
        }

        const CHART: &str = "apiVersion: v2\nname: mychart\ndescription: Mine\nversion: 0.1.0\nannotations:\n  yolab.io/display-name: My Chart\n  yolab.io/icon: box\ndependencies:\n  - name: yolab-common\n";

        const RENDERED: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: yolab-mychart\n";

        struct Dirs {
            catalog: tempfile::TempDir,
            custom: tempfile::TempDir,
        }

        fn dirs() -> Dirs {
            let catalog = tempfile::tempdir().unwrap();
            let lib = catalog.path().join("yolab-common");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(
                lib.join("Chart.yaml"),
                "name: yolab-common\ntype: library\n",
            )
            .unwrap();
            Dirs {
                catalog,
                custom: tempfile::tempdir().unwrap(),
            }
        }

        async fn uploaded(host: FakeHost, body: &[u8]) -> (StatusCode, Value, Dirs) {
            let d = dirs();
            let (code, Json(said)) = upload(&host, d.catalog.path(), d.custom.path(), body).await;
            (code, said, d)
        }

        fn good() -> FakeHost {
            copying(
                unpacking(vec![("Chart.yaml", CHART), ("values.schema.json", "{}")])
                    .ok("cp -r", "")
                    .ok("helm template", RENDERED),
            )
        }

        #[tokio::test]
        async fn an_uploaded_chart_is_checked_vendored_and_kept_as_a_custom_service() {
            let (code, said, d) = uploaded(good(), TGZ).await;
            assert_eq!(code, StatusCode::OK, "{said}");
            assert_eq!(said["app"]["id"], "mychart");
            assert_eq!(said["app"]["display_name"], "My Chart");
            assert_eq!(said["app"]["icon"], "box");
            assert_eq!(said["has_form"], true);
            let kept = d.custom.path().join("mychart");
            assert!(kept.join("Chart.yaml").is_file());
            assert!(kept.join("charts/yolab-common/Chart.yaml").is_file());
            let details: CustomApp = serde_json::from_str(
                &std::fs::read_to_string(kept.join("yolab-custom.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(details.description, "Mine");

            let again = copying(
                unpacking(vec![("Chart.yaml", "name: mychart\n")])
                    .ok("cp -r", "")
                    .ok("helm template", RENDERED),
            );
            let (code, said) = {
                let (code, Json(said)) =
                    upload(&again, d.catalog.path(), d.custom.path(), TGZ).await;
                (code, said)
            };
            assert_eq!(code, StatusCode::OK, "{said}");
            assert_eq!(said["app"]["display_name"], "mychart");
            assert_eq!(said["has_form"], false);
        }

        async fn refused(host: FakeHost, body: &[u8]) -> (StatusCode, String) {
            let (code, said, _d) = uploaded(host, body).await;
            (code, said["error"].as_str().unwrap_or_default().to_string())
        }

        #[tokio::test]
        async fn an_upload_that_is_not_a_safe_chart_is_refused_with_why() {
            let cases: Vec<(FakeHost, &[u8], StatusCode, &str)> = vec![
                (
                    FakeHost::new(),
                    &b"plain text"[..],
                    StatusCode::BAD_REQUEST,
                    "is not a .zip or a .tgz",
                ),
                (
                    FakeHost::new().fail("tar -xzf", "corrupt"),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "could not unpack that archive",
                ),
                (
                    FakeHost::new().fail("unzip", "corrupt"),
                    &b"PK\x03\x04 zip"[..],
                    StatusCode::BAD_REQUEST,
                    "could not unpack that archive",
                ),
                (
                    FakeHost::new().unstartable("tar -xzf"),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "could not run tar",
                ),
                (
                    FakeHost::new().ok("tar -xzf", ""),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "no Chart.yaml",
                ),
                (
                    unpacking(vec![("Chart.yaml", "name: [")]),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "Chart.yaml is not valid",
                ),
                (
                    unpacking(vec![("Chart.yaml", "name: Bad_Name\n")]),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "cannot be used as an app id",
                ),
                (
                    copying(
                        unpacking(vec![("Chart.yaml", CHART)])
                            .ok("cp -r", "")
                            .fail("helm template", "template: bad"),
                    ),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "this chart does not render",
                ),
                (
                    copying(unpacking(vec![("Chart.yaml", CHART)]).ok("cp -r", "").ok(
                        "helm template",
                        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: x\n",
                    )),
                    TGZ,
                    StatusCode::BAD_REQUEST,
                    "applies to the whole cluster",
                ),
                (
                    unpacking(vec![("Chart.yaml", CHART)]).fail("cp -r", "disk full"),
                    TGZ,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not add",
                ),
                (
                    unpacking(vec![("Chart.yaml", "name: mychart\n")])
                        .ok("helm template", RENDERED)
                        .fail("cp -r", "disk full"),
                    TGZ,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "disk full",
                ),
            ];
            for (host, body, code, why) in cases {
                let (got, said) = refused(host, body).await;
                assert_eq!(got, code, "{why}: {said}");
                assert!(said.contains(why), "{why}: {said}");
            }
        }

        fn unpacking_bytes(chart: &'static [u8]) -> FakeHost {
            FakeHost::new()
                .ok("tar -xzf", "")
                .effect("tar -xzf", move |_: &str, args: &[&str]| {
                    let dest = std::path::Path::new(args[3]).join("mychart");
                    std::fs::create_dir_all(&dest).unwrap();
                    std::fs::write(dest.join("Chart.yaml"), chart).unwrap();
                })
        }

        #[tokio::test]
        async fn an_upload_that_cannot_be_unpacked_or_kept_says_why() {
            let cases: Vec<(FakeHost, StatusCode, &str)> = vec![
                (
                    unpacking_bytes(b"name: \xff\xfe\n"),
                    StatusCode::BAD_REQUEST,
                    "Chart.yaml could not be read",
                ),
                (
                    unpacking(vec![("Chart.yaml", CHART), ("charts", "not a folder")]),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not make the chart's charts folder",
                ),
                (
                    unpacking(vec![("Chart.yaml", "name: mychart\n")])
                        .ok("helm template", RENDERED)
                        .ok("cp -r", ""),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not save this app's details",
                ),
            ];
            for (host, code, why) in cases {
                let (got, said) = refused(host, TGZ).await;
                assert_eq!(got, code, "{why}: {said}");
                assert!(said.contains(why), "{why}: {said}");
            }
        }

        #[tokio::test]
        async fn a_scratch_folder_that_cannot_be_made_is_an_internal_error() {
            let d = dirs();
            let scratch = std::path::Path::new("/proc/yolab-scratch");
            let (code, Json(said)) =
                upload_in(&good(), d.catalog.path(), d.custom.path(), TGZ, scratch).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(said["error"]
                .as_str()
                .unwrap()
                .contains("could not make a folder to unpack into"));
        }

        #[tokio::test]
        async fn a_custom_folder_that_cannot_be_made_or_cleared_is_an_internal_error() {
            let d = dirs();
            let unwritable = std::path::Path::new("/proc/yolab-custom");
            let (code, _) = upload(&good(), d.catalog.path(), unwritable, TGZ).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);

            std::fs::write(d.custom.path().join("mychart"), "in the way").unwrap();
            let (code, Json(said)) = upload(&good(), d.catalog.path(), d.custom.path(), TGZ).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(said["error"]
                .as_str()
                .unwrap()
                .contains("could not remove the previous copy"));
        }

        #[tokio::test]
        async fn a_custom_service_that_cannot_be_written_is_an_internal_error() {
            let _cache = crate::charts::testing::cache_in_temp();
            std::fs::create_dir_all(custom_dir()).unwrap();
            std::fs::write(custom_dir().join("hello"), "in the way").unwrap();
            let api = crate::testkit::TestApi::with_kube_and_host(
                crate::k8s::testing::unreachable(),
                FakeHost::new(),
            )
            .login()
            .await;
            let body = serde_json::json!({ "id": "hello", "display_name": "Hello", "yaml": POD });
            let res = api.post("/api/apps/custom", &body.to_string()).await;
            assert_eq!(
                res.status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{}",
                res.body
            );
        }

        #[tokio::test]
        async fn custom_services_are_saved_listed_and_removed_over_http() {
            let _cache = crate::charts::testing::cache_in_temp();
            let api = crate::testkit::TestApi::with_kube_and_host(
                crate::k8s::testing::unreachable(),
                FakeHost::new(),
            )
            .login()
            .await;
            let save = |body: Value| {
                let api = &api;
                async move { api.post("/api/apps/custom", &body.to_string()).await }
            };
            for bad in [
                serde_json::json!({ "id": "-x", "display_name": "X", "yaml": POD }),
                serde_json::json!({ "id": "hello", "display_name": " ", "yaml": POD }),
                serde_json::json!({ "id": "hello", "display_name": "Hello", "yaml": "kind: [" }),
            ] {
                assert_eq!(
                    save(bad.clone()).await.status,
                    StatusCode::BAD_REQUEST,
                    "{bad}"
                );
            }
            let res = save(serde_json::json!({
                "id": "hello", "display_name": "Hello", "yaml": POD, "port": 8080
            }))
            .await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.body);
            assert_eq!(res.json()["app"]["id"], "hello");
            let res = api.get("/api/apps/custom").await;
            assert_eq!(res.json()[0]["display_name"], "Hello", "{}", res.body);
            assert_eq!(
                api.send("DELETE", "/api/apps/custom/hello", None)
                    .await
                    .status,
                StatusCode::OK
            );
            assert_eq!(
                api.send("DELETE", "/api/apps/custom/hello", None)
                    .await
                    .status,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                api.send("DELETE", "/api/apps/custom/Bad_Id", None)
                    .await
                    .status,
                StatusCode::BAD_REQUEST
            );
            let res = api.post("/api/apps/custom/chart", "plain text").await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
        }
    }
}
