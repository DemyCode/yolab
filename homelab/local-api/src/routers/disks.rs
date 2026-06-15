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

#[derive(Serialize, Deserialize)]
pub struct AddVirtualRequest {
    pub size_gb: u64,
    pub host: Option<String>,
}

// Virtual OSDs occupy loop1..loop7, system OSD is loop0.
const MAX_VIRTUAL_LOOP: u32 = 7;

// ── Loop device helpers ──────────────────────────────────────────────────────

/// Returns a map of loop-device short name → backing-file path for every
/// currently-attached loop device, e.g. `{"loop0" → "/var/lib/rook/system-osd.img"}`.
fn loop_backing_files() -> std::collections::HashMap<String, String> {
    let Ok(out) = std::process::Command::new("losetup")
        .args(["-l", "--output", "NAME,BACK-FILE", "--noheadings"])
        .output()
    else { return Default::default() };
    let mut map = std::collections::HashMap::new();
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

fn scan_block_devices() -> anyhow::Result<(Vec<serde_json::Value>, std::collections::HashMap<String, String>)> {
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

/// Strip a trailing partition number from a device name so that e.g. "sdb1"
/// maps back to "sdb". This lets callers match the whole-disk name shown in
/// the UI even when Ceph is running on a partition.
fn disk_base_name(dev: &str) -> &str {
    dev.trim_end_matches(|c: char| c.is_ascii_digit())
}

// ── Ceph OSD mapping ─────────────────────────────────────────────────────────

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
                            // Only map if the symlink target actually exists as a device.
                            if resolved.exists() {
                                let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                                if !name.is_empty() {
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
    }
    if !mapping.is_empty() { return mapping; }
    // Fallback: query Ceph metadata directly via a running OSD pod.
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

// ── Ghost OSD detection & recovery ──────────────────────────────────────────

/// True if the OSD activation directory has a block symlink pointing to a
/// device that actually exists on the host right now.
fn activation_has_valid_device(activate_path: &str) -> bool {
    let block = std::path::Path::new(activate_path).join("block");
    block.is_symlink() && block.exists()
}

/// Returns (osd_id, activate_path) for any OSD deployment that has zero ready
/// replicas — covering both "just created" and "CrashLoopBackOff" states.
fn pending_osd_info(deploy: &serde_json::Value) -> Option<(u32, String)> {
    let osd_id: u32 = deploy["metadata"]["labels"]["ceph-osd-id"].as_str()?.parse().ok()?;
    let ready = deploy["status"]["readyReplicas"].as_u64().unwrap_or(0);
    if ready > 0 { return None; }
    let activate_path = deploy["spec"]["template"]["spec"]["volumes"]
        .as_array()?
        .iter()
        .find(|v| v["name"] == "activate-osd")?
        ["hostPath"]["path"].as_str()?.to_string();
    Some((osd_id, activate_path))
}

/// Maximum restart count across all init + main containers of an OSD pod.
async fn osd_pod_max_restarts(osd_id: u32) -> u32 {
    let Ok(pod_json) = kubectl::get_json(&[
        "get", "pod", "-n", CEPH_NS,
        "-l", &format!("app=rook-ceph-osd,ceph-osd-id={osd_id}"),
        "-o", "json",
    ]).await else { return 0 };

    pod_json["items"].as_array().unwrap_or(&vec![])
        .iter()
        .flat_map(|p| {
            let init = p["status"]["initContainerStatuses"].as_array().cloned().unwrap_or_default();
            let main = p["status"]["containerStatuses"].as_array().cloned().unwrap_or_default();
            init.into_iter().chain(main)
        })
        .filter_map(|c| c["restartCount"].as_u64())
        .max()
        .unwrap_or(0) as u32
}

/// Returns true if Rook currently has an active prepare job for any hostname.
/// An active job means the prepare container is running or pending — in that
/// window the activation directory may legitimately have no block symlink yet.
async fn any_prepare_job_active() -> bool {
    let jobs = kubectl::run(&[
        "get", "jobs", "-n", CEPH_NS,
        "-l", "app=rook-ceph-osd-prepare",
        "-o", "jsonpath={range .items[*]}{.status.active}{\"\\n\"}{end}",
    ]).await.unwrap_or_default();
    jobs.lines().any(|l| l.trim().parse::<u32>().unwrap_or(0) > 0)
}

/// A "ghost OSD" is one where:
///  - The Rook deployment exists but has 0 ready replicas
///  - The activation directory has no block symlink to a real device
///  - No Rook prepare job is currently running (so the missing symlink isn't
///    just "prepare hasn't finished yet")
///
/// Ghost OSDs arise when a device is wiped while Rook is still running the
/// prepare job, leaving a Ceph OSD entry (auth key, CRUSH bucket) with no
/// backing storage.  They loop forever in Init:CrashLoopBackOff and must be
/// purged so the cluster stays healthy.
async fn cleanup_ghost_osds() {
    // If any prepare job is actively running, an activation dir may legitimately
    // be missing its block symlink — bail out and retry next cycle.
    if any_prepare_job_active().await { return; }

    let Ok(deploys) = kubectl::get_json(&[
        "get", "deploy", "-n", CEPH_NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ]).await else { return };

    for deploy in deploys["items"].as_array().unwrap_or(&vec![]) {
        let Some((osd_id, activate_path)) = pending_osd_info(deploy) else { continue };

        // If the block device is present, the OSD might still be starting — leave it.
        if activation_has_valid_device(&activate_path) { continue; }

        tracing::warn!(
            "cleanup_ghost_osds: osd.{osd_id} has no block symlink and no active \
             prepare job — purging from Ceph"
        );

        let id_str = osd_id.to_string();
        // Drain weight to 0 (no-op if already 0), then remove from cluster.
        for cmd in [
            vec!["osd", "reweight", &id_str, "0"],
            vec!["osd", "out", &id_str],
            vec!["osd", "crush", "remove", &format!("osd.{osd_id}")],
            vec!["auth", "del", &format!("osd.{osd_id}")],
            vec!["osd", "rm", &id_str],
        ] {
            if let Err(e) = kubectl::ceph_exec(&cmd).await {
                tracing::warn!("cleanup_ghost_osds: osd.{osd_id}: ceph {:?} failed: {e}", cmd);
            }
        }

        let _ = kubectl::run(&[
            "delete", "deploy", "-n", CEPH_NS,
            &format!("rook-ceph-osd-{osd_id}"), "--ignore-not-found",
        ]).await;

        // Remove the stale (empty) activation directory so a future prepare
        // for the same device starts from a clean slate.
        let _ = std::fs::remove_dir_all(&activate_path);

        tracing::info!("cleanup_ghost_osds: osd.{osd_id} purged successfully");
    }
}

/// Re-attach loop devices for any OSD whose pod is crashing because its loop
/// device was detached at runtime (the symlink exists but the target is gone).
///
/// This can happen if `losetup -d` is run by hand, if the kernel recycled the
/// loop device number, or after certain suspend/resume cycles.  The image file
/// is still on disk, so we can re-attach and let K8s restart the pod.
async fn recover_detached_loop_osds() {
    let Ok(deploys) = kubectl::get_json(&[
        "get", "deploy", "-n", CEPH_NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ]).await else { return };

    for deploy in deploys["items"].as_array().unwrap_or(&vec![]) {
        let Some((osd_id, activate_path)) = pending_osd_info(deploy) else { continue };

        let block = std::path::PathBuf::from(&activate_path).join("block");
        // We only care about the case where the symlink exists but target is missing.
        if !block.is_symlink() { continue; }
        if block.exists() { continue; }

        let Ok(loop_dev) = std::fs::read_link(&block) else { continue };
        let loop_dev_str = loop_dev.to_string_lossy().to_string();
        if !loop_dev_str.starts_with("/dev/loop") { continue; }

        let loop_num: u32 = loop_dev_str
            .trim_start_matches("/dev/loop")
            .parse()
            .unwrap_or(u32::MAX);

        // Determine the image path from the loop slot number.
        let img_path = if loop_num == 0 {
            "/var/lib/rook/system-osd.img".to_string()
        } else {
            format!("/var/lib/rook/virtual-osd-{loop_num}.img")
        };

        if !std::path::Path::new(&img_path).exists() {
            tracing::warn!(
                "recover_detached_loop_osds: osd.{osd_id} loop {loop_dev_str} is gone \
                 and image {img_path} does not exist — cannot recover"
            );
            continue;
        }

        tracing::warn!(
            "recover_detached_loop_osds: osd.{osd_id} device {loop_dev_str} is detached \
             — re-attaching {img_path}"
        );

        // Detach whatever currently occupies the slot, then re-attach.
        let _ = std::process::Command::new("losetup")
            .args(["-d", &loop_dev_str])
            .output();

        let ok = std::process::Command::new("losetup")
            .args(["--direct-io=on", &loop_dev_str, &img_path])
            .status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            let out = std::process::Command::new("losetup")
                .args([&loop_dev_str, &img_path])
                .output();
            if !out.map(|o| o.status.success()).unwrap_or(false) {
                tracing::error!(
                    "recover_detached_loop_osds: osd.{osd_id}: losetup {loop_dev_str} failed"
                );
                continue;
            }
        }

        // Delete the crashing pod so K8s restarts it with the device now present.
        let _ = kubectl::run(&[
            "delete", "pod", "-n", CEPH_NS,
            "-l", &format!("app=rook-ceph-osd,ceph-osd-id={osd_id}"),
            "--ignore-not-found",
        ]).await;

        tracing::info!(
            "recover_detached_loop_osds: osd.{osd_id} re-attached {img_path} → {loop_dev_str}, pod deleted for restart"
        );
    }
}

/// True when Rook is actively working on an OSD for this host and no further
/// activation should be triggered:
///
///  - A prepare job is currently running (active pods > 0).
///  - An OSD deployment exists with 0 ready replicas AND a valid block symlink
///    — meaning the prepare completed and the OSD pod is starting up.
///    (Pods in this state have low restart counts; CrashLoopBackOff ghosts
///    have high restart counts and are handled by cleanup_ghost_osds.)
async fn has_osd_work_in_progress(hostname: &str) -> bool {
    // 1. Is a prepare job currently running?
    let job_name = format!("rook-ceph-osd-prepare-{hostname}");
    let active = kubectl::run(&[
        "get", "job", "-n", CEPH_NS, &job_name,
        "-o", "jsonpath={.status.active}",
    ]).await
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0) > 0;
    if active {
        tracing::debug!("has_osd_work_in_progress: prepare job {job_name} is active");
        return true;
    }

    // 2. Is any OSD deployment starting up (block symlink present, low restarts)?
    let Ok(deploys) = kubectl::get_json(&[
        "get", "deploy", "-n", CEPH_NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ]).await else { return false };

    for deploy in deploys["items"].as_array().unwrap_or(&vec![]) {
        let Some((osd_id, activate_path)) = pending_osd_info(deploy) else { continue };
        let block = std::path::Path::new(&activate_path).join("block");
        if block.is_symlink() {
            // Block symlink present → Rook finished the prepare, OSD pod is starting.
            let restarts = osd_pod_max_restarts(osd_id).await;
            if restarts < 5 {
                tracing::debug!(
                    "has_osd_work_in_progress: osd.{osd_id} has block symlink and only \
                     {restarts} restarts — still starting up"
                );
                return true;
            }
        }
    }

    false
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

// ── Disk activation / deactivation ───────────────────────────────────────────

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

/// Find the activation directory (if any) for a specific device on this host.
/// The directory is named `{ceph-fsid}_{osd-uuid}` under /var/lib/rook/rook-ceph/
/// and contains a `block` symlink pointing to the device.
fn find_activation_dir_for_device(device_name: &str) -> Option<std::path::PathBuf> {
    let rook_dir = std::path::Path::new("/var/lib/rook/rook-ceph");
    let target_dev = format!("/dev/{device_name}");
    let Ok(entries) = std::fs::read_dir(rook_dir) else { return None };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let block = path.join("block");
        if !block.is_symlink() { continue; }
        let Ok(link) = std::fs::read_link(&block) else { continue };
        if link.to_string_lossy() == target_dev {
            return Some(path);
        }
    }
    None
}

fn do_activate_local(disk_name: &str) -> anyhow::Result<()> {
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let job_name = format!("rook-ceph-osd-prepare-{hostname}");

    let (job_active, job_succeeded, job_failed) = std::process::Command::new("kubectl")
        .args(["get", "job", "-n", CEPH_NS, &job_name,
               "-o", "jsonpath={.status.active}/{.status.succeeded}/{.status.failed}"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            let mut p = s.splitn(3, '/');
            let active    = p.next().unwrap_or("").trim().parse::<u32>().unwrap_or(0);
            let succeeded = p.next().unwrap_or("").trim().parse::<u32>().unwrap_or(0);
            let failed    = p.next().unwrap_or("").trim().parse::<u32>().unwrap_or(0);
            (active > 0, succeeded > 0, failed > 0)
        })
        .unwrap_or((false, false, false));

    if job_active { return Ok(()); }

    if job_succeeded {
        // The job object persists after completion (no ttlSecondsAfterFinished).
        // Check whether it actually prepared THIS device by looking for the
        // activation dir — if found, the OSD pod is starting up and we should wait.
        let ceph_dev_check = ceph_device_for(disk_name);
        if find_activation_dir_for_device(&ceph_dev_check).is_some() {
            return Ok(());
        }
        // Stale succeeded job from a previous device (e.g., loop0 was prepared
        // hours ago and its job is still around). Delete it and re-trigger.
        tracing::info!(
            "activate {disk_name}: stale succeeded prepare job (no activation dir for \
             {ceph_dev_check}) — deleting and re-triggering"
        );
        let _ = std::process::Command::new("kubectl")
            .args(["delete", "job", "-n", CEPH_NS, &job_name, "--ignore-not-found"])
            .output();
        wipe_device(disk_name);
    } else if job_failed {
        tracing::warn!("activate {disk_name}: prepare job failed");
        let _ = std::process::Command::new("kubectl")
            .args(["delete", "job", "-n", CEPH_NS, &job_name, "--ignore-not-found"])
            .output();

        let ceph_dev = ceph_device_for(disk_name);
        // If the prepare job wrote the BlueStore label before failing, the
        // activation dir will have a `block` symlink pointing to our device.
        // Don't wipe — re-trigger the prepare so Rook can activate the
        // already-registered OSD without data loss.
        if find_activation_dir_for_device(&ceph_dev).is_some() {
            tracing::info!(
                "activate {disk_name}: existing activation dir found — re-triggering \
                 prepare without wiping"
            );
            // Fall through to patch + delete prepare job, skipping the wipe.
        } else {
            tracing::info!("activate {disk_name}: no activation dir — wiping device for clean retry");
            wipe_device(disk_name);
        }
    } else {
        // No prepare job at all — first activation or post-drain activation.
        wipe_device(disk_name);
    }

    let ceph_dev = ceph_device_for(disk_name);
    let raw = std::process::Command::new("kubectl")
        .args(["get", "cephcluster", "-n", CEPH_NS, CEPH_CLUSTER,
               "-o", "jsonpath={.spec.storage.devices}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut devices: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
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
        .args(["delete", "job", "-n", CEPH_NS, &job_name, "--ignore-not-found"])
        .output();
    Ok(())
}

/// Wipe a device so Rook can use it as a fresh OSD.
/// Loop devices are zeroed + wipefs'd; real disks get a new GPT + partition.
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
        let _ = std::process::Command::new("blockdev")
            .args(["--rereadpt", &dev])
            .output();
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
    // permanent data loss when running single-replica pools.
    //
    // Safety: if the Prometheus exporter is unavailable, osd_numpg() returns
    // an empty map.  We must NOT treat that as "0 PGs" (data-loss wipe).
    // Fall back to asking Ceph directly; if that also fails, keep waiting.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let numpg_map = kubectl::osd_numpg().await;
        let numpg = if numpg_map.is_empty() {
            match kubectl::ceph_osd_numpg_direct(osd_id).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        "drain osd.{osd_id}: exporter and direct query both unavailable ({e}), \
                         keeping OSD safe"
                    );
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

// ── Reconcile ────────────────────────────────────────────────────────────────

pub async fn reconcile_storage(cfg: Arc<Config>) {
    // ── Phase 0a: remove ghost OSDs (no device, stuck in CrashLoopBackOff) ──
    cleanup_ghost_osds().await;

    // ── Phase 0b: re-attach detached loop devices for live OSDs ─────────────
    recover_detached_loop_osds().await;

    // ── Phase 1: gather disk state from all nodes ────────────────────────────
    let node_results = gather_from_nodes(&cfg, "/api/disks/local").await;
    let disk_map: HashMap<(String, String), serde_json::Value> = node_results
        .into_iter()
        .flat_map(|(_, disks)| disks.into_iter().filter_map(|d| {
            let host = d["host"].as_str()?.to_string();
            let name = d["name"].as_str()?.to_string();
            Some(((host, name), d))
        }))
        .collect();

    if disk_map.is_empty() {
        tracing::debug!("reconcile: no disks visible from any node");
        return;
    }
    tracing::debug!("reconcile: {} disk(s) visible across nodes", disk_map.len());

    // ── Phase 2: ensure all visible disks are in the priority list ───────────
    let mut prio = priority::read().await;
    let known: std::collections::HashSet<_> =
        prio.iter().map(|e| (e.host.clone(), e.disk_name.clone())).collect();
    let mut updated = false;
    for ((host, name), disk) in &disk_map {
        if !known.contains(&(host.clone(), name.clone())) {
            let builtin = disk["is_builtin"].as_bool().unwrap_or(false);
            tracing::info!(
                "reconcile: new disk {name} on {host} (builtin={builtin}) → adding to priority list"
            );
            if builtin {
                let _ = priority::prepend(host, name).await;
            } else {
                let _ = priority::append(host, name).await;
            }
            updated = true;
        }
    }
    if updated { prio = priority::read().await; }

    if prio.is_empty() {
        tracing::warn!("reconcile: priority list is empty after update — ConfigMap write may have failed");
        return;
    }

    // ── Phase 3: skip if Rook is already working on an OSD ──────────────────
    // Check every node's hostname (fan-out nodes share the same ConfigMap so
    // we check the local hostname; for multi-node setups each node runs its
    // own reconcile for its own disks).
    let hostname = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    if has_osd_work_in_progress(&hostname).await {
        tracing::debug!("reconcile: Rook is preparing or starting an OSD — skipping activation");
        return;
    }

    // ── Phase 4: capacity-based activation / drain ───────────────────────────

    let any_osd = disk_map.values().any(|d| d["is_osd"].as_bool().unwrap_or(false));
    if !any_osd {
        // No OSDs running anywhere — activate the first disk in priority order.
        for entry in &prio {
            let key = (entry.host.clone(), entry.disk_name.clone());
            if disk_map.contains_key(&key) {
                tracing::info!(
                    "reconcile: no OSDs yet — activating {} on {}",
                    entry.disk_name, entry.host
                );
                activate_disk(&cfg, &entry.disk_name, &entry.host).await;
                return;
            }
        }
        tracing::debug!("reconcile: no OSDs and no priority disk is visible — waiting");
        return;
    }

    let Ok((status, _, _)) = crate::routers::ceph::cluster_status_from_k8s().await else {
        tracing::debug!("reconcile: Ceph status unavailable — skipping capacity check");
        return;
    };
    let current_used = status.get("ceph")
        .and_then(|c| c.get("capacity"))
        .and_then(|cap| cap.get("bytesUsed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if current_used == 0 {
        tracing::debug!("reconcile: Ceph reports 0 bytes used — skipping expansion check");
        return;
    }

    // Build the minimum set of disks (in priority order) required to hold
    // 120 % of current usage, giving a comfortable headroom buffer.
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
    tracing::debug!(
        "reconcile: need {} disk(s) to cover {demanded} bytes (used={current_used})",
        needed.len()
    );

    let osd_map = ceph_osd_map().await;

    // All needed disks must already be OSDs before we start draining extras.
    // This prevents draining a disk while the replacement is still being set up.
    let needed_all_ready = needed.iter().all(|k| {
        disk_map.get(k).and_then(|d| d["is_osd"].as_bool()).unwrap_or(false)
    });

    for entry in &prio {
        let key = (entry.host.clone(), entry.disk_name.clone());
        let Some(disk) = disk_map.get(&key) else { continue };
        let is_osd = disk["is_osd"].as_bool().unwrap_or(false);

        if needed.contains(&key) && !is_osd {
            tracing::info!(
                "reconcile: activating {} on {} (needed for capacity)",
                entry.disk_name, entry.host
            );
            activate_disk(&cfg, &entry.disk_name, &entry.host).await;
            // Activate one disk at a time — return and let the next reconcile
            // cycle decide whether more are needed.
            return;
        } else if !needed.contains(&key) && is_osd && needed_all_ready {
            if let Some(&osd_id) = osd_map.get(&entry.disk_name) {
                let mut draining = draining_osds().lock().unwrap();
                if draining.contains(&osd_id) { continue; }
                draining.insert(osd_id);
                drop(draining);
                tracing::info!(
                    "reconcile: draining osd.{osd_id} ({}) — no longer needed",
                    entry.disk_name
                );
                let (cfg2, dn, host) = (Arc::clone(&cfg), entry.disk_name.clone(), entry.host.clone());
                tokio::spawn(async move { drain_osd(cfg2, dn, osd_id, host).await });
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
        let is_builtin = if is_loop {
            backing_files.get(&name).map(String::as_str).unwrap_or("") == "/var/lib/rook/system-osd.img"
        } else {
            false
        };
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

fn do_add_virtual_local(size_gb: u64) -> anyhow::Result<String> {
    use std::path::Path;

    // loop0  → system-osd.img  (reserved by yolab-system-osd service)
    // loop1..MAX_VIRTUAL_LOOP → virtual-osd-{1..N}.img
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

    if !Path::new(&img_path).exists() {
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
        let out = std::process::Command::new("losetup")
            .args([&loop_dev, &img_path])
            .output()?;
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
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({ "ok": true, "device": loop_name })))
}

pub async fn add_virtual(
    State(state): State<AppState>,
    Json(body): Json<AddVirtualRequest>,
) -> Result<Json<serde_json::Value>> {
    let host = body.host.clone().unwrap_or_else(|| state.config.node_ipv6.clone());
    if host != state.config.node_ipv6 {
        let json: serde_json::Value = node_client()
            .post(format!(
                "http://[{}]:{}/api/disks/add-virtual-local",
                host, state.config.port
            ))
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(e))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!(e))?
            .json()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        return Ok(Json(json));
    }
    let size_gb = body.size_gb;
    let loop_name = tokio::task::spawn_blocking(move || do_add_virtual_local(size_gb))
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({ "ok": true, "device": loop_name })))
}
