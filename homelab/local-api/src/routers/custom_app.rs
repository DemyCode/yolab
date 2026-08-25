//! Apps the owner brings themselves, as plain Kubernetes YAML.
//!
//! ── Why the YAML becomes a chart instead of being applied directly ────────────
//!
//! `kubectl apply` would be three lines and would strand the result outside
//! everything that makes an app an app here. A YoLab app is not just a Deployment:
//! it is a namespace with the labels the Apps page lists by, a tunnel subdomain
//! claimed through wg-register, a Caddy that terminates TLS for it, a PVC the backup
//! system already knows to include, and an uninstall path that gives the subdomain
//! back. All of that lives in the yolab-common Helm library, and all of it would have
//! to be reimplemented for anything applied outside Helm.
//!
//! So a custom app is materialised as a real chart that depends on yolab-common, and
//! then installed down the ordinary path. It appears on the Apps page, gets a URL,
//! gets backed up and uninstalls cleanly, because it genuinely is an app like the
//! others — the only difference is who wrote the manifest.
//!
//! ── Why the manifest is a FILE and not a template ────────────────────────────
//!
//! Anything under `templates/` is rendered by Helm, and a manifest that happens to
//! contain `{{` — a Go template in a ConfigMap, a Prometheus rule, a Grafana
//! dashboard — would be interpreted rather than shipped, and would usually fail to
//! parse. The manifest is written to the chart root and pulled in with
//! `.Files.Get`, which returns the bytes untouched.
//!
//! ── What is refused ──────────────────────────────────────────────────────────
//!
//! check_charts.py enforces the catalog's safety rules at build time; nothing
//! enforces them on YAML that arrives at runtime. The rules below are the subset
//! that stops a pasted manifest from reaching past its own namespace — the same
//! boundary the catalog is held to, applied at the point the YAML arrives rather
//! than trusting whoever pasted it.

use axum::{extract::Path as AxPath, extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::AppState;

/// Reserved repo name for locally-authored charts. `charts::chart_sources` includes
/// this directory, and `sync_repo` never touches it because it is not a real repo.
pub const CUSTOM_REPO: &str = "custom";

fn custom_dir() -> PathBuf {
    PathBuf::from(crate::charts::CACHE_DIR).join(CUSTOM_REPO)
}

/// Kinds that exist outside a namespace, and therefore outside the blast radius an
/// app is supposed to have. A ClusterRoleBinding in a pasted manifest is a cluster
/// takeover with extra steps.
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
    Rejection { reason: reason.into() }
}

/// Walks a parsed document looking for the things a namespaced app may not do.
///
/// Recursive on purpose: a pod spec can be nested at several depths depending on the
/// workload kind (Deployment, StatefulSet, Job, CronJob each bury it differently), and
/// checking only the shapes we thought of is how one gets missed.
fn scan_for_escapes(node: &Value, out: &mut Vec<String>) {
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
                    "privileged" if v == &Value::Bool(true) => {
                        out.push("privileged: true removes the container boundary".into());
                    }
                    "nodeName" => {
                        out.push("nodeName pins this to one machine by name".into());
                    }
                    _ => {}
                }
                scan_for_escapes(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                scan_for_escapes(v, out);
            }
        }
        _ => {}
    }
}

/// Parses a multi-document manifest and holds it to the same boundary the catalog is
/// held to. Returns the number of documents on success.
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
        // `---` separators produce empty documents; they are not an error.
        if v.is_null() {
            continue;
        }
        let Some(kind) = v["kind"].as_str() else {
            return Err(reject("every document needs a `kind` — this looks like Kubernetes YAML is missing, or a Docker Compose file was pasted instead"));
        };
        if v["apiVersion"].as_str().is_none() {
            return Err(reject(format!("the {kind} document is missing `apiVersion`")));
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
        scan_for_escapes(&v, &mut escapes);
        if let Some(first) = escapes.first() {
            return Err(reject(format!("the {kind} document is not allowed: {first}")));
        }
        count += 1;
    }

    if count == 0 {
        return Err(reject("there is nothing here to install"));
    }
    Ok(count)
}

/// Chart id rules: it becomes a directory name, a URL segment and a Helm release name.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
    /// Port inside the pod that Caddy should send traffic to. None means the app has
    /// no web interface and gets no subdomain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Service name Caddy proxies to. Empty means "in the gateway pod", which a raw
    /// manifest never is — its workloads are separate Deployments.
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

/// `.Files.Get` returns the file's bytes without rendering them, which is the whole
/// point: a manifest containing `{{ }}` ships as written instead of being evaluated.
const USER_TEMPLATE: &str = "{{ .Files.Get \"user-manifest.yaml\" }}\n";

async fn write_chart_at(root: &std::path::Path, app: &CustomApp, yaml: &str) -> anyhow::Result<()> {
    let dir = root.join(&app.id);
    // Clean slate: an edit that removes a document must not leave it behind, the same
    // reason sync_repo untars over a removed directory rather than into a live one.
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(dir.join("templates")).await?;

    let upstream = match (app.port, app.service.as_str()) {
        (Some(p), "") => format!("localhost:{p}"),
        (Some(p), svc) => format!("{svc}:{p}"),
        (None, _) => String::new(),
    };

    let chart_yaml = format!(
        "apiVersion: v2\nname: {id}\ndescription: {desc}\ntype: application\nversion: 0.1.0\n\ndependencies:\n  - name: yolab-common\n    version: \"0.1.0\"\n    repository: \"oci://ghcr.io/demycode/charts\"\n\nannotations:\n  yolab.io/display-name: {display}\n  yolab.io/icon: {icon}\n  yolab.io/category: \"custom\"\n  yolab.io/uischema: |\n    {{ \"subdomain\": {{ \"ui:widget\": \"TunnelWidget\" }} }}\n",
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
    tokio::fs::write(dir.join("values.schema.json"), serde_json::to_string_pretty(&schema)?).await?;

    // The gateway half only when the app actually serves something. An app with no
    // port would otherwise get a subdomain that resolves to a Caddy with nowhere to
    // send the request — a 502 with a DNS record in front of it.
    if !upstream.is_empty() {
        tokio::fs::write(dir.join("templates/gateway.yaml"), CHART_TEMPLATE).await?;
    }
    tokio::fs::write(dir.join("templates/user.yaml"), USER_TEMPLATE).await?;
    tokio::fs::write(dir.join("user-manifest.yaml"), yaml).await?;
    tokio::fs::write(
        dir.join("yolab-custom.json"),
        serde_json::to_string(app)?,
    )
    .await?;
    Ok(())
}

/// GET /api/apps/custom
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

/// POST /api/apps/custom — validate and materialise. Does NOT install; the app then
/// appears in the catalog and is installed through the ordinary form, so a custom app
/// and a catalog app are the same thing from here on.
pub async fn save_custom(
    State(_s): State<AppState>,
    Json(req): Json<SaveCustomReq>,
) -> impl IntoResponse {
    if !valid_id(&req.id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "the id may use lowercase letters, digits and hyphens, and cannot start or end with a hyphen"})),
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
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": r.reason})))
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

/// DELETE /api/apps/custom/:id — removes the definition. Anything already installed
/// from it keeps running; it is an ordinary app now and is uninstalled like one.
pub async fn delete_custom(AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !valid_id(&id) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "unknown app"})));
    }
    match tokio::fs::remove_dir_all(custom_dir().join(&id)).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()}))),
    }
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

    /// Empty documents come from a trailing `---` and are not a mistake.
    #[test]
    fn empty_documents_are_skipped_not_rejected() {
        let padded = format!("---\n{POD}---\n");
        assert_eq!(validate_manifest(&padded), Ok(1));
    }

    // ── The boundary ──────────────────────────────────────────────────────────
    //
    // check_charts.py holds catalog charts to these rules at build time. Nothing
    // held pasted YAML to anything, so these are that same boundary applied where
    // the YAML arrives.

    #[test]
    fn cluster_scoped_kinds_are_refused() {
        for kind in ["ClusterRoleBinding", "CustomResourceDefinition", "Namespace", "PersistentVolume", "StorageClass"] {
            let doc = format!("apiVersion: v1\nkind: {kind}\nmetadata:\n  name: x\n");
            let err = validate_manifest(&doc).unwrap_err();
            assert!(err.reason.contains("whole cluster"), "{kind}: {}", err.reason);
        }
    }

    #[test]
    fn a_manifest_naming_another_namespace_is_refused() {
        let doc = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n  namespace: kube-system\n";
        let err = validate_manifest(doc).unwrap_err();
        assert!(err.reason.contains("kube-system"), "{}", err.reason);
    }

    /// Nested at pod-spec depth, which is where these actually appear — a check that
    /// only looked at the top level would pass all of them.
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
            let doc = format!("apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\n{body}");
            assert!(
                validate_manifest(&doc).is_err(),
                "{label} should be refused, but was accepted"
            );
        }
    }

    /// privileged: false is the normal case and must not be caught by a check that
    /// only looks for the key.
    #[test]
    fn privileged_false_is_fine() {
        let doc = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec:\n  template:\n    spec:\n      containers:\n        - name: c\n          securityContext:\n            privileged: false\n";
        assert!(validate_manifest(doc).is_ok());
    }

    // ── Things people will actually paste by mistake ──────────────────────────

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
        for bad in ["", "-x", "x-", "Upper", "has space", "under_score", "a/b", &"a".repeat(41)] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    /// A manifest full of Go template braces — a Grafana dashboard, a Prometheus
    /// rule — must survive as written. It is carried by `.Files.Get`, so nothing here
    /// evaluates it; this pins that it is not rejected on the way in either.

    // ── The generated chart ───────────────────────────────────────────────────

    /// Materialises a chart and renders it with the real yolab-common from this
    /// working tree. This is the assertion that matters: everything above checks the
    /// YAML going in, and this checks that what comes out is a chart Helm accepts and
    /// that the manifest survived verbatim.
    #[tokio::test]
    async fn the_generated_chart_renders_and_ships_the_manifest_untouched() {
        let Ok(helm) = which("helm") else { return };
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/catalog/yolab-common");
        if !lib.exists() {
            return; // packaged build without the catalog beside it
        }

        // Deliberately full of Go template syntax: a Prometheus rule is the everyday
        // example, and it is exactly what naive templating would destroy.
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
        write_chart_at(&tmp, &app, manifest).await.expect("chart should be written");
        let chart = tmp.join("my-thing");

        // The library is vendored the same way check_charts.py does it.
        tokio::fs::create_dir_all(chart.join("charts")).await.unwrap();
        let status = std::process::Command::new("cp")
            .args(["-r", lib.to_str().unwrap(), chart.join("charts").to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());

        let out = std::process::Command::new(helm)
            .args(["template", "rel", chart.to_str().unwrap(), "--namespace", "yolab-my-thing"])
            .output()
            .expect("helm should run");
        assert!(
            out.status.success(),
            "helm template failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rendered = String::from_utf8_lossy(&out.stdout);

        // The braces are still braces. If .Files.Get were ever replaced with an
        // include, this is the line that would fail.
        assert!(
            rendered.contains("{{ $labels.instance }} is down"),
            "the manifest was evaluated instead of shipped:\n{rendered}"
        );
        // And the gateway came along, so the app is actually reachable.
        assert!(rendered.contains("name: gateway"), "no gateway in:\n{rendered}");
        assert!(rendered.contains("localhost:8080"), "upstream missing in:\n{rendered}");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// An app with no port is a worker, not a website: it must NOT get a gateway,
    /// because a subdomain pointing at a Caddy with no upstream is a 502 with a DNS
    /// record in front of it.
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
