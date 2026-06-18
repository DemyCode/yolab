use std::collections::{HashMap, HashSet};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, kubectl, AppState};

const CEPH_NS: &str = "rook-ceph";
const CEPH_CLUSTER: &str = "rook-ceph";
const MAX_VIRTUAL_LOOP: u32 = 7;

// ── Types ──────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiskStatus {
    /// Present but not in CephCluster spec.
    Unused,
    /// In spec, OSD not yet running.
    Joining,
    /// OSD running and `in`.
    Active,
    /// OSD `out`, data migrating off. Auto-purges once safe.
    Removing,
    /// OSD deploy exists but device not detected.
    Missing,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DiskItem {
    pub name: String,
    pub model: String,
    pub size_bytes: u64,
    pub host: String,
    pub hostname: String,
    pub status: DiskStatus,
    pub is_builtin: bool,
    pub osd_id: Option<u32>,
    pub used_bytes: Option<u64>,
    /// For Removing and Missing: whether Ceph considers this OSD safe to destroy.
    pub safe_to_destroy: Option<bool>,
    /// True when this is the only active OSD — removing it would destroy all data.
    pub last_disk: bool,
}

#[derive(Deserialize)]
pub struct DiskRequest {
    pub disk_name: String,
    pub host: String,
}

#[derive(Serialize, Deserialize)]
pub struct AddVirtualRequest {
    pub box_type: String,
    pub host: Option<String>,
}

impl AddVirtualRequest {
    fn size_gb(&self) -> u64 {
        match self.box_type.as_str() {
            "bx21" => 5120,
            "bx31" => 10240,
            "bx41" => 20480,
            _ => 1024,
        }
    }
}

// ── Block device helpers ───────────────────────────────────────────────────────

fn loop_backing_files() -> HashMap<String, String> {
    let Ok(out) = std::process::Command::new("losetup")
        .args(["-l", "--output", "NAME,BACK-FILE", "--noheadings"])
        .output()
    else { return Default::default() };
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(file)) = (parts.next(), parts.next()) {
            if name.starts_with("/dev/loop") {
                map.insert(name.trim_start_matches("/dev/").to_string(), file.to_string());
            }
        }
    }
    map
}

fn is_our_backing_file(path: &str) -> bool {
    path.starts_with("/var/lib/rook/")
}

fn scan_block_devices() -> anyhow::Result<(Vec<serde_json::Value>, HashMap<String, String>)> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"])
        .output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let devices = v["blockdevices"].as_array().cloned().unwrap_or_default();
    Ok((devices, loop_backing_files()))
}

fn is_system_disk(device: &serde_json::Value) -> bool {
    let Some(children) = device["children"].as_array() else { return false };
    for child in children {
        let mp = child["mountpoint"].as_str().unwrap_or("");
        let fstype = child["fstype"].as_str().unwrap_or("");
        if mp.starts_with("/boot") || mp == "/" || mp.starts_with("/nix") { return true; }
        if fstype == "LVM2_member" { return true; }
        if is_system_disk(child) { return true; }
    }
    false
}

/// Strip trailing partition digit: "sdb1" → "sdb". Loop devices unchanged.
fn dev_to_disk_name(dev: &str) -> &str {
    if dev.starts_with("loop") { dev } else { dev.trim_end_matches(|c: char| c.is_ascii_digit()) }
}

// ── OSD helpers ────────────────────────────────────────────────────────────────

/// Map from device name (e.g. "sdb", "sdb1", "loop0") to OSD id, built from
/// running OSD pod labels + volumes. Only populated for pods that are running.
async fn ceph_osd_map() -> HashMap<String, u32> {
    let mut mapping = HashMap::new();
    if let Ok(v) = kubectl::get_json(&["get", "pods", "-n", CEPH_NS, "-l", "app=rook-ceph-osd", "-o", "json"]).await {
        for pod in v["items"].as_array().unwrap_or(&vec![]) {
            let Some(id_str) = pod["metadata"]["labels"]["ceph-osd-id"].as_str() else { continue };
            let Ok(osd_id) = id_str.parse::<u32>() else { continue };
            for vol in pod["spec"]["volumes"].as_array().unwrap_or(&vec![]) {
                if vol["name"] == "activate-osd" {
                    if let Some(path) = vol["hostPath"]["path"].as_str() {
                        let block = std::path::Path::new(path).join("block");
                        if let Ok(resolved) = std::fs::read_link(&block) {
                            if resolved.exists() {
                                let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                                if !name.is_empty() {
                                    let base = dev_to_disk_name(&name).to_string();
                                    mapping.insert(name, osd_id);
                                    if !base.is_empty() { mapping.entry(base).or_insert(osd_id); }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if !mapping.is_empty() { return mapping; }
    if let Ok(out) = kubectl::ceph_exec(&["osd", "metadata", "--format", "json"]).await {
        if let Ok(data) = serde_json::from_str::<Vec<serde_json::Value>>(&out) {
            for osd in data {
                let Some(id) = osd["id"].as_u64() else { continue };
                for dev in osd["devices"].as_str().unwrap_or("").split(',') {
                    let dev = dev.trim().trim_start_matches("/dev/");
                    if !dev.is_empty() {
                        let base = dev_to_disk_name(dev).to_string();
                        mapping.insert(dev.to_string(), id as u32);
                        if !base.is_empty() { mapping.entry(base).or_insert(id as u32); }
                    }
                }
            }
        }
    }
    mapping
}

struct OsdDeploy {
    name: String,
    osd_id: u32,
    ready: bool,
    dev_path: String,
}

async fn list_osd_deploys() -> Vec<OsdDeploy> {
    let Ok(v) = kubectl::get_json(&[
        "get", "deploy", "-n", CEPH_NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ]).await else { return vec![] };

    v["items"].as_array().unwrap_or(&vec![]).iter().filter_map(|d| {
        let name = d["metadata"]["name"].as_str()?.to_string();
        let osd_id: u32 = d["metadata"]["labels"]["ceph-osd-id"].as_str()?.parse().ok()?;
        let ready = d["status"]["readyReplicas"].as_u64().unwrap_or(0) > 0;
        let dev_path = d["spec"]["template"]["spec"]["initContainers"]
            .as_array()?
            .iter()
            .flat_map(|c| c["env"].as_array().into_iter().flatten())
            .find(|e| e["name"] == "ROOK_BLOCK_PATH")?
            ["value"].as_str()?.to_string();
        Some(OsdDeploy { name, osd_id, ready, dev_path })
    }).collect()
}

/// Returns Some(true) = safe, Some(false) = still migrating, None = Ceph unreachable.
async fn osd_safe_to_destroy(osd_id: u32) -> Option<bool> {
    let id_str = osd_id.to_string();
    let out = kubectl::ceph_exec(&["osd", "safe-to-destroy", &id_str, "--format", "json"]).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let safe = v["safe_to_destroy"].as_array()
        .map(|arr| arr.iter().any(|id| id.as_u64() == Some(osd_id as u64)))
        .unwrap_or(false);
    Some(safe)
}

/// OSD IDs currently marked `out` in Ceph (in == 0).
async fn osd_out_ids() -> HashSet<u32> {
    let Ok(out) = kubectl::ceph_exec(&["osd", "dump", "--format", "json"]).await
    else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&out)
    else { return Default::default() };
    v["osds"].as_array().unwrap_or(&vec![])
        .iter()
        .filter_map(|o| {
            let id = o["osd"].as_u64()? as u32;
            if o["in"].as_u64().unwrap_or(1) == 0 { Some(id) } else { None }
        })
        .collect()
}

// ── CephCluster helpers ────────────────────────────────────────────────────────

/// Ceph device name for a disk. Loops use raw device; real disks use partition 1.
fn ceph_device_for(disk_name: &str) -> String {
    if disk_name.starts_with("loop") { disk_name.to_string() } else { format!("{disk_name}1") }
}

fn wipe_device(disk_name: &str) {
    if disk_name.starts_with("loop") {
        let _ = std::process::Command::new("dd")
            .args(["if=/dev/zero", &format!("of=/dev/{disk_name}"), "bs=1M", "count=100"])
            .output();
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &format!("/dev/{disk_name}")])
            .output();
    } else {
        let dev = format!("/dev/{disk_name}");
        let _ = std::process::Command::new("sgdisk").args(["-Z", &dev]).output();
        let _ = std::process::Command::new("sgdisk")
            .args(["-n", "1:0:0", "-t", "1:8300", &dev])
            .output();
        let _ = std::process::Command::new("blockdev").args(["--rereadpt", &dev]).output();
        let part_dev = format!("/dev/{disk_name}1");
        for _ in 0..10u8 {
            if std::path::Path::new(&part_dev).exists() { break; }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &part_dev])
            .output();
    }
}

fn k8s_node_name() -> String {
    std::process::Command::new("kubectl")
        .args(["get", "nodes", "-o", "jsonpath={.items[0].metadata.name}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// Read the per-node devices list for a given k8s node name.
// Falls back to global spec.storage.devices + active rook-backed loop devices.
// Per-node entries override the global deviceFilter — so when we first create
// a per-node list we must include loop devices explicitly (otherwise they'd
// be dropped since the filter no longer applies for this node).
fn cephcluster_node_devices_sync(k8s_node: &str) -> Vec<String> {
    let nodes_raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.nodes}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if let Ok(nodes) = serde_json::from_str::<Vec<serde_json::Value>>(&nodes_raw) {
        for node in &nodes {
            if node["name"].as_str() == Some(k8s_node) {
                return node["devices"].as_array().unwrap_or(&vec![])
                    .iter().filter_map(|d| d["name"].as_str().map(String::from)).collect();
            }
        }
    }
    // No per-node entry yet. Build a seed list from the global devices list
    // plus any loop devices already backed by rook files (managed via deviceFilter).
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let global: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    let mut devices: Vec<String> = global.iter()
        .filter_map(|d| d["name"].as_str().map(String::from))
        .collect();
    // Include active rook-backed loop devices so they survive the per-node override.
    for (name, backing) in loop_backing_files() {
        if is_our_backing_file(&backing) && !devices.contains(&name) {
            devices.push(name);
        }
    }
    devices
}

// Keep for disks_local() spec check (uses k8s_node_name internally).
fn cephcluster_devices_sync() -> Vec<String> {
    cephcluster_node_devices_sync(&k8s_node_name())
}

fn cephcluster_has_device_sync(ceph_dev: &str) -> bool {
    cephcluster_devices_sync().iter().any(|d| d == ceph_dev)
}

// Write the per-node devices list for a k8s node.
// Creating a per-node entry disables the global deviceFilter for that node,
// so Rook only provisions the devices we explicitly list.
fn cephcluster_set_node_devices_sync(k8s_node: &str, devices: Vec<String>) {
    let nodes_raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.nodes}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut nodes: Vec<serde_json::Value> = serde_json::from_str(&nodes_raw).unwrap_or_default();

    let dev_entries: Vec<serde_json::Value> = devices.iter()
        .map(|d| serde_json::json!({"name": d}))
        .collect();

    let mut found = false;
    for node in nodes.iter_mut() {
        if node["name"].as_str() == Some(k8s_node) {
            node["devices"] = serde_json::Value::Array(dev_entries.clone());
            found = true;
            break;
        }
    }
    if !found {
        nodes.push(serde_json::json!({"name": k8s_node, "devices": dev_entries}));
    }

    let patch = serde_json::json!({"spec": {"storage": {"nodes": nodes}}});
    let _ = std::process::Command::new("kubectl")
        .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "--type", "merge", "-p", &patch.to_string()])
        .output();
}

fn cephcluster_add_device_sync(ceph_dev: &str) {
    let k8s_node = k8s_node_name();
    let mut devices = cephcluster_node_devices_sync(&k8s_node);
    if !devices.iter().any(|d| d == ceph_dev) {
        devices.push(ceph_dev.to_string());
    }
    cephcluster_set_node_devices_sync(&k8s_node, devices);
}

async fn cephcluster_remove_device(ceph_dev: &str) {
    let ceph_dev = ceph_dev.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let k8s_node = k8s_node_name();
        let mut devices = cephcluster_node_devices_sync(&k8s_node);
        devices.retain(|d| d != &ceph_dev);
        cephcluster_set_node_devices_sync(&k8s_node, devices);
    }).await;
}

async fn purge_osd_from_ceph(osd_id: u32) {
    let id_str = osd_id.to_string();
    for cmd in [
        vec!["osd", "crush", "remove", &format!("osd.{osd_id}")],
        vec!["auth", "del", &format!("osd.{osd_id}")],
        vec!["osd", "rm", &id_str],
    ] {
        if let Err(e) = kubectl::ceph_exec(&cmd).await {
            tracing::warn!("purge osd.{osd_id}: ceph {:?} failed: {e}", cmd);
        }
    }
}

// ── Fan-out helper ─────────────────────────────────────────────────────────────

async fn gather_from_nodes(cfg: &Config, path: &str) -> Vec<(String, Vec<serde_json::Value>)> {
    let ips = kubectl::get_node_ips().await;
    let port = cfg.port;
    let futs = ips.iter().map(|ip| {
        let url = format!("http://[{ip}]:{port}{path}");
        async move {
            let items = reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(10))
                .send().await.ok()?
                .json::<Vec<serde_json::Value>>().await.ok()?;
            Some((ip.clone(), items))
        }
    });
    futures::future::join_all(futs).await.into_iter().flatten().collect()
}

// ── Main query ─────────────────────────────────────────────────────────────────

pub async fn disks_local(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let cfg = &state.config;
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();

    let (osd_map, usage, spec_devs_res, deploys, out_ids, scan_res) = tokio::join!(
        ceph_osd_map(),
        kubectl::osd_df(),
        tokio::task::spawn_blocking(cephcluster_devices_sync),
        list_osd_deploys(),
        osd_out_ids(),
        tokio::task::spawn_blocking(scan_block_devices),
    );

    let spec_devs: HashSet<String> = spec_devs_res.unwrap_or_default().into_iter().collect();
    let (devices, backing_files) = scan_res
        .unwrap_or_else(|_| Ok((vec![], Default::default())))
        .unwrap_or_default();

    // Active OSDs = running pod + OSD `in`. Used for last_disk flag.
    let active_osd_count = deploys.iter()
        .filter(|d| d.ready && !out_ids.contains(&d.osd_id))
        .count();

    let mut result: Vec<DiskItem> = vec![];

    // ── Physical devices ───────────────────────────────────────────────────────
    for d in &devices {
        let name = d["name"].as_str().unwrap_or("").to_string();
        let dtype = d["type"].as_str().unwrap_or("");
        let is_loop = dtype == "loop";
        let is_disk = dtype == "disk";
        if !is_disk && !is_loop { continue; }
        if is_disk && is_system_disk(d) { continue; }
        if is_loop {
            let backing = backing_files.get(&name).map(String::as_str).unwrap_or("");
            if !is_our_backing_file(backing) { continue; }
        }

        let is_builtin = is_loop
            && backing_files.get(&name).map(String::as_str).unwrap_or("") == "/var/lib/rook/system-osd.img";
        let model = if is_loop { "Built-in storage".to_string() }
                    else { d["model"].as_str().unwrap_or("").trim().to_string() };

        let ceph_dev = ceph_device_for(&name);
        // osd_id is only set when a pod is actually running for this device.
        let osd_id = osd_map.get(&name).copied().or_else(|| osd_map.get(&ceph_dev).copied());
        let in_spec = spec_devs.contains(&ceph_dev);
        let has_deploy = deploys.iter().any(|dep| {
            let dn = dep.dev_path.trim_start_matches("/dev/");
            dn == ceph_dev || dev_to_disk_name(dn) == name.as_str()
        });
        let is_removing = osd_id.map_or(false, |id| out_ids.contains(&id));

        let status = if is_removing {
            DiskStatus::Removing
        } else if osd_id.is_some() {
            DiskStatus::Active
        } else if in_spec || has_deploy {
            DiskStatus::Joining
        } else {
            DiskStatus::Unused
        };

        let u = osd_id.and_then(|id| usage.get(&id));
        result.push(DiskItem {
            name,
            model,
            size_bytes: d["size"].as_u64().unwrap_or(0),
            host: cfg.node_ipv6.clone(),
            hostname: hostname.clone(),
            status,
            is_builtin,
            osd_id,
            used_bytes: u.map(|u| u.used_bytes),
            safe_to_destroy: None, // filled below for Removing
            last_disk: active_osd_count <= 1
                && osd_id.is_some()
                && !is_removing,
        });
    }

    // ── Removing: check safe + auto-purge ─────────────────────────────────────
    let removing: Vec<(usize, u32, String)> = result.iter().enumerate()
        .filter_map(|(i, item)| {
            if item.status == DiskStatus::Removing {
                Some((i, item.osd_id?, ceph_device_for(&item.name)))
            } else { None }
        })
        .collect();

    let safe_checks: Vec<Option<bool>> = futures::future::join_all(
        removing.iter().map(|(_, id, _)| osd_safe_to_destroy(*id))
    ).await;

    for ((idx, osd_id, ceph_dev), safe) in removing.iter().zip(safe_checks.iter()) {
        result[*idx].safe_to_destroy = *safe;
        if *safe == Some(true) {
            let osd_id = *osd_id;
            let ceph_dev = ceph_dev.clone();
            let deploy_name = deploys.iter()
                .find(|d| d.osd_id == osd_id)
                .map(|d| d.name.clone());
            tokio::spawn(async move {
                // Remove from per-node spec FIRST — this overrides the global
                // deviceFilter so Rook won't auto-reprovision the disk.
                cephcluster_remove_device(&ceph_dev).await;
                // Give Rook time to reconcile before we delete the deploy.
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                if let Some(name) = deploy_name {
                    let _ = kubectl::run(&["delete", "deploy", "-n", CEPH_NS, &name, "--ignore-not-found"]).await;
                }
                purge_osd_from_ceph(osd_id).await;
            });
        }
    }

    // ── Missing disks (deploy exists, device gone) ─────────────────────────────
    let seen: HashSet<&str> = result.iter().map(|d| d.name.as_str()).collect();
    let missing_deploys: Vec<_> = deploys.iter().filter(|dep| {
        if std::path::Path::new(&dep.dev_path).exists() { return false; }
        let dev_name = dep.dev_path.trim_start_matches("/dev/");
        !seen.contains(dev_to_disk_name(dev_name))
    }).collect();

    let missing_safe: Vec<Option<bool>> = futures::future::join_all(
        missing_deploys.iter().map(|dep| osd_safe_to_destroy(dep.osd_id))
    ).await;

    for (dep, safe) in missing_deploys.iter().zip(missing_safe.iter()) {
        let dev_name = dep.dev_path.trim_start_matches("/dev/");
        let disk_name = dev_to_disk_name(dev_name).to_string();
        let is_loop_dev = disk_name.starts_with("loop");
        let u = usage.get(&dep.osd_id);
        result.push(DiskItem {
            name: disk_name,
            model: if is_loop_dev { "Built-in storage".to_string() } else { String::new() },
            size_bytes: u.map(|u| u.used_bytes + u.free_bytes).unwrap_or(0),
            host: cfg.node_ipv6.clone(),
            hostname: hostname.clone(),
            status: DiskStatus::Missing,
            is_builtin: dep.dev_path == "/dev/loop0",
            osd_id: Some(dep.osd_id),
            used_bytes: u.map(|u| u.used_bytes),
            safe_to_destroy: *safe,
            last_disk: false,
        });
    }

    Ok(Json(result))
}

pub async fn disks(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let node_results = gather_from_nodes(&state.config, "/api/disks/local").await;
    let mut result: Vec<DiskItem> = node_results
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().filter_map(|v| serde_json::from_value(v).ok()))
        .collect();
    result.sort_by(|a, b| a.host.cmp(&b.host).then(a.name.cmp(&b.name)));
    Ok(Json(result))
}

// ── add_disk ──────────────────────────────────────────────────────────────────
//
// Unused  → wipe disk, add to CephCluster spec (Rook handles the rest).
// Removing → bring OSD back `in` (cancel the removal).
// Fan-out: wipe must run on the disk's host; Ceph commands run from anywhere.

pub async fn add_disk(
    State(state): State<AppState>,
    Json(body): Json<DiskRequest>,
) -> Result<Json<serde_json::Value>> {
    if body.host != state.config.node_ipv6 {
        let url = format!("http://[{}]:{}/api/disks/add-local", body.host, state.config.port);
        let _ = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"disk_name": body.disk_name, "host": body.host}))
            .timeout(std::time::Duration::from_secs(60))
            .send().await;
        return Ok(Json(serde_json::json!({"ok": true})));
    }
    do_add_local(&body.disk_name).await
}

pub async fn add_disk_local(Json(body): Json<DiskRequest>) -> Result<Json<serde_json::Value>> {
    do_add_local(&body.disk_name).await
}

async fn do_add_local(disk_name: &str) -> Result<Json<serde_json::Value>> {
    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    // Check if this disk is currently Removing (OSD `out`) — cancel the drain.
    if let Some(dep) = deploys.iter().find(|d| d.dev_path == dev_path) {
        let out_ids = osd_out_ids().await;
        if out_ids.contains(&dep.osd_id) {
            let id_str = dep.osd_id.to_string();
            let _ = kubectl::ceph_exec(&["osd", "in", &id_str]).await;
            let _ = kubectl::ceph_exec(&["osd", "reweight", &id_str, "1"]).await;
            tracing::info!("add: cancelled removal of osd.{} ({disk_name})", dep.osd_id);
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        // Deploy exists and OSD is not out → already Joining/Active, nothing to do.
        return Ok(Json(serde_json::json!({"ok": true})));
    }

    // No deploy → Unused disk. Wipe and add to spec.
    let name = disk_name.to_string();
    tokio::task::spawn_blocking(move || {
        let cd = ceph_device_for(&name);
        if !cephcluster_has_device_sync(&cd) {
            wipe_device(&name);
            cephcluster_add_device_sync(&cd);
        }
    }).await.map_err(|e| anyhow::anyhow!(e))?;

    tracing::info!("add: wiped and added {disk_name} to CephCluster spec");
    Ok(Json(serde_json::json!({"ok": true})))
}

// ── remove_disk ───────────────────────────────────────────────────────────────
//
// Marks the OSD `out` so Ceph starts migrating data off it.
// Blocked when this is the last active OSD (would make all data inaccessible).
// The poll loop in disks_local() auto-purges once safe_to_destroy is true.

pub async fn remove_disk(
    State(_state): State<AppState>,
    Json(body): Json<DiskRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    let dep = match deploys.iter().find(|d| d.dev_path == dev_path) {
        Some(d) => d,
        None => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "no active OSD found for this disk"
        }))),
    };

    if !std::path::Path::new(&dep.dev_path).exists() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "disk is disconnected — use dismiss to remove it from Ceph"
        })));
    }

    // Guard: refuse if this would be the last active OSD.
    let out_ids = osd_out_ids().await;
    let active_count = deploys.iter()
        .filter(|d| std::path::Path::new(&d.dev_path).exists() && !out_ids.contains(&d.osd_id))
        .count();
    if active_count <= 1 {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "This is the only active disk. Removing it would make all data inaccessible."
        })));
    }

    let id_str = dep.osd_id.to_string();
    let _ = kubectl::ceph_exec(&["osd", "reweight", &id_str, "0"]).await;
    let _ = kubectl::ceph_exec(&["osd", "out", &id_str]).await;
    tracing::info!("remove: osd.{} ({disk_name}) marked out", dep.osd_id);

    Ok(Json(serde_json::json!({"ok": true})))
}

// ── dismiss_disk ──────────────────────────────────────────────────────────────
//
// For Missing disks only: purge OSD from Ceph and remove from spec.
// Blocked when safe_to_destroy is false (data not replicated elsewhere) or
// when Ceph is unreachable (can't verify safety).

pub async fn dismiss_disk(
    State(_state): State<AppState>,
    Json(body): Json<DiskRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    let dep = match deploys.iter().find(|d| d.dev_path == dev_path) {
        Some(d) => d,
        None => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "no OSD found for this disk — it may already be removed"
        }))),
    };

    if std::path::Path::new(&dep.dev_path).exists() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "disk is still connected — use remove instead"
        })));
    }

    match osd_safe_to_destroy(dep.osd_id).await {
        None => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "storage cluster unreachable — cannot verify data safety. Reconnect Ceph before dismissing."
        }))),
        Some(false) => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "data on this disk is not replicated elsewhere. Replug it to recover your data, or restore from backup before dismissing."
        }))),
        Some(true) => {}
    }

    let osd_id = dep.osd_id;
    let deploy_name = dep.name.clone();
    let _ = kubectl::run(&["delete", "deploy", "-n", CEPH_NS, &deploy_name, "--ignore-not-found"]).await;
    purge_osd_from_ceph(osd_id).await;
    cephcluster_remove_device(&ceph_dev).await;
    tracing::info!("dismiss: osd.{osd_id} ({disk_name}) purged and removed from spec");

    Ok(Json(serde_json::json!({"ok": true})))
}

// ── Virtual disk management ───────────────────────────────────────────────────

fn do_add_virtual_local(size_gb: u64) -> anyhow::Result<String> {
    use std::path::Path;
    let disk_num = (1u32..=MAX_VIRTUAL_LOOP)
        .find(|n| {
            let img = format!("/var/lib/rook/virtual-osd-{n}.img");
            if !Path::new(&img).exists() { return true; }
            let loop_dev = format!("/dev/loop{n}");
            let backing = std::process::Command::new("losetup")
                .args(["-l", "--output", "BACK-FILE", "--noheadings", &loop_dev])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            backing != img
        })
        .ok_or_else(|| anyhow::anyhow!("no free virtual disk slot (max {})", MAX_VIRTUAL_LOOP))?;

    let img_path = format!("/var/lib/rook/virtual-osd-{disk_num}.img");
    let loop_dev = format!("/dev/loop{disk_num}");
    let loop_name = format!("loop{disk_num}");

    std::fs::create_dir_all("/var/lib/rook")?;
    if !std::path::Path::new(&img_path).exists() {
        let size_bytes = size_gb.saturating_mul(1024 * 1024 * 1024);
        if !std::process::Command::new("fallocate")
            .args(["-l", &size_bytes.to_string(), &img_path])
            .status()?.success()
        {
            anyhow::bail!("fallocate failed for {img_path}");
        }
    }

    let current_backing = std::process::Command::new("losetup")
        .args(["-l", "--output", "BACK-FILE", "--noheadings", &loop_dev])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if current_backing == img_path { return Ok(loop_name); }

    let _ = std::process::Command::new("losetup").args(["-d", &loop_dev]).output();
    let ok = std::process::Command::new("losetup")
        .args(["--direct-io=on", &loop_dev, &img_path])
        .status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        let out = std::process::Command::new("losetup").args([&loop_dev, &img_path]).output()?;
        if !out.status.success() {
            anyhow::bail!("losetup {} {} failed: {}",
                loop_dev, img_path, String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(loop_name)
}

pub async fn add_virtual_local(
    Json(body): Json<AddVirtualRequest>,
) -> Result<Json<serde_json::Value>> {
    let size_gb = body.size_gb();
    let loop_name = tokio::task::spawn_blocking(move || do_add_virtual_local(size_gb))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({ "ok": true, "device": loop_name })))
}

pub async fn add_virtual(
    State(state): State<AppState>,
    Json(body): Json<AddVirtualRequest>,
) -> Result<Json<serde_json::Value>> {
    let result = try_provision_storage_box_result(&state.config, &body).await?;
    Ok(Json(result))
}

async fn try_provision_storage_box_result(
    cfg: &Config,
    body: &AddVirtualRequest,
) -> anyhow::Result<serde_json::Value> {
    let (url, token) = crate::routers::backups::ye_creds(cfg)
        .ok_or_else(|| anyhow::anyhow!("No account configured — connect this node to an account first"))?;

    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(format!("{url}/storage/volume"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "box_type": body.box_type }))
        .send().await
        .map_err(|e| anyhow::anyhow!("Could not reach provisioning service: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("Provisioning failed: {e}"))?
        .json::<serde_json::Value>().await
        .map_err(|e| anyhow::anyhow!("Invalid response from provisioning service: {e}"))?;

    Ok(serde_json::json!({
        "ok": true,
        "type": "storage_box",
        "host": resp["host"],
        "port": resp["port"],
        "username": resp["username"],
        "password": resp["password"],
    }))
}
