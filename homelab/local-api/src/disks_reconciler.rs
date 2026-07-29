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

// ── Leader election ───────────────────────────────────────────────────────────
//
// Every node publishes its own disk inventory, but only ONE node may write the
// shared CephCluster CR — otherwise concurrent nodes clobber each other's entry
// in spec.storage.nodes (a JSON merge patch replaces the whole array). A
// standard coordination.k8s.io Lease elects that single writer.
mod leader {
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use kube::api::PostParams;

    const LEASE_NAME: &str = "yolab-disk-reconciler";
    const LEASE_NS: &str = "rook-ceph";
    const LEASE_SECS: i32 = 30;

    /// Acquire or renew the reconciler lease. Returns true iff `identity` holds a
    /// currently-valid lease afterwards. Uses replace() (optimistic concurrency
    /// via resourceVersion) so a simultaneous take-over by another node loses
    /// cleanly with a conflict.
    pub async fn try_acquire(identity: &str) -> bool {
        let Ok(client) = crate::kubectl::client().await else { return false };
        let api: kube::Api<Lease> = kube::Api::namespaced(client, LEASE_NS);
        let now = chrono::Utc::now();
        match api.get_opt(LEASE_NAME).await {
            Ok(Some(mut lease)) => {
                let spec = lease.spec.clone().unwrap_or_default();
                let holder = spec.holder_identity.clone().unwrap_or_default();
                let dur = spec.lease_duration_seconds.unwrap_or(LEASE_SECS) as i64;
                let expired = spec
                    .renew_time
                    .as_ref()
                    .map(|t| (now - t.0).num_seconds() > dur)
                    .unwrap_or(true);
                if holder != identity && !expired {
                    return false; // another node holds a valid lease
                }
                lease.spec = Some(LeaseSpec {
                    holder_identity: Some(identity.to_string()),
                    lease_duration_seconds: Some(LEASE_SECS),
                    renew_time: Some(MicroTime(now)),
                    acquire_time: if holder == identity {
                        spec.acquire_time
                    } else {
                        Some(MicroTime(now))
                    },
                    ..Default::default()
                });
                api.replace(LEASE_NAME, &PostParams::default(), &lease)
                    .await
                    .is_ok()
            }
            Ok(None) => {
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(LEASE_NAME.into()),
                        namespace: Some(LEASE_NS.into()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(identity.to_string()),
                        lease_duration_seconds: Some(LEASE_SECS),
                        renew_time: Some(MicroTime(now)),
                        acquire_time: Some(MicroTime(now)),
                        ..Default::default()
                    }),
                };
                api.create(&PostParams::default(), &lease).await.is_ok()
            }
            Err(_) => false,
        }
    }

    /// Read-only: does `identity` currently hold a valid (unexpired) lease?
    /// Does not renew — used by other leader-only controllers to gate work.
    pub async fn is_holder(identity: &str) -> bool {
        let Ok(client) = crate::kubectl::client().await else { return false };
        let api: kube::Api<Lease> = kube::Api::namespaced(client, LEASE_NS);
        match api.get_opt(LEASE_NAME).await {
            Ok(Some(lease)) => {
                let spec = lease.spec.unwrap_or_default();
                let holder = spec.holder_identity.unwrap_or_default();
                let dur = spec.lease_duration_seconds.unwrap_or(LEASE_SECS) as i64;
                let fresh = spec
                    .renew_time
                    .as_ref()
                    .map(|t| (chrono::Utc::now() - t.0).num_seconds() <= dur)
                    .unwrap_or(false);
                holder == identity && fresh
            }
            _ => false,
        }
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

/// Returns the OSD ID that resides on `device` on this node, if any.
/// Reads the BlueStore OSD UUID from the device header and matches it against
/// running OSD deployments. Returns None if the device is not a Ceph OSD or
/// if the OSD can't be identified.
pub fn osd_id_for_device(device: &str, our_fsid: &str) -> Option<i64> {
    if device.starts_with("loop") {
        return None;
    }
    if bluestore_fsid(device).as_deref() != Some(our_fsid) {
        return None;
    }
    bluestore_osd_uuid(device).and_then(|uuid| osd_id_from_uuid_sync(&uuid))
}

fn osd_id_from_uuid_sync(osd_uuid: &str) -> Option<i64> {
    // OSD deployments have label rook-ceph-osd-id=<id>
    // We look for deployments whose ROOK_OSD_UUID env var matches.
    let out = std::process::Command::new("kubectl")
        .args([
            "get", "deploy", "-n", NS, "-l", "app=rook-ceph-osd",
            "-o", "jsonpath={range .items[*]}{.metadata.labels.rook-ceph-osd-id}{\"\\t\"}{.spec.template.spec.containers[0].env}{\"\\n\"}{end}",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let osd_id_str = parts[0].trim();
        let env_json = parts[1].trim();
        if env_json.contains(osd_uuid) {
            if let Ok(id) = osd_id_str.parse::<i64>() {
                return Some(id);
            }
        }
    }
    None
}

/// Per-node: publish this node's disk inventory + its effective device list to
/// the status ConfigMap, and run this node's own OSD lifecycle. No shared-CR
/// writes happen here, so every node can run it concurrently without racing.
async fn publish_local(node: &str) -> Result<()> {
    let devices = get_devices();
    let our_fsid = cluster_fsid().await.unwrap_or_default();

    let meta: HashMap<String, Value> = devices
        .iter()
        .map(|d| (disk_id(d), disk_meta(d, &our_fsid)))
        .collect();

    // Read desired states; missing key → default USING
    let desired = read_desired().await;

    let effective: Vec<String> = classify(&devices, &our_fsid)
        .into_iter()
        .filter(|d| {
            let key = format!("{}--{}", node, disk_id(d));
            desired.get(&key).map(|v| v == "USING").unwrap_or(true)
        })
        .collect();

    write_status(node, &meta, &effective).await;

    if !our_fsid.is_empty() {
        // OSD lifecycle acts only on THIS node's own OSDs (deploy_node == node),
        // so it's safe to run on every node without coordination.
        migrate_osd_deployments(node, &our_fsid).await;
        purge_dead_osds(node, &devices, &our_fsid).await;
    }
    Ok(())
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

/// Detect OSD deployments on this node whose underlying disk has disappeared.
/// When the activate init container can't find its disk (device wiped or
/// removed), the pod crashes repeatedly and the OSD can never come back online.
/// Purging it removes the tombstone from Ceph's OSD map so the cluster can
/// recover; Rook will re-provision a fresh OSD on the next disk discovery if
/// the device is still in the CephCluster CR.
async fn purge_dead_osds(node: &str, devices: &[String], our_fsid: &str) {
    // Build the set of OSD UUIDs for every disk that is physically present.
    let live_uuids: std::collections::HashSet<String> = devices
        .iter()
        .filter(|d| !d.starts_with("loop"))
        .filter(|d| bluestore_fsid(d).as_deref() == Some(our_fsid))
        .filter_map(|d| bluestore_osd_uuid(d))
        .collect();

    let Ok(deploys) = kubectl::get_json(&[
        "get", "deployments", "-n", NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ]).await else { return };

    for deploy in deploys["items"].as_array().cloned().unwrap_or_default() {
        let name = deploy["metadata"]["name"].as_str().unwrap_or("");
        let deploy_node = deploy["spec"]["template"]["spec"]["nodeSelector"]
            ["kubernetes.io/hostname"].as_str().unwrap_or("");
        if deploy_node != node {
            continue;
        }

        // Check if the pod for this deployment is crashing (not ready).
        let replicas_ready = deploy["status"]["readyReplicas"].as_u64().unwrap_or(0);
        if replicas_ready > 0 {
            continue; // OSD is healthy — leave it alone
        }

        // Get the OSD UUID from the deployment env.
        let env = deploy["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let osd_uuid = env.iter()
            .find(|e| e["name"] == "ROOK_OSD_UUID")
            .and_then(|e| e["value"].as_str())
            .unwrap_or("")
            .to_string();
        let osd_id_str = deploy["metadata"]["labels"]["rook-ceph-osd-id"]
            .as_str()
            .unwrap_or("");
        let Ok(osd_id) = osd_id_str.parse::<i64>() else { continue };

        if osd_uuid.is_empty() || live_uuids.contains(&osd_uuid) {
            continue; // Disk is still here — may just be temporarily down
        }

        // The disk is gone. Only purge if the pod has been failing for a while —
        // avoid racing with a pod that's in its first few restart cycles.
        let init_restarts: u32 = deploy["status"]["conditions"]
            .as_array()
            .and_then(|_| {
                // Approximate: check if the pod's last-known restart count is high.
                // We use the deployment's unavailableReplicas as a rough signal.
                None::<u32>
            })
            .unwrap_or(0);
        let _ = init_restarts; // used implicitly via pod check below

        // Check if the pod has been restarting for >5 minutes (has a restart count > 3).
        let pod_name_prefix = format!("{name}-");
        let pods_out = kubectl::get_json(&[
            "get", "pods", "-n", NS, "-l", &format!("app=rook-ceph-osd,rook-ceph-osd-id={osd_id_str}"),
            "-o", "json",
        ]).await;
        let min_restarts = if let Ok(pods) = pods_out {
            pods["items"].as_array().cloned().unwrap_or_default()
                .iter()
                .filter(|p| p["spec"]["nodeName"].as_str() == Some(node))
                .flat_map(|p| p["status"]["initContainerStatuses"].as_array().cloned().unwrap_or_default())
                .map(|c| c["restartCount"].as_u64().unwrap_or(0))
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let _ = pod_name_prefix;

        if min_restarts < 5 {
            // Not yet sure — too early to purge
            continue;
        }

        // CRITICAL: never purge an OSD that still holds the only copy of data.
        // `ceph osd safe-to-destroy` returns this OSD's id only when every PG it
        // carries has copies on other OSDs. A transient BlueStore-header read
        // failure can make a present-but-flaky disk look "gone", so without this
        // gate a re-provision (which wipes the disk) would lose data. If it's not
        // safe, leave the OSD down and surface it for the user rather than purge.
        let safe_to_destroy = crate::kubectl::ceph_exec(&[
            "osd", "safe-to-destroy", &format!("osd.{osd_id}"), "-f", "json",
        ])
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v["safe_to_destroy"]
                .as_array()
                .map(|a| a.iter().any(|x| x.as_i64() == Some(osd_id)))
        })
        .unwrap_or(false);
        if !safe_to_destroy {
            tracing::warn!(
                "purge_dead_osds: osd.{osd_id} on {node} looks gone but is NOT safe-to-destroy \
                 (may hold the only copy of some data) — leaving it down for user action, not purging"
            );
            continue;
        }

        tracing::warn!(
            "purge_dead_osds: osd.{osd_id} on {node} has disk UUID {osd_uuid} but disk is gone \
             ({min_restarts} restart(s)) and is safe-to-destroy — purging"
        );
        if let Err(e) = crate::kubectl::ceph_exec(&[
            "osd", "purge", &format!("{osd_id}"), "--yes-i-really-mean-it",
        ]).await {
            tracing::warn!("purge_dead_osds: ceph osd purge {osd_id}: {e}");
            continue;
        }
        let _ = kubectl::run(&[
            "delete", "deployment", name, "-n", NS, "--ignore-not-found",
        ]).await;
        tracing::info!("purge_dead_osds: osd.{osd_id} purged and deployment deleted");
    }
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

/// Returns true if the device has any partition entries in /sys/block/{dev}/{dev}*.
/// Used to exclude system/boot disks that have a partition table.
fn has_partitions(device: &str) -> bool {
    std::fs::read_dir(format!("/sys/block/{device}"))
        .ok()
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with(device)
            })
        })
        .unwrap_or(false)
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

// ── Classify devices ──────────────────────────────────────────────────────────

fn classify(devices: &[String], our_fsid: &str) -> Vec<String> {
    let mut effective = Vec::new();
    for device in devices {
        if device.starts_with("loop") {
            effective.push(device.clone());
            continue;
        }
        match bluestore_fsid(device).as_deref() {
            None => {
                if has_partitions(device) {
                    tracing::debug!("{device}: has partition table — skipping (system/boot disk)");
                } else {
                    effective.push(device.clone());
                }
            }
            Some(fsid) if fsid == our_fsid => {
                tracing::debug!("{device}: our OSD, Rook will re-integrate");
                effective.push(device.clone());
            }
            Some(other) => {
                // Foreign BlueStore label — data from another Ceph cluster.
                // NEVER wipe automatically: a relocated disk, a DR reinstall
                // (which mints a new fsid), or a disk carried over from another
                // machine would be silently destroyed. Exclude it from Ceph and
                // surface it to the UI as "contains data from another system";
                // erasing only happens on explicit user confirmation.
                tracing::warn!("{device}: foreign BlueStore ({other}) — excluding, NOT wiping");
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
    // Expose OSD ID so the UI can correlate disks with OSD metrics.
    let osd_id: Option<i64> = if is_our_osd {
        bluestore_osd_uuid(device).and_then(|uuid| osd_id_from_uuid_sync(&uuid))
    } else {
        None
    };
    json!({
        "device": device,
        "model": model,
        "size_bytes": size_bytes,
        "is_loop": is_loop,
        "is_our_osd": is_our_osd,
        "osd_id": osd_id,
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

    let patch = json!({"spec": {"storage": {"nodes": desired}}}).to_string();
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
