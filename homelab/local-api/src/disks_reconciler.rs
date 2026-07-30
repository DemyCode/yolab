/// Disk reconciler — runs as a background task inside yolab-local-api.
/// Replaces the former Python osd-node-controller DaemonSet.
///
/// Every 30 seconds on each node:
///   1. Discover block devices via lsblk (type=disk, no partition children)
///   2. Classify them (ours / clean / foreign-wipe)
///   3. Publish metadata to yolab-disk-status ConfigMap
///   4. Read desired states from yolab-disk-config ConfigMap
///   5. Patch CephCluster CR to match (ON = include, OFF = exclude)
///
/// Division of labour with Rook:
///   - We own: desired-state mapping (ON/OFF), CephCluster spec, osd in/out, crush weight
///   - Rook owns: provisioning, deployment lifecycle, purge (removeOSDsIfOutAndSafeToRemove)
///   We never touch OSD deployments or call osd purge — that eliminates the race
///   where Rook immediately recreates a deployment we just deleted.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Read,
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

// The system OSD is a dedicated LVM logical volume created by disko at install
// (see disk-config.nix). It's always present and always ours, so it's injected
// directly rather than discovered/classified like pluggable physical disks.
const SYSTEM_OSD_DEV: &str = "/dev/mapper/pool-ceph";
const SYSTEM_OSD_ID: &str = "system";

fn system_osd_present() -> bool {
    Path::new(SYSTEM_OSD_DEV).exists()
}

/// Size of the system OSD LV: resolve the mapper symlink to dm-N and read
/// /sys/block/dm-N/size (512-byte sectors). 0 if it can't be determined.
fn system_osd_size_bytes() -> u64 {
    std::fs::read_link(SYSTEM_OSD_DEV)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .and_then(|dm| std::fs::read_to_string(format!("/sys/block/{dm}/size")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|sectors| sectors * 512)
        .unwrap_or(0)
}

fn system_osd_meta() -> Value {
    json!({
        "device": SYSTEM_OSD_DEV,
        "model": "System disk",
        "size_bytes": system_osd_size_bytes(),
        "is_loop": true, // the UI renders is_loop as the built-in "System disk"
        "is_our_osd": true,
        "osd_id": null, // populated by fetch_disk_to_osd in publish_local
    })
}

// ── Leader election ───────────────────────────────────────────────────────────
//
// Every node publishes its own disk inventory, but only ONE node may write the
// shared CephCluster CR — otherwise concurrent nodes clobber each other's entry
// in spec.storage.nodes. A coordination.k8s.io Lease elects that single writer.
// Implemented via kubectl CLI so it works without a working kube-rs client.
mod leader {
    const LEASE_NAME: &str = "yolab-disk-reconciler";
    const LEASE_NS: &str = "rook-ceph";
    const LEASE_SECS: i32 = 30;

    pub async fn try_acquire(identity: &str) -> bool {
        let now = chrono::Utc::now();
        // MicroTime requires microsecond precision (.000000Z).
        let fmt = chrono::SecondsFormat::Micros;

        let lease_json = crate::kubectl::get_json(&[
            "get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json",
        ]).await;

        match &lease_json {
            Err(_) => {
                // Lease doesn't exist yet. Use kubectl create — only one node wins (atomic).
                let manifest = serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": { "name": LEASE_NAME, "namespace": LEASE_NS },
                    "spec": {
                        "holderIdentity": identity,
                        "leaseDurationSeconds": LEASE_SECS,
                        "acquireTime": now.to_rfc3339_opts(fmt, true),
                        "renewTime": now.to_rfc3339_opts(fmt, true),
                    },
                });
                crate::kubectl::create(&manifest.to_string()).await.is_ok()
            }
            Ok(v) => {
                let spec = &v["spec"];
                let holder = spec["holderIdentity"].as_str().unwrap_or("");
                let dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_SECS as i64);
                let expired = spec["renewTime"]
                    .as_str()
                    .and_then(|t| {
                        let ts = chrono::DateTime::parse_from_rfc3339(t).ok()?;
                        Some((now - ts.with_timezone(&chrono::Utc)).num_seconds() > dur)
                    })
                    .unwrap_or(true);

                if holder != identity && !expired {
                    return false; // someone else holds a live lease
                }

                let acquire_time = if holder == identity {
                    // Renewing: preserve original acquireTime.
                    spec["acquireTime"].as_str()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or(now)
                } else {
                    now // taking over from expired holder
                };

                // Use kubectl replace with the exact resourceVersion from the GET.
                // The API server rejects with 409 if another node has since written
                // the resource — prevents two nodes both seeing an expired lease and
                // both becoming leader (TOCTOU).
                let resource_version = v["metadata"]["resourceVersion"].as_str().unwrap_or("");
                let manifest = serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": {
                        "name": LEASE_NAME,
                        "namespace": LEASE_NS,
                        "resourceVersion": resource_version,
                    },
                    "spec": {
                        "holderIdentity": identity,
                        "leaseDurationSeconds": LEASE_SECS,
                        "acquireTime": acquire_time.to_rfc3339_opts(fmt, true),
                        "renewTime": now.to_rfc3339_opts(fmt, true),
                    },
                });
                crate::kubectl::replace(&manifest.to_string()).await.is_ok()
            }
        }
    }

    pub async fn is_holder(identity: &str) -> bool {
        let Ok(v) = crate::kubectl::get_json(&[
            "get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json",
        ]).await else { return false };
        let spec = &v["spec"];
        let holder = spec["holderIdentity"].as_str().unwrap_or("");
        let dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_SECS as i64);
        let fresh = spec["renewTime"]
            .as_str()
            .and_then(|t| {
                let ts = chrono::DateTime::parse_from_rfc3339(t).ok()?;
                Some((chrono::Utc::now() - ts.with_timezone(&chrono::Utc)).num_seconds() <= dur)
            })
            .unwrap_or(false);
        holder == identity && fresh
    }
}

/// True if this node currently holds the disk-reconciler lease. Other
/// leader-only controllers (e.g. the topology controller) gate on this so the
/// whole cluster has a single writer.
pub async fn is_reconcile_leader() -> bool {
    match node_name() {
        Ok(node) => leader::is_holder(&node).await,
        Err(_) => false,
    }
}

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
        // Every node: publish its own disk inventory + run its own OSD lifecycle.
        if let Err(e) = publish_local(&node).await {
            tracing::warn!("disk publish: {e}");
        }
        // Exactly one node assembles the CephCluster CR from all published
        // inventories — a single writer, so no lost-update race.
        if leader::try_acquire(&node).await {
            if let Err(e) = reconcile_cluster().await {
                tracing::warn!("cluster reconcile: {e}");
            }
        }
        sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Build a disk_id → osd_id map using `ceph osd metadata` as the authoritative
/// source. Ceph records `bluestore_bdev_dev_node` (e.g. `/dev/sdb`) for every
/// OSD, which is the exact device it opened — no heuristics, no UUID parsing.
async fn fetch_disk_to_osd(node: &str, meta: &HashMap<String, Value>) -> HashMap<String, i64> {
    // Build full device path → disk_id from our local inventory.
    // Index both the stored path and its canonical (symlink-resolved) path so
    // that /dev/mapper/pool-ceph (a symlink → /dev/dm-1) matches whichever
    // path Ceph actually opened and reports in bluestore_bdev_dev_node.
    let mut device_to_disk_id: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        let Some(dev) = m["device"].as_str() else { continue };
        let full = if dev.starts_with('/') { dev.to_string() } else { format!("/dev/{dev}") };
        if let Ok(canonical) = std::fs::canonicalize(&full) {
            device_to_disk_id.insert(canonical.to_string_lossy().to_string(), disk_id.clone());
        }
        device_to_disk_id.insert(full, disk_id.clone());
    }

    let raw = match kubectl::ceph_exec(&["osd", "metadata", "-f", "json"]).await {
        Ok(r) => r,
        Err(e) => { tracing::warn!("fetch_disk_to_osd: {e}"); return HashMap::new(); }
    };
    let osds: Vec<Value> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => { tracing::warn!("fetch_disk_to_osd: parse: {e}"); return HashMap::new(); }
    };

    let mut result = HashMap::new();
    for osd in &osds {
        if osd["hostname"].as_str().unwrap_or("") != node { continue; }
        let Some(osd_id) = osd["id"].as_i64() else { continue };
        let dev_node = osd["bluestore_bdev_dev_node"].as_str().unwrap_or("");
        if dev_node.is_empty() { continue; }
        if let Some(disk_id) = device_to_disk_id.get(dev_node) {
            result.insert(disk_id.clone(), osd_id);
        }
    }
    result
}

/// Per-node: publish this node's disk inventory + its effective device list to
/// the status ConfigMap, and run this node's own OSD lifecycle. No shared-CR
/// writes happen here, so every node can run it concurrently without racing.
async fn publish_local(node: &str) -> Result<()> {
    let devices = get_devices();
    let our_fsid = cluster_fsid().await.unwrap_or_default();

    let mut meta: HashMap<String, Value> = devices
        .iter()
        .map(|d| (disk_id(d), disk_meta(d, &our_fsid)))
        .collect();
    if system_osd_present() {
        meta.insert(SYSTEM_OSD_ID.to_string(), system_osd_meta());
    }

    // Use Ceph's own metadata as the authoritative disk→OSD mapping — no
    // bluestore header parsing, no size heuristics, no deployment env scraping.
    let disk_to_osd = if !our_fsid.is_empty() {
        fetch_disk_to_osd(node, &meta).await
    } else {
        HashMap::new()
    };
    // Enrich meta with resolved osd_ids so the UI can display them.
    for (d_id, &osd_id) in &disk_to_osd {
        if let Some(m) = meta.get_mut(d_id) {
            m["osd_id"] = json!(osd_id);
        }
    }

    let desired = read_desired().await;

    let want_on = |key: &str| desired.get(key).map(|v| v == "ON" || v == "USING").unwrap_or(false);

    let mut effective: Vec<String> = Vec::new();
    if system_osd_present() {
        let key = format!("{node}--{SYSTEM_OSD_ID}");
        if want_on(&key) {
            effective.push(SYSTEM_OSD_DEV.to_string());
        }
    }
    for d in classify(&devices, &our_fsid) {
        let key = format!("{}--{}", node, disk_id(&d));
        if want_on(&key) {
            effective.push(d);
        }
    }

    write_status(node, &meta, &effective).await;
    auto_register_all_disks(node, &meta, &desired).await;

    reconcile_local_osds(node, &meta, &desired, &disk_to_osd).await;
    Ok(())
}

/// Reconcile each local OSD's active state toward its desired ON/OFF.
///
///   ON  → crush_weight > 0 (set from disk size if 0) + osd in
///   OFF → osd out (starts draining; Rook's removeOSDsIfOutAndSafeToRemove
///         handles purge + deployment deletion once data has migrated)
///
/// We never touch OSD deployments or call osd purge — that's Rook's job.
async fn reconcile_local_osds(
    node: &str,
    meta: &HashMap<String, Value>,
    desired: &HashMap<String, String>,
    disk_to_osd: &HashMap<String, i64>,
) {
    let crush_nodes: Vec<Value> = match kubectl::ceph_exec(&["osd", "df", "tree", "-f", "json"]).await {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v["nodes"].as_array().cloned())
            .unwrap_or_default(),
        Err(_) => return,
    };

    let mut osd_state: HashMap<i64, (f64, f64, u64)> = HashMap::new();
    for n in &crush_nodes {
        if n["type"].as_str() != Some("osd") { continue; }
        if let Some(id) = n["id"].as_i64() {
            osd_state.insert(id, (
                n["crush_weight"].as_f64().unwrap_or(0.0),
                n["reweight"].as_f64().unwrap_or(1.0),
                n["kb"].as_u64().unwrap_or(0),
            ));
        }
    }

    for (disk_id, m) in meta {
        let key = format!("{node}--{disk_id}");
        let want_on = desired.get(&key).map(|v| v == "ON" || v == "USING").unwrap_or(false);
        let Some(&osd_id) = disk_to_osd.get(disk_id) else { continue };
        let (crush_weight, reweight, kb) = osd_state.get(&osd_id).copied().unwrap_or((0.0, 1.0, 0));
        let osd = format!("osd.{osd_id}");

        if want_on {
            if crush_weight == 0.0 {
                let weight = weight_tib_from(kb, m["size_bytes"].as_u64().unwrap_or(0));
                if weight > 0.0 {
                    tracing::info!("{osd} ({disk_id}): crush_weight=0 — setting weight={weight:.5}");
                    let _ = kubectl::ceph_exec(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")]).await;
                }
            }
            if reweight < 0.5 {
                tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=ON — marking in");
                let _ = kubectl::ceph_exec(&["osd", "in", &osd]).await;
            }
        } else if reweight > 0.5 {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=OFF — marking out");
            let _ = kubectl::ceph_exec(&["osd", "out", &osd]).await;
        }
        // reweight ≤ 0.5 + desired=OFF: already draining, Rook will purge when safe
    }
}

/// Convert raw capacity to TiB for a CRUSH weight.
/// Prefers Ceph's own `kb` (from `osd df tree`) over lsblk size_bytes when available.
fn weight_tib_from(kb: u64, size_bytes: u64) -> f64 {
    if kb > 0 {
        kb as f64 / (1u64 << 30) as f64  // KB → TiB: divide by 2^30 (1 TiB = 2^30 KB)
    } else if size_bytes > 0 {
        size_bytes as f64 / (1u64 << 40) as f64
    } else {
        0.0
    }
}

/// Register every newly detected disk in the config CM on first sight.
/// System disk (is_loop) defaults ON — it's always the primary storage.
/// All other disks default OFF; the user must explicitly enable them.
/// Foreign-Ceph disks are registered too so they show in the UI.
async fn auto_register_all_disks(
    node: &str,
    meta: &HashMap<String, Value>,
    desired: &HashMap<String, String>,
) {
    let mut new_entries: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        let key = format!("{node}--{disk_id}");
        if desired.contains_key(&key) { continue; }
        let default = if m["is_loop"].as_bool().unwrap_or(false) { "ON" } else { "OFF" };
        new_entries.insert(key, default.to_string());
    }
    if new_entries.is_empty() { return; }

    let patch = json!({"data": new_entries}).to_string();
    if let Err(e) = kubectl::run(&[
        "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
    ]).await {
        let _ = kubectl::run(&["create", "configmap", CONFIG_CM, "-n", NS]).await;
        if let Err(e2) = kubectl::run(&[
            "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
        ]).await {
            tracing::warn!("auto_register_all_disks: {e}, then {e2}");
        } else {
            tracing::info!("auto_register_all_disks: registered {} new disk(s) on {node}", new_entries.len());
        }
    } else {
        tracing::info!("auto_register_all_disks: registered {} new disk(s) on {node}", new_entries.len());
    }
}

/// Leader-only: read every node's published effective device list and write the
/// CephCluster CR once, as the single writer of spec.storage.nodes.
async fn reconcile_cluster() -> Result<()> {
    let status = kubectl::get_json(&[
        "get", "configmap", STATUS_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .unwrap_or(Value::Object(Default::default()));

    let mut node_devices: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(map) = status.as_object() {
        for (node, payload) in map {
            let Some(s) = payload.as_str() else { continue };
            let Ok(p) = serde_json::from_str::<Value>(s) else { continue };
            let devs: Vec<String> = p["effective"]
                .as_array()
                .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
                .unwrap_or_default();
            node_devices.push((node.clone(), devs));
        }
    }
    node_devices.sort_by(|a, b| a.0.cmp(&b.0));
    patch_cephcluster_all(&node_devices).await
}

// ── Device discovery ──────────────────────────────────────────────────────────

/// Returns pluggable physical disks without partition tables.
/// Uses `lsblk -J` so device-type classification and partition detection are
/// handled by the kernel rather than manual prefix matching on device names.
/// Disks WITH partition children are OS/boot disks — excluded here so they
/// never appear in the Ceph disk list.
fn get_devices() -> Vec<String> {
    let out = std::process::Command::new("lsblk")
        .args(["-J", "-o", "NAME,TYPE"])
        .output()
        .ok();
    let Some(out) = out else { return vec![] };
    let Ok(json) = serde_json::from_slice::<Value>(&out.stdout) else { return vec![] };
    let mut devices = Vec::new();
    if let Some(devs) = json["blockdevices"].as_array() {
        for dev in devs {
            if dev["type"].as_str() != Some("disk") {
                continue;
            }
            let name = dev["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let has_parts = dev["children"].as_array()
                .map(|c| c.iter().any(|ch| ch["type"].as_str() == Some("part")))
                .unwrap_or(false);
            if !has_parts {
                devices.push(name);
            }
        }
    }
    devices.sort();
    devices
}

// ── BlueStore label parsing ───────────────────────────────────────────────────

fn read_bluestore_header(device: &str) -> Option<[u8; 4096]> {
    // Accept both bare kernel names ("sda") and full paths ("/dev/mapper/pool-ceph"),
    // so the system OSD LV can be read the same way as pluggable disks.
    let path = if device.starts_with('/') {
        device.to_string()
    } else {
        format!("/dev/{device}")
    };
    let mut buf = [0u8; 4096];
    let mut f = std::fs::File::open(path).ok()?;
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

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&l, p)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

// ── Classify devices ──────────────────────────────────────────────────────────

fn classify(devices: &[String], our_fsid: &str) -> Vec<String> {
    // devices from get_devices() are already partition-free physical disks.
    let mut effective = Vec::new();
    for device in devices {
        match bluestore_fsid(device).as_deref() {
            None => effective.push(device.clone()),
            Some(fsid) if fsid == our_fsid => {
                tracing::debug!("{device}: our OSD, Rook will re-integrate");
                effective.push(device.clone());
            }
            Some(other) => {
                // Foreign BlueStore label — data from another Ceph cluster.
                // NEVER wipe automatically. Exclude from Ceph and surface to
                // the UI as "contains data from another system"; erasing only
                // happens on explicit user confirmation.
                tracing::warn!("{device}: foreign BlueStore ({other}) — excluding, NOT wiping");
            }
        }
    }
    effective.sort();
    effective
}

// ── Stable disk identity ──────────────────────────────────────────────────────

fn disk_id(device: &str) -> String {
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
    let model = std::fs::read_to_string(format!("/sys/block/{device}/device/model"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let size_bytes: u64 = std::fs::read_to_string(format!("/sys/block/{device}/size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 512;
    let fsid = bluestore_fsid(device);
    let is_our_osd = !our_fsid.is_empty() && fsid.as_deref() == Some(our_fsid);
    // A disk with a BlueStore header from a *different* cluster — data from another
    // Ceph installation. Never auto-wipe; surface for explicit user confirmation.
    let foreign_ceph = fsid.is_some() && !is_our_osd;
    json!({
        "device": device,
        "model": model,
        "size_bytes": size_bytes,
        "is_our_osd": is_our_osd,
        "foreign_ceph": foreign_ceph,
        "osd_id": null, // populated by fetch_disk_to_osd in publish_local
    })
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

async fn write_status(node: &str, meta: &HashMap<String, Value>, effective: &[String]) {
    // Each node's value carries both the per-disk metadata (for the UI) and the
    // effective device list the leader assembles into the CephCluster CR.
    let payload = json!({ "disks": meta, "effective": effective });
    let json_val = serde_json::to_string(&payload).unwrap_or_default();
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

/// Assemble spec.storage.nodes from every node's published effective device
/// list and write it in one patch. Run only by the lease holder, so there is a
/// single writer and no lost-update race. A node that goes offline keeps its
/// last-published entry (its ConfigMap value persists), so its OSDs are not
/// yanked from the CR just because it's temporarily down.
async fn patch_cephcluster_all(node_devices: &[(String, Vec<String>)]) -> Result<()> {
    let cr = kubectl::get_json(&["get", "cephcluster", CLUSTER, "-n", NS, "-o", "json"]).await?;
    let current: Vec<Value> = cr["spec"]["storage"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let desired: Vec<Value> = node_devices
        .iter()
        .filter(|(_, devs)| !devs.is_empty())
        .map(|(node, devs)| {
            let mut d = devs.clone();
            d.sort();
            json!({
                "name": node,
                "devices": d.iter().map(|x| json!({"name": x})).collect::<Vec<_>>(),
            })
        })
        .collect();

    if nodes_equal(&current, &desired) {
        return Ok(());
    }

    let patch = json!({"spec": {
        "storage": {"nodes": desired},
        "removeOSDsIfOutAndSafeToRemove": true,
    }}).to_string();
    for attempt in 0..5u32 {
        match kubectl::run(&[
            "patch", "cephcluster", CLUSTER, "-n", NS, "--type", "merge", "-p", &patch,
        ])
        .await
        {
            Ok(_) => {
                tracing::info!("CephCluster storage.nodes reconciled: {} node(s)", desired.len());
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

/// Compare storage.nodes entries by (name → sorted device names), ignoring
/// ordering and unrelated fields, so we only patch when it truly changed.
fn nodes_equal(a: &[Value], b: &[Value]) -> bool {
    fn norm(nodes: &[Value]) -> std::collections::BTreeMap<String, Vec<String>> {
        nodes
            .iter()
            .filter_map(|n| {
                let name = n["name"].as_str()?.to_string();
                let mut devs: Vec<String> = n["devices"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|d| d["name"].as_str().map(String::from)).collect())
                    .unwrap_or_default();
                devs.sort();
                Some((name, devs))
            })
            .collect()
    }
    norm(a) == norm(b)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn node_name() -> Result<String> {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .context("read /etc/hostname")
}
