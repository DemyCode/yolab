use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, kubectl, priority, AppState};

// Tracks OSD IDs currently being drained so reconcile_storage (which fires
// every 30 s) never spawns a second task for the same OSD.
static DRAINING_OSDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
fn draining_osds() -> &'static Mutex<HashSet<u32>> {
    DRAINING_OSDS.get_or_init(|| Mutex::new(HashSet::new()))
}

const CEPH_NS: &str = "rook-ceph";
const CEPH_CLUSTER: &str = "rook-ceph";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DiskItem {
    pub name: String,
    pub model: String,
    pub size_bytes: u64,
    pub host: String,
    pub hostname: String,
    pub is_osd: bool,
    pub is_builtin: bool,
    pub used_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Deserialize)]
pub struct DiskOrderEntry {
    pub host: String,
    pub disk_name: String,
}

#[derive(Deserialize)]
pub struct DiskOrderRequest {
    pub entries: Vec<DiskOrderEntry>,
}

fn lsblk() -> anyhow::Result<Vec<serde_json::Value>> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,MODEL,FSTYPE"])
        .output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(v["blockdevices"].as_array().cloned().unwrap_or_default())
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

/// Strip a trailing partition number from a device name so that e.g. "sdb1"
/// maps back to "sdb". This lets callers match the whole-disk name shown in
/// the UI even when Ceph is running on a partition.
fn disk_base_name(dev: &str) -> &str {
    dev.trim_end_matches(|c: char| c.is_ascii_digit())
}

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
                        if let Ok(resolved) = std::fs::read_link(&block).or_else(|_| Ok::<_, std::io::Error>(block)) {
                            let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                            if !name.is_empty() {
                                // Map both the exact device (e.g. sdb1) and its
                                // parent disk (e.g. sdb) so the UI sees the disk
                                // as an OSD regardless of whether a partition is used.
                                let base = disk_base_name(&name).to_string();
                                mapping.insert(name, osd_id);
                                if !base.is_empty() { mapping.entry(base).or_insert(osd_id); }
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
                        let base = disk_base_name(dev).to_string();
                        mapping.insert(dev.to_string(), id as u32);
                        if !base.is_empty() { mapping.entry(base).or_insert(id as u32); }
                    }
                }
            }
        }
    }
    mapping
}

async fn node_ips(cfg: &Config) -> Vec<String> {
    let mut ips = vec![cfg.node_ipv6.clone()];
    for node in kubectl::get_nodes().await.unwrap_or_default() {
        if let Some(addr) = node["status"]["addresses"].as_array()
            .and_then(|a| a.iter().find(|a| {
                a["type"] == "InternalIP" && a["address"].as_str().map(|s| s.contains(':')).unwrap_or(false)
            }))
            .and_then(|a| a["address"].as_str())
        {
            if addr != cfg.node_ipv6 { ips.push(addr.to_string()); }
        }
    }
    ips
}

async fn gather_from_nodes(cfg: &Config, path: &str) -> Vec<(String, Vec<serde_json::Value>)> {
    let ips = node_ips(cfg).await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let futs = ips.iter().map(|ip| {
        let url = format!("http://[{}]:{}{}", ip, cfg.port, path);
        let client = client.clone();
        let ip = ip.clone();
        async move {
            let Ok(resp) = client.get(&url).send().await else { return None };
            let Ok(data) = resp.json::<Vec<serde_json::Value>>().await else { return None };
            Some((ip, data))
        }
    });
    futures::future::join_all(futs).await.into_iter().flatten().collect()
}

/// Return the block device name that Ceph should use for the given disk.
/// Loop devices are used directly; real disks get a GPT partition so the
/// BlueStore label lands at partition-start (sector 2048+) rather than at
/// absolute sector 0, where some firmware/USB bridges silently discard writes.
fn ceph_device_for(disk_name: &str) -> String {
    if disk_name.starts_with("loop") {
        disk_name.to_string()
    } else {
        format!("{disk_name}1")
    }
}

fn do_activate_local(disk_name: &str) -> anyhow::Result<()> {
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let job_active = std::process::Command::new("kubectl")
        .args(["get", "job", "-n", CEPH_NS, &format!("rook-ceph-osd-prepare-{hostname}"),
               "-o", "jsonpath={.status.active}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string() == "1")
        .unwrap_or(false);
    if job_active { return Ok(()); }

    if disk_name.starts_with("loop") {
        let _ = std::process::Command::new("dd")
            .args(["if=/dev/zero", &format!("of=/dev/{disk_name}"), "bs=1M", "count=100"])
            .output();
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &format!("/dev/{disk_name}")])
            .output();
    } else {
        // Real disk: destroy existing partition table, then create a single GPT
        // partition. The BlueStore label will be written at the partition start
        // (typically sector 2048), not at disk sector 0, which is unreliable on
        // some USB bridges and SMR drives.
        let dev = format!("/dev/{disk_name}");
        let _ = std::process::Command::new("sgdisk").args(["-Z", &dev]).output();
        let _ = std::process::Command::new("sgdisk")
            .args(["-n", "1:0:0", "-t", "1:8300", &dev])
            .output();
        // Ask kernel to re-read the partition table.
        let _ = std::process::Command::new("blockdev")
            .args(["--rereadpt", &dev])
            .output();
        // Wait up to 5 s for the partition device to appear.
        let part_dev = format!("/dev/{disk_name}1");
        for _ in 0..10u8 {
            if std::path::Path::new(&part_dev).exists() { break; }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &part_dev])
            .output();
    }

    let ceph_dev = ceph_device_for(disk_name);
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    // Remove any stale entry for either the raw disk or the partition.
    devices.retain(|d| {
        let n = d["name"].as_str().unwrap_or("");
        n != disk_name && n != ceph_dev
    });
    devices.push(serde_json::json!({"name": ceph_dev}));
    let patch = serde_json::json!({"spec": {"storage": {"devices": devices}}});
    let _ = std::process::Command::new("kubectl")
        .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "--type", "merge", "-p", &patch.to_string()])
        .output();
    let _ = std::process::Command::new("kubectl")
        .args(["delete", "job", "-n", CEPH_NS,
               &format!("rook-ceph-osd-prepare-{hostname}"), "--ignore-not-found"])
        .output();
    Ok(())
}

fn do_deactivate_local(disk_name: &str) -> anyhow::Result<()> {
    let ceph_dev = ceph_device_for(disk_name);
    if disk_name.starts_with("loop") {
        let _ = std::process::Command::new("dd")
            .args(["if=/dev/zero", &format!("of=/dev/{disk_name}"), "bs=1M", "count=100"])
            .output();
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &format!("/dev/{disk_name}")])
            .output();
    } else {
        // Wipe the partition, then destroy the partition table so the disk is
        // fully blank and ready for future activation.
        let part_dev = format!("/dev/{ceph_dev}");
        let _ = std::process::Command::new("wipefs")
            .args(["--all", "--force", &part_dev])
            .output();
        let _ = std::process::Command::new("sgdisk")
            .args(["-Z", &format!("/dev/{disk_name}")])
            .output();
    }
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    // Remove both the raw-disk name and the partition name in case either was stored.
    let new_devices: Vec<_> = devices.into_iter()
        .filter(|d| {
            let n = d["name"].as_str().unwrap_or("");
            n != disk_name && n != ceph_dev
        }).collect();
    let patch = serde_json::json!({"spec": {"storage": {"devices": new_devices}}});
    let _ = std::process::Command::new("kubectl")
        .args(["patch", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "--type", "merge", "-p", &patch.to_string()])
        .output();
    Ok(())
}

fn node_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn activate_disk(cfg: &Config, disk_name: &str, host: &str) {
    if host != cfg.node_ipv6 {
        let _ = node_client()
            .post(format!("http://[{}]:{}/api/disks/activate-local", host, cfg.port))
            .json(&serde_json::json!({"host": host, "disk_name": disk_name}))
            .send().await;
        return;
    }
    let name = disk_name.to_string();
    tokio::task::spawn_blocking(move || { let _ = do_activate_local(&name); }).await.ok();
}

async fn drain_osd(cfg: Arc<Config>, disk_name: String, osd_id: u32, host: String) {
    let id_str = osd_id.to_string();
    if kubectl::ceph_exec(&["osd", "reweight", &id_str, "0"]).await.is_err() {
        tracing::warn!("drain osd.{osd_id}: failed to reweight, aborting");
        draining_osds().lock().unwrap().remove(&osd_id);
        return;
    }
    let _ = kubectl::ceph_exec(&["osd", "out", &id_str]).await;
    tracing::info!("drain osd.{osd_id} ({disk_name}): marked out, waiting for PGs to migrate");

    // Wait without any timeout until every PG has migrated off this OSD.
    // This is intentional: removing an OSD before its PGs are gone causes
    // permanent data loss when running single-replica pools.  It is safe to
    // wait indefinitely — the OSD stays up and serving data the whole time.
    //
    // Safety: if the Prometheus exporter is unavailable, osd_numpg() returns
    // an empty map.  We must NOT treat that as "0 PGs" (which would cause
    // immediate data-loss wipe).  Instead we fall back to asking Ceph directly
    // via `ceph pg ls-by-osd`, and if that also fails we keep waiting.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let numpg_map = kubectl::osd_numpg().await;
        let numpg = if numpg_map.is_empty() {
            match kubectl::ceph_osd_numpg_direct(osd_id).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("drain osd.{osd_id}: exporter and direct Ceph query both unavailable ({e}), keeping OSD safe");
                    u32::MAX
                }
            }
        } else {
            numpg_map.get(&osd_id).copied().unwrap_or(0)
        };
        if numpg == 0 {
            tracing::info!("drain osd.{osd_id}: 0 PGs remaining, proceeding to remove");
            break;
        }
        tracing::info!("drain osd.{osd_id} ({disk_name}): {numpg} PGs still present, waiting...");
    }

    for cmd in [
        vec!["osd", "crush", "remove", &format!("osd.{osd_id}")],
        vec!["auth", "del", &format!("osd.{osd_id}")],
        vec!["osd", "rm", &id_str],
    ] {
        if let Err(e) = kubectl::ceph_exec(&cmd).await {
            tracing::warn!("drain osd.{osd_id}: ceph {:?} failed: {e}", cmd);
        }
    }

    let _ = kubectl::run(&["delete", "deploy", "-n", CEPH_NS,
        &format!("rook-ceph-osd-{osd_id}"), "--ignore-not-found"]).await;

    if host != cfg.node_ipv6 {
        let _ = node_client()
            .post(format!("http://[{}]:{}/api/disks/deactivate-local", host, cfg.port))
            .json(&serde_json::json!({"host": host, "disk_name": disk_name}))
            .send().await;
    } else {
        let name = disk_name.clone();
        tokio::task::spawn_blocking(move || { let _ = do_deactivate_local(&name); }).await.ok();
    }

    draining_osds().lock().unwrap().remove(&osd_id);
    tracing::info!("drain osd.{osd_id} ({disk_name}): complete");
}

pub async fn reconcile_storage(cfg: Arc<Config>) {
    let node_results = gather_from_nodes(&cfg, "/api/disks/local").await;
    let disk_map: HashMap<(String, String), serde_json::Value> = node_results
        .into_iter()
        .flat_map(|(_, disks)| disks.into_iter().filter_map(|d| {
            let host = d["host"].as_str()?.to_string();
            let name = d["name"].as_str()?.to_string();
            Some(((host, name), d))
        }))
        .collect();

    let mut prio = priority::read().await;
    let known: std::collections::HashSet<_> =
        prio.iter().map(|e| (e.host.clone(), e.disk_name.clone())).collect();
    let mut updated = false;
    for ((host, name), disk) in &disk_map {
        if !known.contains(&(host.clone(), name.clone())) {
            if disk["is_builtin"].as_bool().unwrap_or(false) {
                let _ = priority::prepend(host, name).await;
            } else {
                let _ = priority::append(host, name).await;
            }
            updated = true;
        }
    }
    if updated { prio = priority::read().await; }
    if prio.is_empty() { return; }

    let any_osd = disk_map.values().any(|d| d["is_osd"].as_bool().unwrap_or(false));
    if !any_osd {
        for entry in &prio {
            if disk_map.contains_key(&(entry.host.clone(), entry.disk_name.clone())) {
                activate_disk(&cfg, &entry.disk_name, &entry.host).await;
                return;
            }
        }
        return;
    }

    let Ok((status, _, _)) = crate::routers::ceph::cluster_status_from_k8s().await else { return };
    let current_used = status.get("ceph")
        .and_then(|c| c.get("capacity"))
        .and_then(|cap| cap.get("bytesUsed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if current_used == 0 { return; }

    let demanded = (current_used as f64 * 1.2) as u64;
    let mut running = 0u64;
    let mut needed = std::collections::HashSet::new();
    for entry in &prio {
        let key = (entry.host.clone(), entry.disk_name.clone());
        if let Some(disk) = disk_map.get(&key) {
            if running < demanded {
                needed.insert(key);
                running += disk["size_bytes"].as_u64().unwrap_or(0);
            }
        }
    }

    let osd_map = ceph_osd_map().await;
    let needed_all_ready = needed.iter().all(|k| {
        disk_map.get(k).and_then(|d| d["is_osd"].as_bool()).unwrap_or(false)
    });

    for entry in &prio {
        let key = (entry.host.clone(), entry.disk_name.clone());
        let Some(disk) = disk_map.get(&key) else { continue };
        let is_osd = disk["is_osd"].as_bool().unwrap_or(false);
        if needed.contains(&key) && !is_osd {
            activate_disk(&cfg, &entry.disk_name, &entry.host).await;
        } else if !needed.contains(&key) && is_osd && needed_all_ready {
            if let Some(&osd_id) = osd_map.get(&entry.disk_name) {
                // Guard against spawning a second drain task if one is already running.
                let mut draining = draining_osds().lock().unwrap();
                if draining.contains(&osd_id) { continue; }
                draining.insert(osd_id);
                drop(draining);
                let (cfg2, dn, host) = (Arc::clone(&cfg), entry.disk_name.clone(), entry.host.clone());
                tokio::spawn(async move { drain_osd(cfg2, dn, osd_id, host).await });
            }
        }
    }
}

pub async fn disks_local(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let cfg = &state.config;
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let (osd_map, usage) = tokio::join!(ceph_osd_map(), kubectl::osd_df());
    let devices = tokio::task::spawn_blocking(lsblk).await
        .unwrap_or_else(|_| Ok(vec![]))
        .unwrap_or_default();
    let mut result = vec![];
    for d in &devices {
        let name = d["name"].as_str().unwrap_or("").to_string();
        let dtype = d["type"].as_str().unwrap_or("");
        let is_loop = dtype == "loop";
        let is_disk = dtype == "disk";
        if !is_disk && !is_loop { continue; }
        if is_disk && is_system_disk(d) { continue; }
        let model = if is_loop { "Built-in storage".into() } else { d["model"].as_str().unwrap_or("").trim().to_string() };
        let osd_id = osd_map.get(&name).copied();
        let u = osd_id.and_then(|id| usage.get(&id));
        result.push(DiskItem {
            name,
            model,
            size_bytes: d["size"].as_u64().unwrap_or(0),
            host: cfg.node_ipv6.clone(),
            hostname: hostname.clone(),
            is_osd: osd_id.is_some(),
            is_builtin: is_loop,
            used_bytes: u.map(|u| u.used_bytes),
            free_bytes: u.map(|u| u.free_bytes),
        });
    }
    Ok(Json(result))
}

pub async fn disks(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let cfg = &state.config;
    let (node_results, mut prio) = tokio::join!(
        gather_from_nodes(cfg, "/api/disks/local"),
        priority::read(),
    );
    let disk_map: HashMap<(String, String), DiskItem> = node_results
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().filter_map(|v| {
            let item: DiskItem = serde_json::from_value(v).ok()?;
            Some(((item.host.clone(), item.name.clone()), item))
        }))
        .collect();

    let known: std::collections::HashSet<_> =
        prio.iter().map(|e| (e.host.clone(), e.disk_name.clone())).collect();
    let mut updated = false;
    for (key, disk) in &disk_map {
        if !known.contains(key) {
            if disk.is_builtin { let _ = priority::prepend(&key.0, &key.1).await; }
            else { let _ = priority::append(&key.0, &key.1).await; }
            updated = true;
        }
    }
    if updated { prio = priority::read().await; }

    Ok(Json(prio.iter()
        .filter_map(|e| disk_map.get(&(e.host.clone(), e.disk_name.clone())).cloned())
        .collect()))
}

pub async fn update_order(
    State(state): State<AppState>,
    Json(body): Json<DiskOrderRequest>,
) -> Result<Json<serde_json::Value>> {
    let entries: Vec<priority::PriorityEntry> = body.entries.into_iter()
        .map(|e| priority::PriorityEntry { host: e.host, disk_name: e.disk_name })
        .collect();
    priority::write(&entries).await?;
    let cfg2 = Arc::clone(&state.config);
    tokio::spawn(async move {
        let handle = tokio::spawn(reconcile_storage(cfg2));
        match tokio::time::timeout(std::time::Duration::from_secs(120), handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("reconcile_storage panicked in update_order: {:?}", e),
            Err(_) => tracing::error!("reconcile_storage timed out in update_order"),
        }
    });
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn activate_local(Json(body): Json<DiskOrderEntry>) -> Result<Json<serde_json::Value>> {
    let name = body.disk_name.clone();
    tokio::task::spawn_blocking(move || do_activate_local(&name))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn deactivate_local(Json(body): Json<DiskOrderEntry>) -> Result<Json<serde_json::Value>> {
    let name = body.disk_name.clone();
    tokio::task::spawn_blocking(move || do_deactivate_local(&name))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"ok": true})))
}
