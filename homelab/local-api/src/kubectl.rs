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

pub async fn run_ok(args: &[&str]) -> bool {
    Command::new("kubectl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Read a Secret and return its base64-decoded string data, trimming trailing
/// whitespace from each value. `None` if the secret is missing or unreadable.
pub async fn get_secret(
    name: &str,
    ns: &str,
) -> Option<std::collections::HashMap<String, String>> {
    use base64::Engine as _;
    let out = Command::new("kubectl")
        .args(["get", "secret", name, "-n", ns, "-o", "json"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let data = v.get("data")?.as_object()?;
    let mut result = std::collections::HashMap::new();
    for (k, val) in data {
        if let Some(encoded) = val.as_str() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                if let Ok(s) = String::from_utf8(bytes) {
                    result.insert(k.clone(), s.trim().to_string());
                }
            }
        }
    }
    Some(result)
}

/// Create or replace an Opaque Secret from string values (base64-encoded here),
/// via a JSON manifest to avoid YAML-escaping pitfalls. `labels` are applied to
/// metadata; pass `&[]` for none.
pub async fn apply_secret(
    name: &str,
    ns: &str,
    data: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> Result<()> {
    use base64::Engine as _;
    let data_map: serde_json::Map<String, Value> = data
        .iter()
        .map(|(k, v)| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(v.as_bytes());
            (k.to_string(), Value::String(b64))
        })
        .collect();
    let label_map: serde_json::Map<String, Value> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
    let manifest = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": ns, "labels": label_map },
        "type": "Opaque",
        "data": data_map,
    });
    apply(&manifest.to_string()).await
}

pub async fn get_nodes() -> Result<Vec<Value>> {
    let v = get_json(&["get", "nodes", "-o", "json"]).await?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

pub async fn get_node_ips() -> Vec<String> {
    let Ok(nodes) = get_nodes().await else { return vec![] };
    nodes
        .iter()
        .filter_map(|n| {
            n["status"]["addresses"]
                .as_array()?
                .iter()
                .find(|a| a["type"] == "InternalIP")
                .and_then(|a| a["address"].as_str().map(String::from))
        })
        .collect()
}

// ── Ceph helpers ─────────────────────────────────────────────────────────────

const CEPH_NS: &str = "rook-ceph";

async fn ceph_exec_pod() -> Result<String> {
    let name = run(&[
        "get", "pod", "-n", CEPH_NS,
        "-l", "app=rook-ceph-osd",
        "--field-selector=status.phase=Running",
        "-o", "jsonpath={.items[0].metadata.name}",
    ])
    .await?;
    if name.is_empty() {
        bail!("No running rook-ceph-osd pod found");
    }
    Ok(name)
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

// ── Ceph exporter metrics ─────────────────────────────────────────────────────

async fn exporter_url() -> Option<String> {
    let ip = run(&[
        "get", "svc", "-n", CEPH_NS, "rook-ceph-exporter",
        "-o", "jsonpath={.spec.clusterIP}",
    ])
    .await
    .ok()?;
    if ip.is_empty() {
        return None;
    }
    Some(format!("http://[{ip}]:9926/metrics"))
}

async fn exporter_metrics() -> String {
    let Some(url) = exporter_url().await else { return String::new() };
    let Ok(resp) = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    else { return String::new() };
    resp.text().await.unwrap_or_default()
}

pub async fn osd_df() -> std::collections::HashMap<u32, OsdUsage> {
    let text = exporter_metrics().await;
    let mut total: std::collections::HashMap<u32, u64> = Default::default();
    let mut used: std::collections::HashMap<u32, u64> = Default::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ceph_osd_stat_bytes{") {
            if let Some((id, val)) = parse_osd_metric(rest) {
                total.insert(id, val);
            }
        } else if let Some(rest) = line.strip_prefix("ceph_osd_stat_bytes_used{") {
            if let Some((id, val)) = parse_osd_metric(rest) {
                used.insert(id, val);
            }
        }
    }
    total
        .into_iter()
        .map(|(id, t)| {
            let u = used.get(&id).copied().unwrap_or(0);
            (id, OsdUsage { osd_id: id, total_bytes: t, used_bytes: u, free_bytes: t.saturating_sub(u) })
        })
        .collect()
}


fn parse_osd_metric(rest: &str) -> Option<(u32, u64)> {
    let id_start = rest.find("\"osd.")? + 5;
    let id_end = rest[id_start..].find('"')? + id_start;
    let id: u32 = rest[id_start..id_end].parse().ok()?;
    let val_str = rest.split("} ").nth(1)?.trim();
    let val: f64 = val_str.parse().ok()?;
    Some((id, val as u64))
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OsdUsage {
    pub osd_id: u32,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osd_metric_extracts_id_and_value() {
        // parse_osd_metric receives the text *after* the metric-name prefix.
        let rest = r#"ceph_daemon="osd.3"} 1073741824"#;
        assert_eq!(parse_osd_metric(rest), Some((3, 1073741824)));
    }

    #[test]
    fn parse_osd_metric_rejects_garbage() {
        assert_eq!(parse_osd_metric("no osd here } 5"), None);
        assert_eq!(parse_osd_metric(r#"ceph_daemon="osd.1"}"#), None); // no value
    }

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
