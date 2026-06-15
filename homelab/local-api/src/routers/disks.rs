use std::{collections::HashMap, sync::{Arc, Mutex, OnceLock}};

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, kubectl, AppState};

const CEPH_NS: &str = "rook-ceph";
const CEPH_CLUSTER: &str = "rook-ceph";
const MAX_VIRTUAL_LOOP: u32 = 7;
// After a prepare job completes, Rook needs time to create the OSD deployment.
// Treat a recently-completed job as still in progress during this cooldown.
const PREPARE_COOLDOWN_SECS: i64 = 300;

// ── Structs ───────────────────────────────────────────────────────────────────

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
pub struct DrainRequest {
    pub disk_name: String,
    pub host: String,
    pub force: Option<bool>,
}

#[derive(Serialize, Deserialize)]
pub struct AddVirtualRequest {
    pub size_gb: u64,
    pub host: Option<String>,
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

// ── Ceph OSD map (used by disks_local for is_osd / usage) ────────────────────

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

/// True if the prepare job for this hostname is currently running (active pods > 0).
fn prepare_job_active_sync(job_name: &str) -> bool {
    std::process::Command::new("kubectl")
        .args(["get", "job", "-n", CEPH_NS, job_name,
               "-o", "jsonpath={.status.active}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().unwrap_or(0) > 0)
        .unwrap_or(false)
}

/// True if the prepare job completed within PREPARE_COOLDOWN_SECS.
/// Prevents wiping a device Rook just prepared before the OSD deploy is created.
fn prepare_job_completed_recently_sync(job_name: &str) -> bool {
    let out = std::process::Command::new("kubectl")
        .args(["get", "job", "-n", CEPH_NS, job_name,
               "-o", "jsonpath={.status.succeeded}/{.status.completionTime}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut parts = out.splitn(2, '/');
    let succeeded: u32 = parts.next().unwrap_or("").trim().parse().unwrap_or(0);
    if succeeded == 0 { return false; }
    let completion_time = parts.next().unwrap_or("").trim();
    // If job succeeded but completionTime is missing, treat as recent (safe).
    if completion_time.is_empty() { return true; }
    if let Ok(t) = time::OffsetDateTime::parse(
        completion_time,
        &time::format_description::well_known::Rfc3339,
    ) {
        let now = time::OffsetDateTime::now_utc();
        return (now - t).whole_seconds() < PREPARE_COOLDOWN_SECS;
    }
    true // unparseable → assume recent (safe)
}

/// Async: true if the prepare job for this hostname has active pods.
async fn prepare_job_active(hostname: &str) -> bool {
    let job_name = format!("rook-ceph-osd-prepare-{hostname}");
    kubectl::run(&["get", "job", "-n", CEPH_NS, &job_name,
                   "-o", "jsonpath={.status.active}"])
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0) > 0
}

/// Ceph OSD "in" status from `ceph osd dump`: osd_id → 1 (in/active) or 0 (out/draining).
/// Returns an empty map if Ceph is unreachable — callers must treat empty as unknown.
struct CephOsdState {
    in_val: u32, // 1 = in, 0 = out
    up_val: u32, // 1 = up, 0 = down
}

async fn ceph_osd_states() -> HashMap<u32, CephOsdState> {
    let Ok(out) = kubectl::ceph_exec(&["osd", "dump", "--format", "json"]).await else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) else {
        return HashMap::new();
    };
    v["osds"].as_array().unwrap_or(&vec![]).iter().filter_map(|o| {
        let id: u32 = o["osd"].as_u64()? as u32;
        let in_val: u32 = o["in"].as_u64().unwrap_or(1) as u32;
        let up_val: u32 = o["up"].as_u64().unwrap_or(1) as u32;
        Some((id, CephOsdState { in_val, up_val }))
    }).collect()
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

// ── Ghost OSD cleanup ─────────────────────────────────────────────────────────

/// A ghost OSD is one whose deploy is not ready AND whose backing device is gone.
/// This happens when a device is physically removed or wiped while Ceph still has
/// an OSD entry for it. Ghost OSDs loop forever in Init:CrashLoopBackOff.
///
/// We skip cleanup when a prepare job is active: a deploy can legitimately be
/// not-ready while Rook is still writing the BlueStore label.
async fn cleanup_ghost_osds() {
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    if prepare_job_active(&hostname).await { return; }

    for deploy in list_osd_deploys().await {
        if deploy.ready { continue; }
        // If the device exists, the OSD is just starting up — not a ghost.
        if std::path::Path::new(&deploy.dev_path).exists() { continue; }

        tracing::warn!(
            "ghost OSD: osd.{} device {} missing — purging",
            deploy.osd_id, deploy.dev_path
        );
        // Delete deploy first so the pod terminates and OSD goes DOWN before
        // we call `osd rm`. Avoids EBUSY if Ceph hasn't marked it down yet.
        let _ = kubectl::run(&[
            "delete", "deploy", "-n", CEPH_NS, &deploy.name, "--ignore-not-found",
        ]).await;
        purge_osd_from_ceph(deploy.osd_id).await;
        tracing::info!("ghost OSD: osd.{} purged", deploy.osd_id);
    }
}

// ── Orphaned OSD cleanup ──────────────────────────────────────────────────────

/// An orphaned OSD is one that is `down+out` in Ceph but has no deploy.
/// This can happen when `ceph osd rm` fails with EBUSY (OSD still up) during
/// drain, and then the deploy is deleted. The OSD goes down once the pod is
/// gone but was never removed from the Ceph OSD map.
async fn purge_orphaned_osds() {
    let deploys = list_osd_deploys().await;
    let deployed_ids: std::collections::HashSet<u32> = deploys.iter().map(|d| d.osd_id).collect();

    let states = ceph_osd_states().await;
    for (osd_id, state) in &states {
        if deployed_ids.contains(osd_id) { continue; } // has a deploy — normal
        if state.in_val != 0 { continue; }             // still in — don't touch active OSDs
        if state.up_val != 0 { continue; }             // still up — wait for pod termination

        // down+out with no deploy → safe to remove from Ceph
        tracing::info!("purge_orphaned: osd.{osd_id} is down+out with no deploy — removing from Ceph");
        purge_osd_from_ceph(*osd_id).await;
    }
}

// ── Loop device recovery ──────────────────────────────────────────────────────

/// Re-attach a loop device that was detached at runtime (kernel recycled the slot,
/// manual losetup -d, etc.). The .img file is still on disk; re-mount and restart
/// the OSD pod so it comes back without data loss.
async fn recover_detached_loop_osds() {
    for deploy in list_osd_deploys().await {
        if deploy.ready { continue; }
        if !deploy.dev_path.starts_with("/dev/loop") { continue; }
        // Device path exists → loop is attached, OSD is just starting.
        if std::path::Path::new(&deploy.dev_path).exists() { continue; }

        let loop_num: u32 = deploy.dev_path
            .trim_start_matches("/dev/loop")
            .parse()
            .unwrap_or(u32::MAX);
        let img_path = if loop_num == 0 {
            "/var/lib/rook/system-osd.img".to_string()
        } else {
            format!("/var/lib/rook/virtual-osd-{loop_num}.img")
        };

        if !std::path::Path::new(&img_path).exists() {
            tracing::warn!(
                "recover_loop: osd.{}: image {img_path} missing, cannot recover",
                deploy.osd_id
            );
            continue;
        }

        tracing::warn!(
            "recover_loop: osd.{}: {} detached — re-attaching {img_path}",
            deploy.osd_id, deploy.dev_path
        );
        let _ = std::process::Command::new("losetup").args(["-d", &deploy.dev_path]).output();
        let ok = std::process::Command::new("losetup")
            .args(["--direct-io=on", &deploy.dev_path, &img_path])
            .status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            let out = std::process::Command::new("losetup")
                .args([&deploy.dev_path, &img_path])
                .output();
            if !out.map(|o| o.status.success()).unwrap_or(false) {
                tracing::error!("recover_loop: osd.{}: losetup failed", deploy.osd_id);
                continue;
            }
        }
        let _ = kubectl::run(&[
            "delete", "pod", "-n", CEPH_NS,
            "-l", &format!("app=rook-ceph-osd,ceph-osd-id={}", deploy.osd_id),
            "--ignore-not-found",
        ]).await;
        tracing::info!("recover_loop: osd.{}: re-attached, pod restarted", deploy.osd_id);
    }
}

// ── Node fan-out ──────────────────────────────────────────────────────────────

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

fn node_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── Activation ────────────────────────────────────────────────────────────────

// Tracks which devices are currently being activated to prevent concurrent
// spawn_blocking tasks (from aborted reconcile tasks) from double-wiping.
static ACTIVATING: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

/// Prepare a device for use as a Ceph OSD on the local node.
///
/// Four guards prevent redundant or destructive activations:
///   1. OSD deploy already references this device → Rook is working on it.
///   2. Prepare job is actively running → Rook is working on it.
///   3. Prepare job completed recently → Rook may not have created the deploy yet.
///   4. Device already in CephCluster → Rook is about to create the prepare job.
///
/// A process-level mutex prevents two concurrent spawn_blocking calls (e.g. from
/// an aborted reconcile whose blocking task outlived the abort) from both wiping
/// the same device simultaneously.
fn do_activate_local(disk_name: &str) -> anyhow::Result<()> {
    // Mutex guard: only one activation per device at a time.
    let activating = ACTIVATING.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    {
        let mut guard = activating.lock().unwrap();
        if guard.contains(disk_name) {
            tracing::debug!("activate {disk_name}: another task is already activating this device");
            return Ok(());
        }
        guard.insert(disk_name.to_string());
    }
    let result = do_activate_local_inner(disk_name);
    activating.lock().unwrap().remove(disk_name);
    result
}

fn do_activate_local_inner(disk_name: &str) -> anyhow::Result<()> {
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let job_name = format!("rook-ceph-osd-prepare-{hostname}");
    let ceph_dev = ceph_device_for(disk_name);
    let dev_path = format!("/dev/{ceph_dev}");

    if osd_deploy_exists_for_device_sync(&dev_path) {
        tracing::debug!("activate {disk_name}: OSD deploy exists, nothing to do");
        return Ok(());
    }
    if prepare_job_active_sync(&job_name) {
        tracing::debug!("activate {disk_name}: prepare job active, waiting");
        return Ok(());
    }
    if prepare_job_completed_recently_sync(&job_name) {
        tracing::debug!("activate {disk_name}: prepare job completed recently, waiting for deploy");
        return Ok(());
    }
    // Device already in CephCluster: Rook is about to (or is) creating the prepare job.
    // This covers the 60-90s window between patching CephCluster and the job appearing,
    // preventing repeated wipes that reset BlueStore initialization mid-flight.
    if cephcluster_has_device_sync(&ceph_dev) {
        tracing::debug!("activate {disk_name}: already in CephCluster, waiting for Rook");
        return Ok(());
    }

    // Clear any stale job, wipe the device, register in CephCluster.
    let _ = std::process::Command::new("kubectl")
        .args(["delete", "job", "-n", CEPH_NS, &job_name, "--ignore-not-found"])
        .output();
    wipe_device(disk_name);
    cephcluster_add_device_sync(&ceph_dev);
    tracing::info!("activate {disk_name}: wiped and registered as {ceph_dev}, waiting for Rook");
    Ok(())
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

// ── Reconcile ────────────────────────────────────────────────────────────────

/// Simple reconcile: fix broken state, then ensure every non-system disk is an OSD.
/// No priority list. No target computation. Ceph handles data distribution.
/// Drain is explicit-only: user triggers POST /api/disks/drain.
pub async fn reconcile_storage(cfg: Arc<Config>) {
    cleanup_ghost_osds().await;
    recover_detached_loop_osds().await;
    purge_orphaned_osds().await;

    // Ensure every non-system disk visible from any node is in CephCluster.
    // Activate at most one disk per tick so Rook isn't overwhelmed.
    let node_results = gather_from_nodes(&cfg, "/api/disks/local").await;
    for (_, disks) in node_results {
        for disk in disks {
            let host = disk["host"].as_str().unwrap_or("").to_string();
            let name = disk["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() || host.is_empty() { continue; }
            let ceph_dev = ceph_device_for(&name);
            if !cephcluster_has_device_sync(&ceph_dev) {
                tracing::info!("reconcile: {name} on {host} not in CephCluster — activating");
                activate_disk(&cfg, &name, &host).await;
                return; // one activation per tick
            }
        }
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

pub async fn disks_local(State(state): State<AppState>) -> Result<Json<Vec<DiskItem>>> {
    let cfg = &state.config;
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let (osd_map, usage) = tokio::join!(ceph_osd_map(), kubectl::osd_df());
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
        let osd_id = osd_map.get(&name).copied();
        let u = osd_id.and_then(|id| usage.get(&id));
        result.push(DiskItem {
            name,
            model,
            size_bytes: d["size"].as_u64().unwrap_or(0),
            host: cfg.node_ipv6.clone(),
            hostname: hostname.clone(),
            is_osd: osd_id.is_some(),
            is_builtin,
            used_bytes: u.map(|u| u.used_bytes),
            free_bytes: u.map(|u| u.free_bytes),
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

/// Drain an OSD: mark it out, wait for safe-to-destroy, then remove from Ceph.
/// No wipe — the disk stays clean and can be re-added by the reconcile loop.
/// Use force=true to skip the safe-to-destroy check and accept data loss.
pub async fn drain_disk(
    State(state): State<AppState>,
    Json(body): Json<DrainRequest>,
) -> Result<Json<serde_json::Value>> {
    let cfg = Arc::clone(&state.config);
    let disk_name = body.disk_name.clone();
    let host = body.host.clone();
    let force = body.force.unwrap_or(false);
    tokio::spawn(async move {
        drain_disk_background(&cfg, &disk_name, &host, force).await;
    });
    Ok(Json(serde_json::json!({"ok": true, "message": "drain started"})))
}

async fn drain_disk_background(cfg: &Config, disk_name: &str, host: &str, force: bool) {
    let ceph_dev = ceph_device_for(disk_name);

    if host != cfg.node_ipv6 {
        // Remote node: fan out to that node's drain-local endpoint (NYI — remove from spec only).
        tracing::warn!("drain: remote drain NYI for {disk_name} on {host}, removing from CephCluster spec");
        cephcluster_remove_device(&ceph_dev).await;
        return;
    }

    let deploys = list_osd_deploys().await;
    let dev_path = format!("/dev/{ceph_dev}");
    let Some(deploy) = deploys.iter().find(|d| d.dev_path == dev_path) else {
        tracing::info!("drain: {disk_name} has no OSD deploy — removing from CephCluster spec only");
        cephcluster_remove_device(&ceph_dev).await;
        return;
    };

    let osd_id = deploy.osd_id;
    let deploy_name = deploy.name.clone();
    let id_str = osd_id.to_string();

    let _ = kubectl::ceph_exec(&["osd", "reweight", &id_str, "0"]).await;
    let _ = kubectl::ceph_exec(&["osd", "out", &id_str]).await;
    tracing::info!("drain: osd.{osd_id} ({disk_name}) marked out");

    if !force {
        // Wait indefinitely — bounded timeouts caused data loss in the past.
        // With size=1 pools, this only resolves if the pool is emptied first.
        // Use force=true to bypass and accept data loss.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            match osd_safe_to_destroy(osd_id).await {
                Some(true) => break,
                Some(false) => tracing::debug!("drain: osd.{osd_id}: PGs still migrating"),
                None => tracing::warn!("drain: osd.{osd_id}: Ceph unreachable, retrying"),
            }
        }
    }

    tracing::info!("drain: osd.{osd_id} ({disk_name}): removing from Ceph");
    let _ = kubectl::run(&["delete", "deploy", "-n", CEPH_NS, &deploy_name, "--ignore-not-found"]).await;
    purge_osd_from_ceph(osd_id).await;
    cephcluster_remove_device(&ceph_dev).await;
    tracing::info!("drain: osd.{osd_id} ({disk_name}): done — disk ready to unplug or re-add");
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
    let size_gb = body.size_gb;
    let loop_name = tokio::task::spawn_blocking(move || do_add_virtual_local(size_gb))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({ "ok": true, "device": loop_name })))
}

pub async fn add_virtual(
    State(state): State<AppState>,
    Json(body): Json<AddVirtualRequest>,
) -> Result<Json<serde_json::Value>> {
    let host = body.host.clone().unwrap_or_else(|| state.config.node_ipv6.clone());
    if host != state.config.node_ipv6 {
        let json: serde_json::Value = node_client()
            .post(format!("http://[{}]:{}/api/disks/add-virtual-local", host, state.config.port))
            .json(&body)
            .send().await.map_err(|e| anyhow::anyhow!(e))?
            .error_for_status().map_err(|e| anyhow::anyhow!(e))?
            .json().await.map_err(|e| anyhow::anyhow!(e))?;
        return Ok(Json(json));
    }
    let size_gb = body.size_gb;
    let loop_name = tokio::task::spawn_blocking(move || do_add_virtual_local(size_gb))
        .await.map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({ "ok": true, "device": loop_name })))
}
