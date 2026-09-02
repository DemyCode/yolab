// kube-rs failed to connect to https://[::1]:6443 in IPv6 environments; all
// cluster access goes through kubectl which works correctly.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

pub async fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("kubectl")
        .args(args)
        .output()
        .await
        .with_context(|| format!("kubectl {}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        bail!(
            "kubectl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

pub async fn get_json(args: &[&str]) -> Result<Value> {
    let out = run(args).await?;
    serde_json::from_str(&out).context("JSON parse")
}

/// Whether a `kubectl get` failure means "this object does not exist" rather
/// than "the API server did not answer".
///
/// The two must never be confused where the caller maps the result onto a
/// default: NotFound is the genuine first-run state and may be read as empty;
/// an unreachable API server must stay "unknown" so a brief outage is not read
/// as "every disk switched off" (which ends in a drain/purge/wipe — see
/// disks_reconciler::read_desired).
pub fn is_not_found(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("NotFound")
}

// ── Shared apply / secret helpers ─────────────────────────────────────────────
//
// One implementation for the whole crate. Previously auth.rs, backups.rs, and
// apps.rs each carried their own copies (auth.rs even shelled out to `base64 -d`
// on the load path); those all funnel here now.

/// Pipe a manifest to `kubectl apply -f -`.
pub async fn apply(manifest: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn kubectl apply")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(manifest.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "kubectl apply: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Pipe a manifest to `kubectl create -f -`.
/// Returns Err if the resource already exists (409) or any other failure.
pub async fn create(manifest: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new("kubectl")
        .args(["create", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn kubectl create")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(manifest.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "kubectl create: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Pipe a manifest to `kubectl replace -f -`.
/// Requires `metadata.resourceVersion` in the manifest; fails with 409 if
/// another writer has since modified the resource (optimistic concurrency CAS).
pub async fn replace(manifest: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new("kubectl")
        .args(["replace", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn kubectl replace")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(manifest.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "kubectl replace: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Read a Secret and return its decoded string data. `None` if missing.
pub async fn get_secret(name: &str, ns: &str) -> Option<std::collections::HashMap<String, String>> {
    let raw = run(&["get", "secret", name, "-n", ns, "-o", "json"])
        .await
        .ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let mut result = std::collections::HashMap::new();
    if let Some(data) = v["data"].as_object() {
        for (k, val) in data {
            let b64 = val.as_str().unwrap_or("");
            let bytes = base64_decode(b64);
            if let Ok(s) = String::from_utf8(bytes) {
                result.insert(k.clone(), s.trim().to_string());
            }
        }
    }
    Some(result)
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .unwrap_or_default()
}

/// Create or replace an Opaque Secret by generating a kubectl manifest and
/// piping it to `kubectl apply -f -`. Labels are applied to metadata.
pub async fn apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> Result<()> {
    use base64::Engine as _;
    let mut entries = serde_json::Map::new();
    for (k, v) in data {
        let encoded = base64::engine::general_purpose::STANDARD.encode(v.as_bytes());
        entries.insert(k.to_string(), Value::String(encoded));
    }
    let mut label_map = serde_json::Map::new();
    for (k, v) in labels {
        label_map.insert(k.to_string(), Value::String(v.to_string()));
    }
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": ns,
            "labels": label_map,
        },
        "data": entries,
        "type": "Opaque",
    });
    apply(&manifest.to_string()).await
}

pub async fn get_nodes() -> Result<Vec<Value>> {
    let raw = run(&["get", "nodes", "-o", "json"]).await?;
    let v: Value = serde_json::from_str(&raw).context("parse kubectl get nodes JSON")?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

// ── Ceph helpers ─────────────────────────────────────────────────────────────

/// Run a `ceph` command.
///
/// Kept under this name and signature so its ~10 call sites did not all have to
/// change, but the implementation is now a plain local subprocess: Ceph runs as
/// host daemons rather than Rook pods (see homelab/nixos/ceph/), so there is no
/// pod to exec into any more. The old pod-exec implementation (`ceph_exec_pod`
/// / `ceph_exec_via_pod`, dead since that move, with no other caller) was
/// removed rather than kept "just in case" — it shelled a keyring into a Rook
/// OSD pod that no longer exists on a host-Ceph node.
pub async fn ceph_exec(args: &[&str]) -> Result<String> {
    crate::ceph_cli::ceph(args).await
}

// ── Generic CRD helpers (yolab.io custom resources) ──────────────────────────
//
// BackupRun/RestoreRun (see routers/backup_run.rs, routers/restore_run.rs) are
// schemaless CRDs (`x-kubernetes-preserve-unknown-fields`) — spec/status are just
// serde_json::Value, so one small generic client covers both instead of hand-rolled
// get/create/patch boilerplate per kind.

#[derive(Clone, Copy)]
pub struct Crd {
    pub group: &'static str,
    pub version: &'static str,
    pub plural: &'static str,
    pub kind: &'static str,
}

impl Crd {
    fn res(&self) -> String {
        format!("{}.{}", self.plural, self.group)
    }

    fn api_version(&self) -> String {
        format!("{}/{}", self.group, self.version)
    }

    pub async fn get(&self, name: &str) -> Option<Value> {
        let raw = run(&["get", &self.res(), name, "-o", "json", "--ignore-not-found"])
            .await
            .ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        serde_json::from_str(&raw).ok()
    }

    /// Lists every object of this kind (cluster-scoped), newest-created first.
    pub async fn list(&self) -> Vec<Value> {
        let raw = match run(&["get", &self.res(), "-o", "json"]).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut items = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default();
        items.sort_by(|a, b| {
            let ta = a["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            let tb = b["metadata"]["creationTimestamp"].as_str().unwrap_or("");
            tb.cmp(ta)
        });
        items
    }

    pub async fn create(&self, name: &str, spec: Value, labels: &[(&str, &str)]) -> Result<()> {
        let mut label_map = serde_json::Map::new();
        for (k, v) in labels {
            label_map.insert(k.to_string(), Value::String(v.to_string()));
        }
        let manifest = serde_json::json!({
            "apiVersion": self.api_version(),
            "kind": self.kind,
            "metadata": { "name": name, "labels": label_map },
            "spec": spec,
        });
        create(&manifest.to_string()).await
    }

    /// Merge-patches `.status`. Requires the CRD's status subresource (both CRDs do).
    pub async fn patch_status(&self, name: &str, status: Value) -> Result<()> {
        let patch = serde_json::json!({ "status": status }).to_string();
        run(&[
            "patch",
            &self.res(),
            name,
            "--type=merge",
            "--subresource=status",
            "-p",
            &patch,
        ])
        .await
        .map(|_| ())
    }

    pub async fn delete(&self, name: &str) {
        let _ = run(&["delete", &self.res(), name, "--ignore-not-found"]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_not_found ─────────────────────────────────────────────────────────
    //
    // This is the one place in the crate that reads a failed `kubectl get` and
    // has to say whether the resource was absent or the server was unreachable.
    // A missing ConfigMap means "fresh install, safe to treat as empty"; a
    // broken connection must never be read that way.

    #[test]
    fn a_kubectl_not_found_is_a_missing_resource() {
        let e = anyhow::anyhow!(
            "kubectl get configmap yolab-disk-config -n rook-ceph -o json: \
             Error from server (NotFound): configmaps \"yolab-disk-config\" not found"
        );
        assert!(is_not_found(&e));
    }

    #[test]
    fn a_connection_failure_is_not_a_missing_resource() {
        for msg in [
            "kubectl get configmap yolab-disk-config: The connection to the server \
             localhost:6443 was refused - did you specify the right host or port?",
            "kubectl get configmap yolab-disk-config: context deadline exceeded",
            "kubectl get configmap yolab-disk-config: unable to connect to the server: EOF",
        ] {
            let e = anyhow::anyhow!("{msg}");
            assert!(!is_not_found(&e), "{msg}");
        }
    }
}
