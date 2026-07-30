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
        bail!("kubectl apply: {}", String::from_utf8_lossy(&out.stderr).trim());
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
        bail!("kubectl create: {}", String::from_utf8_lossy(&out.stderr).trim());
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
        bail!("kubectl replace: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Read a Secret and return its decoded string data. `None` if missing.
pub async fn get_secret(
    name: &str,
    ns: &str,
) -> Option<std::collections::HashMap<String, String>> {
    let raw = run(&["get", "secret", name, "-n", ns, "-o", "json"]).await.ok()?;
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

const CEPH_NS: &str = "rook-ceph";

async fn ceph_exec_pod() -> Result<String> {
    // Get all Running-phase OSD pods as JSON and pick the first one where the
    // Ready condition is True. This skips pods in CrashLoopBackOff, which have
    // phase=Running at the pod level but no ready containers to exec into.
    let out = run(&[
        "get", "pod", "-n", CEPH_NS,
        "-l", "app=rook-ceph-osd",
        "--field-selector=status.phase=Running",
        "-o", "json",
    ]).await?;
    let pods: Value = serde_json::from_str(&out).context("parse pods JSON")?;
    for pod in pods["items"].as_array().unwrap_or(&vec![]) {
        let name = pod["metadata"]["name"].as_str().unwrap_or("");
        let ready = pod["status"]["conditions"].as_array()
            .and_then(|cs| cs.iter().find(|c| c["type"] == "Ready"))
            .and_then(|c| c["status"].as_str()) == Some("True");
        if !name.is_empty() && ready {
            return Ok(name.to_string());
        }
    }
    bail!("No ready rook-ceph-osd pod found")
}

pub async fn ceph_exec(args: &[&str]) -> Result<String> {
    let keyring_b64 = run(&[
        "get", "secret", "-n", CEPH_NS, "rook-ceph-admin-keyring",
        "-o", "jsonpath={.data.keyring}",
    ])
    .await
    .context("read admin keyring")?;

    let mon_ip = run(&[
        "get", "svc", "-n", CEPH_NS, "-l", "app=rook-ceph-mon",
        "-o", "jsonpath={.items[0].spec.clusterIP}",
    ])
    .await
    .context("find mon service")?;

    if mon_ip.is_empty() {
        bail!("Cannot find rook-ceph-mon service");
    }

    let quoted_args = args
        .iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");

    // Use a per-call random id so concurrent ceph_exec calls never race on the
    // same files inside the OSD pod. The previous code keyed these paths on
    // std::thread::current().id(), but this fn awaits (kubectl exec) and Tokio
    // multiplexes many tasks onto few worker threads — so two concurrent calls
    // could share a worker, collide on the same /tmp paths, and clobber or
    // `rm` each other's keyring mid-run.
    let uniq: u64 = rand::random();
    let key_path  = format!("/tmp/ceph-key-{uniq:016x}.keyring");
    let conf_path = format!("/tmp/ceph-conf-{uniq:016x}.conf");

    let shell_cmd = format!(
        "echo {keyring_b64} | base64 -d > {key_path} && \
         printf '[global]\\nmon_host = v2:[{mon_ip}]:3300\\n\
         ms_cluster_mode = crc\\nms_service_mode = crc\\nms_client_mode = crc\\n\
         [client.admin]\\nkeyring = {key_path}\\n' > {conf_path} && \
         ceph -c {conf_path} --name client.admin {quoted_args}; \
         RC=$?; rm -f {key_path} {conf_path}; exit $RC"
    );

    let pod = ceph_exec_pod().await?;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("kubectl")
            .args(["exec", "-n", CEPH_NS, &pod, "--", "bash", "-c", &shell_cmd])
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ceph_exec timed out after 30s"))?
    .context("kubectl exec")?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_wraps_and_escapes_quotes() {
        assert_eq!(shell_escape("plain"), "'plain'");
        // A single quote in the arg must be broken out and re-quoted.
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_escape_neutralizes_injection() {
        // The whole payload stays a single quoted literal — no unescaped metachars.
        let got = shell_escape("; rm -rf /");
        assert_eq!(got, "'; rm -rf /'");
    }
}
