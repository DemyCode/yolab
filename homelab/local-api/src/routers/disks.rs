use std::collections::HashMap;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, kubectl, AppState};

const CEPH_NS: &str = "rook-ceph";
const CEPH_CLUSTER: &str = "rook-ceph";
const MAX_VIRTUAL_LOOP: u32 = 7;

// ── Structs ───────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiskStatus {
    /// Visible but not in CephCluster — safe to unplug, shows Join button.
    Pending,
    /// In CephCluster spec, prepare job running or OSD deploy not yet ready.
    Joining,
    /// OSD deploy exists and is ready.
    Active,
    /// osd out issued, waiting for PG migration to complete.
    Draining,
    /// OSD deploy exists but the block device has disappeared.
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
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Ceph OSD id, present when the disk has an active OSD or is missing one.
    pub osd_id: Option<u32>,
    /// Populated for Missing and Draining disks: whether Ceph says it's safe to remove.
    /// None = Ceph unreachable (never allow removal without this).
    pub safe_to_destroy: Option<bool>,
}

#[derive(Deserialize)]
pub struct DiskOrderEntry {
    pub host: String,
    pub disk_name: String,
}

#[derive(Deserialize)]
pub struct DrainRequest {
    pub disk_name: String,
    pub host: String,
    pub force: Option<bool>,
}

#[derive(Serialize, Deserialize)]
pub struct AddVirtualRequest {
    /// Storage Box type: bx11 (1 TB), bx21 (5 TB), bx31 (10 TB), bx41 (20 TB)
    pub box_type: String,
    pub host: Option<String>,
}

impl AddVirtualRequest {
    fn size_gb(&self) -> u64 {
        match self.box_type.as_str() {
            "bx21" => 5120,
            "bx31" => 10240,
            "bx41" => 20480,
            _      => 1024, // bx11 default
        }
    }
}

// ── Block device helpers ──────────────────────────────────────────────────────

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

/// Strip trailing partition number: "sdb1" → "sdb". Loop devices are returned as-is.
fn dev_to_disk_name(dev: &str) -> &str {
    if dev.starts_with("loop") { dev } else { dev.trim_end_matches(|c: char| c.is_ascii_digit()) }
}

// ── Ceph OSD map ──────────────────────────────────────────────────────────────

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

// ── OSD deploy state ──────────────────────────────────────────────────────────

/// An OSD deployment as seen by Rook.
struct OsdDeploy {
    name: String,
    osd_id: u32,
    ready: bool,
    /// The block device path (ROOK_BLOCK_PATH), e.g. "/dev/sdb1" or "/dev/loop0".
    dev_path: String,
}

/// List all current OSD deployments by reading ROOK_BLOCK_PATH from init container env.
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

/// Sync version of the OSD deploy check, for use inside spawn_blocking.
fn osd_deploy_exists_for_device_sync(dev_path: &str) -> bool {
    let out = std::process::Command::new("kubectl")
        .args(["get", "deploy", "-n", CEPH_NS, "-l", "app=rook-ceph-osd",
               "-o", "jsonpath={range .items[*]}{range .spec.template.spec.initContainers[*]}\
                      {range .env[*]}{.name}={.value}{'\\n'}{end}{end}{end}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let needle = format!("ROOK_BLOCK_PATH={dev_path}");
    out.lines().any(|l| l == needle)
}



/// Ask Ceph whether an OSD can be safely destroyed without data loss.
///
/// This is the correct check before wiping a disk. `pg ls-by-osd` is WRONG:
/// when an OSD goes down its PGs are immediately re-mapped in CRUSH (showing 0
/// PGs assigned), even though the data hasn't been backfilled yet.
///
/// Returns:
///   Some(true)  — safe: all data has been replicated elsewhere
///   Some(false) — not safe: data still being migrated
///   None        — Ceph unreachable: never wipe, keep OSD safe
async fn osd_safe_to_destroy(osd_id: u32) -> Option<bool> {
    let id_str = osd_id.to_string();
    let out = kubectl::ceph_exec(&[
        "osd", "safe-to-destroy", &id_str, "--format", "json",
    ]).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    // Response: {"safe_to_destroy":[N], "active":[], "missing_stats":[], "stored_pgs":[]}
    let safe = v["safe_to_destroy"].as_array()
        .map(|arr| arr.iter().any(|id| id.as_u64() == Some(osd_id as u64)))
        .unwrap_or(false);
    Some(safe)
}

/// Query `ceph osd dump` and return the set of OSD IDs marked `out` (in == 0).
/// Returns an empty set if Ceph is unreachable — callers treat absence as Active.
async fn osd_out_ids() -> std::collections::HashSet<u32> {
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

// ── Ceph device / CephCluster helpers ────────────────────────────────────────

/// Block device name Ceph should use. Loop devices are used raw; real disks get
/// a GPT partition (BlueStore label at partition start, not absolute sector 0).
fn ceph_device_for(disk_name: &str) -> String {
    if disk_name.starts_with("loop") { disk_name.to_string() } else { format!("{disk_name}1") }
}

/// Wipe a device so Rook can use it as a fresh OSD.
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

fn cephcluster_devices_sync() -> Vec<String> {
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    devices.iter().filter_map(|d| d["name"].as_str().map(String::from)).collect()
}

fn cephcluster_has_device_sync(ceph_dev: &str) -> bool {
    cephcluster_devices_sync().iter().any(|d| d == ceph_dev)
}

/// Add a device to CephCluster.spec.storage.devices. Sync, idempotent.
fn cephcluster_add_device_sync(ceph_dev: &str) {
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    if !devices.iter().any(|d| d["name"].as_str() == Some(ceph_dev)) {
        devices.push(serde_json::json!({"name": ceph_dev}));
    }
    let patch = serde_json::json!({"spec": {"storage": {"devices": devices}}});
    let _ = std::process::Command::new("kubectl")
        .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "--type", "merge", "-p", &patch.to_string()])
        .output();
}

/// Remove a device from CephCluster.spec.storage.devices. Async, idempotent.
async fn cephcluster_remove_device(ceph_dev: &str) {
    let Ok(raw) = kubectl::run(&[
        "get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
        "-o", "jsonpath={.spec.storage.devices}",
    ]).await else { return };
    let devices: Vec<serde_json::Value> = serde_json::from_str(raw.trim()).unwrap_or_default();
    let new_devices: Vec<_> = devices.into_iter()
        .filter(|d| d["name"].as_str() != Some(ceph_dev))
        .collect();
    let patch = serde_json::json!({"spec": {"storage": {"devices": new_devices}}});
    let _ = kubectl::run(&[
        "patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
        "--type", "merge", "-p", &patch.to_string(),
    ]).await;
}

/// Purge an OSD from Ceph: crush rm + auth del + osd rm. Logs errors, never panics.
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

// ── Fan-out helpers ───────────────────────────────────────────────────────────

/// Fan out a GET request to all K8s node IPs and collect their JSON responses.
async fn gather_from_nodes(
    cfg: &Config,
    path: &str,
) -> Vec<(String, Vec<serde_json::Value>)> {
    let ips = kubectl::get_node_ips().await;
    let port = cfg.port;
    let futs = ips.iter().map(|ip| {
        let url = format!("http://[{ip}]:{port}{path}");
        async move {
            let items = reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?
                .json::<Vec<serde_json::Value>>()
                .await
                .ok()?;
            Some((ip.clone(), items))
        }
    });
    futures::future::join_all(futs).await
        .into_iter()
        .flatten()
        .collect()
}

// ── Local disk activation ─────────────────────────────────────────────────────

fn do_activate_local(disk_name: &str) -> anyhow::Result<()> {
    let ceph_dev = ceph_device_for(disk_name);
    // Idempotent: skip if already in CephCluster spec or OSD deploy exists.
    if cephcluster_has_device_sync(&ceph_dev) { return Ok(()); }
    if osd_deploy_exists_for_device_sync(&format!("/dev/{ceph_dev}")) { return Ok(()); }
    wipe_device(disk_name);
    cephcluster_add_device_sync(&ceph_dev);
    Ok(())
}

/// Activate a disk — local if host matches, otherwise fan out via HTTP.
async fn activate_disk(cfg: &Config, disk_name: &str, host: &str) {
    if host == cfg.node_ipv6 {
        let name = disk_name.to_string();
        let _ = tokio::task::spawn_blocking(move || do_activate_local(&name)).await;
    } else {
        let url = format!("http://[{host}]:{}/api/disks/activate-local", cfg.port);
        let _ = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"disk_name": disk_name, "host": host}))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

pub async fn disks_local(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let cfg = &state.config;
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let (osd_map, usage, cephcluster_devs_vec, deploys, out_ids) = tokio::join!(
        ceph_osd_map(),
        kubectl::osd_df(),
        tokio::task::spawn_blocking(cephcluster_devices_sync),
        list_osd_deploys(),
        osd_out_ids(),
    );
    let cephcluster_devs: std::collections::HashSet<String> =
        cephcluster_devs_vec.unwrap_or_default().into_iter().collect();
    let (devices, backing_files) = tokio::task::spawn_blocking(scan_block_devices).await
        .unwrap_or_else(|_| Ok((vec![], Default::default())))
        .unwrap_or_default();
    let mut result = vec![];
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
        let model = if is_loop {
            "Built-in storage".to_string()
        } else {
            d["model"].as_str().unwrap_or("").trim().to_string()
        };
        let ceph_dev = ceph_device_for(&name);
        let osd_id = osd_map.get(&name).copied();
        let is_draining = osd_id.map_or(false, |id| out_ids.contains(&id));
        let status = if is_draining {
            DiskStatus::Draining
        } else if osd_id.is_some() {
            DiskStatus::Active
        } else if cephcluster_devs.contains(&ceph_dev) {
            DiskStatus::Joining
        } else {
            DiskStatus::Pending
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
            used_bytes: u.map(|u| u.used_bytes),
            free_bytes: u.map(|u| u.free_bytes),
            osd_id,
            safe_to_destroy: None,
        });
    }

    // Check safe-to-destroy for draining disks in parallel, then apply.
    let draining_items: Vec<(usize, u32)> = result.iter().enumerate()
        .filter_map(|(i, item)| {
            if item.status == DiskStatus::Draining { Some((i, item.osd_id?)) }
            else { None }
        })
        .collect();
    let drain_safe: Vec<Option<bool>> = futures::future::join_all(
        draining_items.iter().map(|(_, id)| osd_safe_to_destroy(*id))
    ).await;
    for ((idx, _), safe) in draining_items.iter().zip(drain_safe.iter()) {
        result[*idx].safe_to_destroy = *safe;
    }

    // Add entries for OSD deploys whose block device has disappeared.
    let seen: std::collections::HashSet<&str> = result.iter().map(|d| d.name.as_str()).collect();
    let missing_deploys: Vec<_> = deploys.iter().filter(|d| {
        if std::path::Path::new(&d.dev_path).exists() { return false; }
        let dev_name = d.dev_path.trim_start_matches("/dev/");
        let disk_name = dev_to_disk_name(dev_name);
        !seen.contains(disk_name)
    }).collect();

    let safe_checks: Vec<Option<bool>> = futures::future::join_all(
        missing_deploys.iter().map(|d| osd_safe_to_destroy(d.osd_id))
    ).await;

    for (deploy, safe) in missing_deploys.iter().zip(safe_checks.iter()) {
        let dev_name = deploy.dev_path.trim_start_matches("/dev/");
        let disk_name = dev_to_disk_name(dev_name).to_string();
        let is_builtin = deploy.dev_path == "/dev/loop0";
        let is_loop = disk_name.starts_with("loop");
        let u = usage.get(&deploy.osd_id);
        result.push(DiskItem {
            name: disk_name,
            model: if is_loop { "Built-in storage".to_string() } else { String::new() },
            size_bytes: u.map(|u| u.used_bytes + u.free_bytes).unwrap_or(0),
            host: cfg.node_ipv6.clone(),
            hostname: hostname.clone(),
            status: DiskStatus::Missing,
            is_builtin,
            used_bytes: u.map(|u| u.used_bytes),
            free_bytes: u.map(|u| u.free_bytes),
            osd_id: Some(deploy.osd_id),
            safe_to_destroy: *safe,
        });
    }

    Ok(Json(result))
}

pub async fn join_disk(
    State(state): State<AppState>,
    Json(body): Json<DiskOrderEntry>,
) -> Result<Json<serde_json::Value>> {
    activate_disk(&state.config, &body.disk_name, &body.host).await;
    Ok(Json(serde_json::json!({"ok": true})))
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

pub async fn activate_local(Json(body): Json<DiskOrderEntry>) -> Result<Json<serde_json::Value>> {
    let name = body.disk_name.clone();
    tokio::task::spawn_blocking(move || do_activate_local(&name))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Called by the fan-out wipe path on non-primary nodes.
pub async fn deactivate_local(Json(body): Json<DiskOrderEntry>) -> Result<Json<serde_json::Value>> {
    let name = body.disk_name.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let ceph_dev = ceph_device_for(&name);
        wipe_device(&name);
        let raw = std::process::Command::new("kubectl")
            .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
                   "-o", "jsonpath={.spec.storage.devices}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
        let new_devices: Vec<_> = devices.into_iter()
            .filter(|d| d["name"].as_str() != Some(&ceph_dev))
            .collect();
        let patch = serde_json::json!({"spec": {"storage": {"devices": new_devices}}});
        let _ = std::process::Command::new("kubectl")
            .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
                   "--type", "merge", "-p", &patch.to_string()])
            .output();
        Ok(())
    }).await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ── Explicit drain ────────────────────────────────────────────────────────────

/// Mark an OSD out so Ceph migrates its data. Status transitions to Draining on
/// the next poll (derived from `ceph osd dump`). Use /complete-drain once
/// safe-to-destroy is true, or /cancel-drain to bring it back Active.
/// For disks with no OSD deploy (Joining state), just removes from spec.
pub async fn drain_disk(
    State(_state): State<AppState>,
    Json(body): Json<DrainRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    if let Some(deploy) = deploys.iter().find(|d| d.dev_path == dev_path) {
        if !std::path::Path::new(&deploy.dev_path).exists() {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "reason": "disk is missing — use /api/disks/remove instead"
            })));
        }
        let id_str = deploy.osd_id.to_string();
        let _ = kubectl::ceph_exec(&["osd", "reweight", &id_str, "0"]).await;
        let _ = kubectl::ceph_exec(&["osd", "out", &id_str]).await;
        tracing::info!("drain: osd.{} ({disk_name}) marked out", deploy.osd_id);
    } else {
        tracing::info!("drain: {disk_name} has no OSD deploy — removing from CephCluster spec");
        cephcluster_remove_device(&ceph_dev).await;
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

// ── Cancel drain ─────────────────────────────────────────────────────────────

/// Bring a draining OSD back in. Status returns to Active on the next poll.
pub async fn cancel_drain(
    State(_state): State<AppState>,
    Json(body): Json<DrainRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");
    if let Some(deploy) = deploys.iter().find(|d| d.dev_path == dev_path) {
        let id_str = deploy.osd_id.to_string();
        let _ = kubectl::ceph_exec(&["osd", "in", &id_str]).await;
        let _ = kubectl::ceph_exec(&["osd", "reweight", &id_str, "1"]).await;
        tracing::info!("cancel-drain: osd.{} ({disk_name}) brought back in", deploy.osd_id);
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

// ── Complete drain ────────────────────────────────────────────────────────────

/// Finalize a drain: purge the OSD from Ceph and remove from spec.
/// Only valid once safe-to-destroy is true. Use force=true to accept data loss.
pub async fn complete_drain(
    State(_state): State<AppState>,
    Json(body): Json<DrainRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let force = body.force.unwrap_or(false);

    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    let deploy = match deploys.iter().find(|d| d.dev_path == dev_path) {
        Some(d) => d,
        None => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "no OSD deploy found for this disk — it may already be removed"
        }))),
    };

    if !std::path::Path::new(&deploy.dev_path).exists() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "disk is missing — use /api/disks/remove instead"
        })));
    }

    let osd_id = deploy.osd_id;
    let deploy_name = deploy.name.clone();

    match osd_safe_to_destroy(osd_id).await {
        None => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "safe": null,
                "reason": "cluster unreachable — cannot verify it is safe to remove this disk"
            })));
        }
        Some(false) if !force => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "safe": false,
                "reason": "data on this disk has not been fully replicated elsewhere. Wait for migration to complete or use force=true to accept data loss."
            })));
        }
        _ => {}
    }

    tracing::info!("complete-drain: osd.{osd_id} ({disk_name}) — force={force}");
    let _ = kubectl::run(&["delete", "deploy", "-n", CEPH_NS, &deploy_name, "--ignore-not-found"]).await;
    purge_osd_from_ceph(osd_id).await;
    cephcluster_remove_device(&ceph_dev).await;
    tracing::info!("complete-drain: osd.{osd_id} ({disk_name}): done");

    let remaining_deploys = list_osd_deploys().await;
    let remaining_osd_count = remaining_deploys.len();
    let pool_size = kubectl::run(&[
        "get", "cephblockpool", "-n", CEPH_NS, "-o",
        "jsonpath={.items[0].spec.replicated.size}",
    ]).await.ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0);

    let pool_warning = if pool_size > 0 && remaining_osd_count < pool_size {
        Some(serde_json::json!({
            "pool_size": pool_size,
            "osd_count": remaining_osd_count,
            "message": format!(
                "Your cluster now has {} OSD(s) but pools require {} replicas. Consider reducing pool replication to match.",
                remaining_osd_count, pool_size
            )
        }))
    } else {
        None
    };

    Ok(Json(serde_json::json!({
        "ok": true,
        "pool_replication_warning": pool_warning,
    })))
}

// ── Missing disk removal ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RemoveRequest {
    pub disk_name: String,
    pub host: String,
    /// Acknowledge data loss and proceed even when safe-to-destroy is false.
    /// Required for the Velero-restore recovery path.
    pub force: Option<bool>,
}

/// Remove a disk that is in Missing state from Ceph and the CephCluster spec.
/// Checks osd safe-to-destroy first; blocks if not safe unless force=true.
/// Ceph unreachable always blocks — we refuse to remove without a health signal.
pub async fn remove_disk(
    State(state): State<AppState>,
    Json(body): Json<RemoveRequest>,
) -> Result<Json<serde_json::Value>> {
    let disk_name = &body.disk_name;
    let force = body.force.unwrap_or(false);

    let deploys = list_osd_deploys().await;
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    let deploy = match deploys.iter().find(|d| d.dev_path == dev_path) {
        Some(d) => d,
        None => return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "no OSD deploy found for this disk — it may already be removed"
        }))),
    };

    // Must actually be missing (device gone)
    if std::path::Path::new(&deploy.dev_path).exists() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "disk is present, not missing — use drain to remove an active disk"
        })));
    }

    let osd_id = deploy.osd_id;
    let deploy_name = deploy.name.clone();

    let safe = osd_safe_to_destroy(osd_id).await;

    match safe {
        None => {
            // Ceph unreachable — never remove, we can't verify cluster health
            return Ok(Json(serde_json::json!({
                "ok": false,
                "safe": null,
                "reason": "cluster unreachable — cannot verify it is safe to remove this disk. Try again when Ceph is healthy."
            })));
        }
        Some(false) if !force => {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "safe": false,
                "reason": "data on this disk has not been fully replicated elsewhere. Replug the disk to recover, or use force=true to remove and accept data loss (then restore from Velero backup)."
            })));
        }
        _ => {}
    }

    let data_loss = matches!(safe, Some(false));

    tracing::info!("remove: osd.{osd_id} ({disk_name}) — force={force}, data_loss={data_loss}");
    let _ = kubectl::run(&[
        "delete", "deploy", "-n", CEPH_NS, &deploy_name, "--ignore-not-found",
    ]).await;
    purge_osd_from_ceph(osd_id).await;
    cephcluster_remove_device(&ceph_dev).await;
    tracing::info!("remove: osd.{osd_id} ({disk_name}): done");

    // Count remaining OSD deploys to detect pool replication mismatch
    let remaining_deploys = list_osd_deploys().await;
    let remaining_osd_count = remaining_deploys.len();

    // Check CephCluster pool replication setting
    let pool_size = kubectl::run(&[
        "get", "cephblockpool", "-n", CEPH_NS, "-o",
        "jsonpath={.items[0].spec.replicated.size}",
    ]).await.ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0);

    let pool_warning = if pool_size > 0 && remaining_osd_count < pool_size {
        Some(serde_json::json!({
            "pool_size": pool_size,
            "osd_count": remaining_osd_count,
            "message": format!(
                "Your cluster now has {} OSD(s) but pools require {} replicas. \
                 Consider reducing pool replication to match.",
                remaining_osd_count, pool_size
            )
        }))
    } else {
        None
    };

    Ok(Json(serde_json::json!({
        "ok": true,
        "data_loss_warning": data_loss,
        "pool_replication_warning": pool_warning,
    })))
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

    let box_type = body.box_type.as_str();

    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(format!("{url}/storage/volume"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "box_type": box_type }))
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
