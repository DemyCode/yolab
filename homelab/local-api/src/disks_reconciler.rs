/// Disk reconciler — runs as a background task inside yolab-local-api.
/// Replaces the former Python osd-node-controller DaemonSet.
///
/// Every 30 seconds on each node:
///   1. Discover block devices from /sys/block
///   2. Classify them (ours / clean / foreign-wipe)
///   3. Publish metadata to yolab-disk-status ConfigMap
///   4. Read desired states from yolab-disk-config ConfigMap
///   5. Patch CephCluster CR to match (USING = include, OFF = exclude)
///   6. Migrate OSD Deployments if a disk moved to this node
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::Path,
};
use tokio::time::{sleep, Duration};

use crate::kubectl;

const NS: &str = "rook-ceph";
const CLUSTER: &str = "rook-ceph";
const STATUS_CM: &str = "yolab-disk-status";
const CONFIG_CM: &str = "yolab-disk-config";
const INTERVAL_SECS: u64 = 30;

const BLUESTORE_MAGIC: &[u8] = b"bluestore block device\n";
const CEPH_FSID_KEY: &[u8] = b"\x09\x00\x00\x00ceph_fsid";

pub async fn run() {
    let node = match node_name() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("disk reconciler: cannot determine node name: {e}");
            return;
        }
    };
    tracing::info!("disk reconciler started on {node}");
    loop {
        if let Err(e) = reconcile(&node).await {
            tracing::warn!("disk reconcile: {e}");
        }
        sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

async fn reconcile(node: &str) -> Result<()> {
    let devices = get_devices();
    let our_fsid = cluster_fsid().await.unwrap_or_default();

    // Publish metadata for the UI
    let meta: HashMap<String, Value> = devices
        .iter()
        .map(|d| (disk_id(d), disk_meta(d, &our_fsid)))
        .collect();
    write_status(node, &meta).await;

    // Read desired states; missing key → default USING
    let desired = read_desired().await;

    let effective: Vec<String> = classify(&devices, &our_fsid)
        .into_iter()
        .filter(|d| {
            let key = format!("{}--{}", node, disk_id(d));
            desired.get(&key).map(|v| v == "USING").unwrap_or(true)
        })
        .collect();

    patch_cephcluster(node, &effective).await?;

    if !our_fsid.is_empty() {
        migrate_osd_deployments(node, &our_fsid).await;
    }
    Ok(())
}

// ── Device discovery ──────────────────────────────────────────────────────────

fn get_devices() -> Vec<String> {
    let mut devices = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/block") else { return devices };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if is_physical_disk(&name) {
            devices.push(name);
        } else if name.starts_with("loop")
            && Path::new(&format!("/sys/block/{name}/loop/backing_file")).exists()
        {
            devices.push(name);
        }
    }
    devices.sort();
    devices
}

fn is_physical_disk(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("sd") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase());
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        return rest.contains('n') && rest.bytes().all(|b| b.is_ascii_digit() || b == b'n');
    }
    if let Some(rest) = name.strip_prefix("vd") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase());
    }
    false
}

// ── BlueStore label parsing ───────────────────────────────────────────────────

fn read_bluestore_header(device: &str) -> Option<[u8; 4096]> {
    let mut buf = [0u8; 4096];
    let mut f = std::fs::File::open(format!("/dev/{device}")).ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn bluestore_fsid(device: &str) -> Option<String> {
    let buf = read_bluestore_header(device)?;
    if !buf.starts_with(BLUESTORE_MAGIC) {
        return None;
    }
    let pos = buf.windows(CEPH_FSID_KEY.len()).position(|w| w == CEPH_FSID_KEY)?;
    let vs = pos + CEPH_FSID_KEY.len();
    if vs + 40 > buf.len() {
        return None;
    }
    if u32::from_le_bytes(buf[vs..vs + 4].try_into().ok()?) != 36 {
        return None;
    }
    let fsid = std::str::from_utf8(&buf[vs + 4..vs + 40]).ok()?;
    is_uuid(fsid).then(|| fsid.to_string())
}

fn bluestore_osd_uuid(device: &str) -> Option<String> {
    let buf = read_bluestore_header(device)?;
    if !buf.starts_with(BLUESTORE_MAGIC) {
        return None;
    }
    let uuid = std::str::from_utf8(&buf[23..59]).ok()?;
    is_uuid(uuid).then(|| uuid.to_string())
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&l, p)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn wipe_device(device: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/dev/{device}"))
        .context("open device")?;
    let chunk = [0u8; 64 * 1024];
    for _ in 0..160 {
        // 160 × 64 KB = 10 MB
        f.write_all(&chunk).context("write zeros")?;
    }
    Ok(())
}

// ── Classify devices ──────────────────────────────────────────────────────────

fn classify(devices: &[String], our_fsid: &str) -> Vec<String> {
    let mut effective = Vec::new();
    for device in devices {
        if device.starts_with("loop") {
            effective.push(device.clone());
            continue;
        }
        match bluestore_fsid(device).as_deref() {
            None => effective.push(device.clone()),
            Some(fsid) if fsid == our_fsid => {
                tracing::debug!("{device}: our OSD, Rook will re-integrate");
                effective.push(device.clone());
            }
            Some(other) => {
                tracing::warn!("{device}: foreign BlueStore ({other}) — wiping");
                if let Err(e) = wipe_device(device) {
                    tracing::error!("wipe {device}: {e}");
                }
                // excluded this cycle; re-added clean next cycle
            }
        }
    }
    effective.sort();
    effective
}

// ── Stable disk identity ──────────────────────────────────────────────────────

fn disk_id(device: &str) -> String {
    if device.starts_with("loop") {
        let bf = std::fs::read_to_string(format!("/sys/block/{device}/loop/backing_file"))
            .unwrap_or_default();
        let name = Path::new(bf.trim())
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| device.to_string());
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .to_lowercase();
        return format!("loop-{}", safe.trim_matches('-'));
    }
    if let Ok(serial) = std::fs::read_to_string(format!("/sys/block/{device}/device/serial")) {
        let s = serial.trim();
        if !s.is_empty() {
            let safe: String = s
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
                .to_lowercase();
            return format!("serial-{}", safe.trim_matches('-'));
        }
    }
    format!("dev-{device}")
}

fn disk_meta(device: &str, our_fsid: &str) -> Value {
    let is_loop = device.starts_with("loop");
    let model = if is_loop {
        "System disk".to_string()
    } else {
        std::fs::read_to_string(format!("/sys/block/{device}/device/model"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let size_bytes: u64 = std::fs::read_to_string(format!("/sys/block/{device}/size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 512;
    let is_our_osd = bluestore_fsid(device)
        .map(|f| f == our_fsid)
        .unwrap_or(false);
    json!({"device": device, "model": model, "size_bytes": size_bytes, "is_loop": is_loop, "is_our_osd": is_our_osd})
}

// ── ConfigMap helpers ─────────────────────────────────────────────────────────

async fn cluster_fsid() -> Option<String> {
    let fsid = kubectl::run(&[
        "get",
        "cephcluster",
        CLUSTER,
        "-n",
        NS,
        "-o",
        "jsonpath={.status.ceph.fsid}",
    ])
    .await
    .ok()?;
    if fsid.is_empty() { None } else { Some(fsid) }
}

async fn write_status(node: &str, meta: &HashMap<String, Value>) {
    let json_val = serde_json::to_string(meta).unwrap_or_default();
    let patch = json!({"data": {node: json_val}}).to_string();
    if kubectl::run(&[
        "patch",
        "configmap",
        STATUS_CM,
        "-n",
        NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await
    .is_err()
    {
        // ConfigMap doesn't exist yet — create it
        let _ = kubectl::run(&["create", "configmap", STATUS_CM, "-n", NS]).await;
        let _ = kubectl::run(&[
            "patch",
            "configmap",
            STATUS_CM,
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await;
    }
}

async fn read_desired() -> HashMap<String, String> {
    kubectl::get_json(&[
        "get",
        "configmap",
        CONFIG_CM,
        "-n",
        NS,
        "-o",
        "jsonpath={.data}",
    ])
    .await
    .ok()
    .and_then(|v| serde_json::from_value(v).ok())
    .unwrap_or_default()
}

// ── CephCluster CR patching ───────────────────────────────────────────────────

async fn patch_cephcluster(node: &str, devices: &[String]) -> Result<()> {
    let cr = kubectl::get_json(&["get", "cephcluster", CLUSTER, "-n", NS, "-o", "json"]).await?;

    let mut nodes: Vec<Value> = cr["spec"]["storage"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let idx = nodes.iter().position(|n| n["name"].as_str() == Some(node));

    let mut current: Vec<String> = idx
        .map(|i| {
            nodes[i]["devices"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|d| d["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    current.sort();

    let mut want = devices.to_vec();
    want.sort();

    if current == want && idx.map(|i| !nodes[i].get("deviceFilter").is_some()).unwrap_or(true) {
        return Ok(());
    }

    let devs: Vec<Value> = devices.iter().map(|d| json!({"name": d})).collect();
    if let Some(i) = idx {
        nodes[i]["devices"] = json!(devs);
        if let Some(obj) = nodes[i].as_object_mut() {
            obj.remove("deviceFilter");
        }
    } else {
        nodes.push(json!({"name": node, "devices": devs}));
    }

    let patch = json!({"spec": {"storage": {"nodes": nodes}}}).to_string();
    for attempt in 0..5u32 {
        match kubectl::run(&[
            "patch",
            "cephcluster",
            CLUSTER,
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        {
            Ok(_) => {
                tracing::info!("CephCluster patched: {node} → {devices:?}");
                return Ok(());
            }
            Err(e) if attempt < 4 && e.to_string().contains("conflict") => {
                sleep(Duration::from_millis(500 + u64::from(attempt) * 500)).await;
            }
            Err(e) => bail!("patch CephCluster: {e}"),
        }
    }
    bail!("patch CephCluster: too many conflicts")
}

// ── OSD Deployment migration (disk moved between nodes) ───────────────────────

async fn migrate_osd_deployments(node: &str, our_fsid: &str) {
    // Only run if Rook's prepare job for this node has completed
    let job_done = kubectl::run(&[
        "get",
        "job",
        &format!("rook-ceph-osd-prepare-{node}"),
        "-n",
        NS,
        "-o",
        "jsonpath={.status.completionTime}",
    ])
    .await
    .map(|s| !s.is_empty())
    .unwrap_or(false);

    if !job_done {
        return;
    }

    // Map OSD UUID → device for disks physically present on this node
    let devices = get_devices();
    let our_uuids: HashMap<String, String> = devices
        .iter()
        .filter(|d| !d.starts_with("loop"))
        .filter(|d| bluestore_fsid(d).as_deref() == Some(our_fsid))
        .filter_map(|d| bluestore_osd_uuid(d).map(|u| (u, d.clone())))
        .collect();

    if our_uuids.is_empty() {
        return;
    }

    let Ok(deploys) = kubectl::get_json(&[
        "get",
        "deployments",
        "-n",
        NS,
        "-l",
        "app=rook-ceph-osd",
        "-o",
        "json",
    ])
    .await
    else {
        return;
    };

    let items = deploys["items"].as_array().cloned().unwrap_or_default();
    for deploy in items {
        let name = deploy["metadata"]["name"].as_str().unwrap_or("");
        let containers = &deploy["spec"]["template"]["spec"]["containers"];
        let env = containers[0]["env"].as_array().cloned().unwrap_or_default();

        let osd_uuid = env
            .iter()
            .find(|e| e["name"] == "ROOK_OSD_UUID")
            .and_then(|e| e["value"].as_str())
            .unwrap_or("");

        if !our_uuids.contains_key(osd_uuid) {
            continue;
        }

        let current_node = deploy["spec"]["template"]["spec"]["nodeSelector"]
            ["kubernetes.io/hostname"]
            .as_str()
            .unwrap_or("");
        if current_node == node {
            continue;
        }

        tracing::info!("{name}: disk {osd_uuid} moved from {current_node} → {node}, patching");
        patch_osd_deployment(name, &env, node).await;
    }
}

async fn patch_osd_deployment(deploy_name: &str, env: &[Value], new_node: &str) {
    // Update ROOK_NODE_NAME / ROOK_CRUSHMAP_HOSTNAME, preserve others
    let new_env: Vec<Value> = env
        .iter()
        .map(|e| {
            let name = e["name"].as_str().unwrap_or("");
            if name == "ROOK_NODE_NAME" || name == "ROOK_CRUSHMAP_HOSTNAME" {
                json!({"name": name, "value": new_node})
            } else {
                e.clone()
            }
        })
        .collect();

    let patch = json!({
        "spec": {"template": {"spec": {
            "nodeSelector": {"kubernetes.io/hostname": new_node},
            "containers": [{"name": "osd", "env": new_env}],
        }}}
    })
    .to_string();

    for attempt in 0..5u32 {
        match kubectl::run(&[
            "patch",
            "deployment",
            deploy_name,
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        {
            Ok(_) => {
                tracing::info!("{deploy_name} patched → {new_node}");
                return;
            }
            Err(e) if attempt < 4 && e.to_string().contains("conflict") => {
                sleep(Duration::from_millis(500 + u64::from(attempt) * 500)).await;
            }
            Err(e) => {
                tracing::error!("patch {deploy_name}: {e}");
                return;
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn node_name() -> Result<String> {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .context("read /etc/hostname")
}
