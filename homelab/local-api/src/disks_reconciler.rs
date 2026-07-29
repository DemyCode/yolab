/// Disk reconciler — runs as a background task inside yolab-local-api.
/// Replaces the former Python osd-node-controller DaemonSet.
///
/// Every 30 seconds on each node:
///   1. Discover block devices via lsblk (type=disk, no partition children)
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
    // Resolve the system LV's OSD id so it can be driven ON/OFF like any disk.
    let osd_id = bluestore_osd_uuid(SYSTEM_OSD_DEV).and_then(|u| osd_id_from_uuid_sync(&u));
    json!({
        "device": SYSTEM_OSD_DEV,
        "model": "System disk",
        "size_bytes": system_osd_size_bytes(),
        "is_loop": true, // the UI renders is_loop as the built-in "System disk"
        "is_our_osd": true,
        "osd_id": osd_id,
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

    let mut meta: HashMap<String, Value> = devices
        .iter()
        .map(|d| (disk_id(d), disk_meta(d, &our_fsid)))
        .collect();
    if system_osd_present() {
        meta.insert(SYSTEM_OSD_ID.to_string(), system_osd_meta());
    }

    // Read desired states; missing key → default USING
    let desired = read_desired().await;

    // The system OSD LV is included unless the user explicitly turns it off,
    // then any pluggable disks the user is using.
    let mut effective: Vec<String> = Vec::new();
    if system_osd_present() {
        let key = format!("{node}--{SYSTEM_OSD_ID}");
        if desired.get(&key).map(|v| v == "USING").unwrap_or(true) {
            effective.push(SYSTEM_OSD_DEV.to_string());
        }
    }
    for d in classify(&devices, &our_fsid) {
        let key = format!("{}--{}", node, disk_id(&d));
        if desired.get(&key).map(|v| v == "USING").unwrap_or(true) {
            effective.push(d);
        }
    }

    write_status(node, &meta, &effective).await;

    // Auto-register newly discovered OSD disks in the config CM so the UI can
    // show and toggle them. Missing key already defaults to USING in the reconciler,
    // but writing it explicitly makes the toggle visible before first user action.
    if !our_fsid.is_empty() {
        auto_register_new_osds(node, &meta, &desired).await;
    }

    if !our_fsid.is_empty() {
        // OSD lifecycle acts only on THIS node's own OSDs (deploy_node == node),
        // so it's safe to run on every node without coordination.
        migrate_osd_deployments(node, &our_fsid).await;
        purge_dead_osds(node, &devices, &our_fsid).await;
    }

    // Drive each local OSD's active state toward the desired ON/OFF in the config
    // ConfigMap. This is the single actuator for crush weight + in/out — the same
    // reconcile whether a disk was just provisioned, wrongly left out, or has just
    // reconnected. Acts only on this node's OSDs (meta is local), so no coordination.
    reconcile_local_osds(node, &meta, &desired).await;
    Ok(())
}

/// Reconcile each local OSD's active state toward its desired ON/OFF.
///
/// Desired lives in the `yolab-disk-config` ConfigMap keyed by `{node}--{disk_id}`;
/// "OFF" means off, anything else (or missing) means on. `meta` maps each local
/// disk_id to its published metadata, including `osd_id` and `size_bytes`.
///
///   ON  → make it join and carry data: crush_weight>0 (set from size if 0) + osd in
///   OFF → make it leave: osd out (drains PGs to other OSDs; fine if slow)
///
/// Every step is idempotent and reads current state from `ceph osd df tree`, so a
/// disk that reconnects simply resumes from wherever it was left.
///
/// Fallback: any OSD under this node's CRUSH host with crush_weight=0 that could
/// not be matched to a disk_id (bluestore UUID lookup failed, e.g. USB disk) is
/// activated from the CRUSH tree's `kb` field. This ensures freshly provisioned
/// OSDs are activated even when osd_id resolution is unavailable.
async fn reconcile_local_osds(
    node: &str,
    meta: &HashMap<String, Value>,
    desired: &HashMap<String, String>,
) {
    // Parse CRUSH/reweight state from `ceph osd df tree`. Keep the full node list
    // so we can also do the host-bucket fallback pass below.
    let crush_nodes: Vec<Value> = match kubectl::ceph_exec(&["osd", "df", "tree", "-f", "json"]).await {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v["nodes"].as_array().cloned())
            .unwrap_or_default(),
        Err(_) => return,
    };

    // (crush_weight, reweight, kb) per OSD id.
    let mut state: HashMap<i64, (f64, f64, u64)> = HashMap::new();
    for n in &crush_nodes {
        if n["type"].as_str() != Some("osd") { continue; }
        if let Some(id) = n["id"].as_i64() {
            let cw = n["crush_weight"].as_f64().unwrap_or(0.0);
            let rw = n["reweight"].as_f64().unwrap_or(1.0);
            let kb = n["kb"].as_u64().unwrap_or(0);
            state.insert(id, (cw, rw, kb));
        }
    }

    let mut handled: std::collections::HashSet<i64> = std::collections::HashSet::new();

    for (disk_id, m) in meta {
        let Some(osd_id) = m["osd_id"].as_i64() else { continue }; // osd_id unresolved → fallback handles it
        handled.insert(osd_id);
        let key = format!("{node}--{disk_id}");
        let want_on = desired.get(&key).map(|v| v != "OFF").unwrap_or(true);
        let osd = format!("osd.{osd_id}");
        let (crush_weight, reweight, kb) = state.get(&osd_id).copied().unwrap_or((0.0, 1.0, 0));

        if want_on {
            // Fresh OSDs come up with crush_weight 0 (osd_crush_initial_weight=0);
            // set it from disk size (prefer live `kb` from Ceph, fall back to lsblk bytes).
            if crush_weight == 0.0 {
                let weight = weight_tib_from(kb, m["size_bytes"].as_u64().unwrap_or(0));
                if weight > 0.0 {
                    tracing::info!("{osd} ({disk_id}): ON, crush_weight=0 — activating (weight={weight:.5})");
                    let _ = kubectl::ceph_exec(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")]).await;
                }
            }
            if reweight < 0.5 {
                tracing::info!("{osd} ({disk_id}): ON, reweight={reweight:.2} — marking in");
                let _ = kubectl::ceph_exec(&["osd", "in", &osd]).await;
            }
        } else if reweight > 0.5 {
            // OFF: drain by marking out. Keep crush weight so re-enabling is a
            // single `osd in`. Full removal/purge is a separate, deliberate step.
            tracing::info!("{osd} ({disk_id}): OFF, reweight={reweight:.2} — draining (osd out)");
            let _ = kubectl::ceph_exec(&["osd", "out", &osd]).await;
        }
    }

    // Fallback: activate any OSD under this node's CRUSH host whose osd_id we
    // couldn't resolve via bluestore header (handled set above). These are OSDs
    // that Rook provisioned and placed in CRUSH but whose UUID we can't read back
    // (e.g. USB/external disks, or a failed bluestore read). Since we don't know
    // which disk_id maps to this OSD we can only activate (not deactivate) here.
    let host_children: Vec<i64> = crush_nodes.iter()
        .find(|n| n["type"].as_str() == Some("host") && n["name"].as_str() == Some(node))
        .and_then(|h| h["children"].as_array())
        .map(|c| c.iter().filter_map(|v| v.as_i64().filter(|&id| id >= 0)).collect())
        .unwrap_or_default();

    for osd_id in host_children {
        if handled.contains(&osd_id) { continue; }
        let (crush_weight, reweight, kb) = state.get(&osd_id).copied().unwrap_or((0.0, 1.0, 0));
        if crush_weight == 0.0 && reweight > 0.5 && kb > 0 {
            let osd = format!("osd.{osd_id}");
            let weight = weight_tib_from(kb, 0);
            tracing::info!("{osd}: node={node}, crush_weight=0, osd_id unresolved — activating (weight={weight:.5})");
            let _ = kubectl::ceph_exec(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")]).await;
        }
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

/// Write a `USING` entry to the config CM for any disk that is our OSD but has
/// no existing config entry. This makes the UI toggle visible on first plug-in,
/// without changing runtime behaviour (missing key already defaults to USING).
async fn auto_register_new_osds(
    node: &str,
    meta: &HashMap<String, Value>,
    desired: &HashMap<String, String>,
) {
    let mut new_entries: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        if !m["is_our_osd"].as_bool().unwrap_or(false) { continue; }
        let key = format!("{node}--{disk_id}");
        if !desired.contains_key(&key) {
            new_entries.insert(key, "USING".to_string());
        }
    }
    if new_entries.is_empty() { return; }

    let patch = json!({"data": new_entries}).to_string();
    if let Err(e) = kubectl::run(&[
        "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
    ]).await {
        // Config CM may not exist yet on a fresh node; create it first.
        let _ = kubectl::run(&["create", "configmap", CONFIG_CM, "-n", NS]).await;
        if let Err(e2) = kubectl::run(&[
            "patch", "configmap", CONFIG_CM, "-n", NS, "--type", "merge", "-p", &patch,
        ]).await {
            tracing::warn!("auto_register_new_osds: {e}, then {e2}");
        } else {
            tracing::info!("auto_register_new_osds: registered {} new disk(s) on {node}", new_entries.len());
        }
    } else {
        tracing::info!("auto_register_new_osds: registered {} new disk(s) on {node}", new_entries.len());
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

        // Only purge if the pod has been crashing for a while — too early to purge
        // on first few restart cycles while the disk might just be temporarily slow.
        let pods_out = kubectl::get_json(&[
            "get", "pods", "-n", NS, "-l", &format!("app=rook-ceph-osd,rook-ceph-osd-id={osd_id_str}"),
            "-o", "json",
        ]).await;
        let max_restarts = if let Ok(pods) = pods_out {
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

        if max_restarts < 5 {
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
             ({max_restarts} restart(s)) and is safe-to-destroy — purging"
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
    let osd_id: Option<i64> = if is_our_osd {
        bluestore_osd_uuid(device).and_then(|uuid| osd_id_from_uuid_sync(&uuid))
    } else {
        None
    };
    json!({
        "device": device,
        "model": model,
        "size_bytes": size_bytes,
        "is_our_osd": is_our_osd,
        "foreign_ceph": foreign_ceph,
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
