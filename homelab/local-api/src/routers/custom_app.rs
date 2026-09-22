use axum::{
    extract::Path as AxPath, extract::State, http::StatusCode, response::IntoResponse, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::AppState;

pub const CUSTOM_REPO: &str = "custom";

fn custom_dir() -> PathBuf {
    PathBuf::from(crate::charts::CACHE_DIR).join(CUSTOM_REPO)
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

const CHART_TEMPLATE: &str = r#"{{ include "yolab-common.caddyConfigMap" . }}
---
{{ include "yolab-common.uninstallHook" . }}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ .Release.Name }}-data
  namespace: {{ .Release.Namespace }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: yolab-cephfs
  resources:
    requests:
      storage: {{ .Values.config.storage_size | default "5Gi" | quote }}
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
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(dir.join("templates")).await?;

    let upstream = match (app.port, app.service.as_str()) {
        (Some(p), "") => format!("localhost:{p}"),
        (Some(p), svc) => format!("{svc}:{p}"),
        (None, _) => String::new(),
    };

    let chart_yaml = format!(
        "apiVersion: v2\nname: {id}\ndescription: {desc}\ntype: application\nversion: 0.1.0\n\ndependencies:\n  - name: yolab-common\n    version: \"0.1.1\"\n    repository: \"oci://ghcr.io/demycode/charts\"\n\nannotations:\n  yolab.io/display-name: {display}\n  yolab.io/icon: {icon}\n  yolab.io/category: \"custom\"\n  yolab.io/uischema: |\n    {{ \"subdomain\": {{ \"ui:widget\": \"TunnelWidget\" }} }}\n",
        id = app.id,
        desc = serde_json::to_string(&app.description).unwrap_or_else(|_| "\"\"".into()),
        display = serde_json::to_string(&app.display_name).unwrap_or_else(|_| "\"\"".into()),
        icon = serde_json::to_string(&app.icon).unwrap_or_else(|_| "\"\"".into()),
    );
    tokio::fs::write(dir.join("Chart.yaml"), chart_yaml).await?;

    let values = format!(
        "config:\n  subdomain: {id}\n  storage_size: 5Gi\nyolab:\n  platformApiUrl: \"\"\n  accountToken: \"\"\n  serviceName: \"\"\n  gateway:\n    upstream: {upstream}\n",
        id = app.id,
        upstream = if upstream.is_empty() { "\"\"".to_string() } else { upstream.clone() },
    );
    tokio::fs::write(dir.join("values.yaml"), values).await?;

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
                    },
                    "storage_size": {
                        "type": "string", "title": "Storage size", "default": "5Gi"
                    }
                }
            },
            "yolab": {
                "type": "object",
                "description": "Injected by local-api at install time; not user-editable.",
                "properties": {
                    "platformApiUrl": {"type": "string"},
                    "accountToken": {"type": "string"},
                    "serviceName": {"type": "string"},
                    "images": {"type": "object"},
                    "gateway": {
                        "type": "object",
                        "properties": {
                            "upstream": {"type": "string"},
                            "caddyfile": {"type": "string"},
                            "pvcName": {"type": "string"}
                        }
                    }
                }
            }
        }
    });
    tokio::fs::write(
        dir.join("values.schema.json"),
        serde_json::to_string_pretty(&schema)?,
    )
    .await?;

    if !upstream.is_empty() {
        tokio::fs::write(dir.join("templates/gateway.yaml"), CHART_TEMPLATE).await?;
    }
    tokio::fs::write(dir.join("templates/user.yaml"), USER_TEMPLATE).await?;
    tokio::fs::write(dir.join("user-manifest.yaml"), yaml).await?;
    tokio::fs::write(dir.join("yolab-custom.json"), serde_json::to_string(app)?).await?;
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
    let entries = std::fs::read_dir(base).ok()?;
    let mut found = None;
    for e in entries.flatten() {
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
        if CLUSTER_SCOPED.contains(&kind) {
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

async fn run(cmd: &str, args: &[&str]) -> Result<String, Rejection> {
    let work = tokio::process::Command::new(cmd)
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(RUN_TIMEOUT, work)
        .await
        .map_err(|_| reject(format!("{cmd} timed out after {}s", RUN_TIMEOUT.as_secs())))?
        .map_err(|e| reject(format!("could not run {cmd}: {e}")))?;
    if !out.status.success() {
        return Err(reject(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn upload_chart(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let bad = |msg: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        )
    };

    let Some(kind) = sniff(&body) else {
        return bad("that file is not a .zip or a .tgz — package a chart with `helm package`, or zip the chart folder".into());
    };

    let tmp = std::env::temp_dir().join(format!(
        "yolab-chart-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    if let Err(e) = tokio::fs::create_dir_all(&tmp).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        );
    }
    let archive = tmp.join("upload");
    if let Err(e) = tokio::fs::write(&archive, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        );
    }

    let dest = tmp.join("x");
    let _ = tokio::fs::create_dir_all(&dest).await;
    let extracted = match kind {
        Archive::TarGz => {
            run(
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
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return bad(format!("could not unpack that archive: {}", r.reason));
    }

    let Some(root) = find_chart_root(&dest, 3) else {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return bad("no Chart.yaml in there — this needs to be a Helm chart. If the archive holds two charts, upload them separately.".into());
    };

    let chart_text = match tokio::fs::read_to_string(root.join("Chart.yaml")).await {
        Ok(t) => t,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return bad(format!("Chart.yaml could not be read: {e}"));
        }
    };
    let meta: ChartYaml = match serde_norway::from_str(&chart_text) {
        Ok(m) => m,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return bad(format!("Chart.yaml is not valid: {e}"));
        }
    };
    if !valid_id(&meta.name) {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return bad(format!(
            "\"{}\" cannot be used as an app id — lowercase letters, digits and hyphens only",
            meta.name
        ));
    }

    if meta.dependencies.iter().any(|d| d.name == "yolab-common") {
        let lib = state.config.catalog_dir().join("yolab-common");
        if lib.is_dir() {
            let charts_dir = root.join("charts");
            let _ = tokio::fs::create_dir_all(&charts_dir).await;
            let _ = run(
                "cp",
                &[
                    "-r",
                    lib.to_str().unwrap_or(""),
                    charts_dir.to_str().unwrap_or(""),
                ],
            )
            .await;
        }
    }

    let release_ns = format!("yolab-{}", meta.name);
    let rendered = match run(
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
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return bad(format!("this chart does not render: {}", r.reason));
        }
    };
    if let Err(r) = validate_rendered(&rendered, &release_ns) {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return bad(r.reason);
    }

    let final_dir = custom_dir().join(&meta.name);
    if let Err(e) = tokio::fs::create_dir_all(custom_dir()).await {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        );
    }
    let _ = tokio::fs::remove_dir_all(&final_dir).await;
    if let Err(r) = run(
        "cp",
        &[
            "-r",
            root.to_str().unwrap_or(""),
            final_dir.to_str().unwrap_or(""),
        ],
    )
    .await
    {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
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
    let _ = tokio::fs::write(
        final_dir.join("yolab-custom.json"),
        serde_json::to_string(&app).unwrap_or_default(),
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;

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
        let Ok(helm) = which("helm") else { return };
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/catalog/yolab-common");
        if !lib.exists() {
            return;
        }

        let manifest = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: rules\ndata:\n  alert.yaml: |\n    expr: up == 0\n    annotations:\n      summary: \"{{ $labels.instance }} is down\"\n";
        let app = CustomApp {
            id: "my-thing".into(),
            display_name: "My Thing".into(),
            icon: "🔧".into(),
            description: "".into(),
            port: Some(8080),
            service: String::new(),
        };

        let tmp = std::env::temp_dir().join(format!("yolab-custom-test-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
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

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn an_app_with_no_port_gets_no_gateway() {
        let tmp = std::env::temp_dir().join(format!("yolab-custom-noport-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
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
        let _ = tokio::fs::remove_dir_all(&tmp).await;
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
        let base = std::env::temp_dir().join(format!("yolab-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

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

        let _ = std::fs::remove_dir_all(&base);
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

    #[tokio::test]
    async fn a_real_packaged_chart_survives_the_whole_pipeline() {
        let Ok(helm) = which("helm") else { return };
        let catalog = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/catalog");
        if !catalog.join("pairdrop/Chart.yaml").exists() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("yolab-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
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

        let _ = std::fs::remove_dir_all(&tmp);
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
}
