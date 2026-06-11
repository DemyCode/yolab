use anyhow::{anyhow, bail, Context, Result};
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

    let shell_cmd = format!(
        "echo {keyring_b64} | base64 -d > /tmp/k && \
         printf '[global]\\nmon_host = v2:[{mon_ip}]:3300\\n\
         ms_cluster_mode = crc\\nms_service_mode = crc\\nms_client_mode = crc\\n\
         [client.admin]\\nkeyring = /tmp/k\\n' > /tmp/ceph.conf && \
         ceph -c /tmp/ceph.conf --name client.admin {quoted_args}"
    );

    let pod = ceph_exec_pod().await?;
    let out = Command::new("kubectl")
        .args(["exec", "-n", CEPH_NS, &pod, "--", "bash", "-c", &shell_cmd])
        .output()
        .await
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
    reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()
        .and_then(|r| futures::executor::block_on(r.text()).ok())
        .unwrap_or_default()
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

pub async fn osd_numpg() -> std::collections::HashMap<u32, u32> {
    let text = exporter_metrics().await;
    let mut result = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("ceph_osd_numpg{") {
            if let Some((id, val)) = parse_osd_metric(rest) {
                result.insert(id, val as u32);
            }
        }
    }
    result
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
