/// Disk reconciler — runs as a background task inside yolab-local-api.
///
/// Every 30 seconds on each node:
///   1. Discover block devices via lsblk (type=disk, no partition children)
///   2. Classify them (ours / clean / foreign-wipe)
///   3. Publish metadata to yolab-disk-status ConfigMap
///   4. Read desired states from yolab-disk-config ConfigMap
///   5. Create or tear down OSDs so reality matches those desired states
///
/// SHARED STATE IS STILL THE SOURCE OF TRUTH
/// -----------------------------------------
/// Ceph no longer runs inside Kubernetes (see homelab/nixos/ceph/ for why: a mon
/// that is a pod makes an RBD-backed containerd store impossible). But the
/// control plane for *which disks are in Ceph* deliberately stays in
/// Kubernetes: `yolab-disk-config` holds the ON/OFF the user sets in the UI,
/// `yolab-disk-status` holds what each node actually sees, and a
/// coordination.k8s.io Lease still elects a single writer. Any node's UI can
/// therefore flip a disk on any other node, exactly as before, and the contract
/// the frontend consumes is unchanged.
///
/// What changed is only the mechanism underneath:
///   - was: patch the CephCluster CR and let Rook's operator provision
///   - now: run `ceph-volume lvm create` and enable `yolab-ceph-osd@<id>`
///
/// That removes the whole class of bugs where Rook fought this loop — the
/// delete/recreate war when Rook rediscovered BlueStore data on a drained disk,
/// and `removeOSDsIfOutAndSafeToRemove` stalling for 45h. Nothing races us now,
/// so removal no longer has to be one atomic burst inside a single tick.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Read,
    path::Path,
};
use tokio::time::{sleep, Duration};

use crate::{ceph_cli, kubectl};

/// Kept as "rook-ceph" even though Rook no longer runs the cluster: it is where
/// the existing ConfigMaps, Lease and CSI resources already live, and renaming
/// it would be a migration with no benefit. It is a namespace name, not a
/// statement about who runs Ceph.
const NS: &str = "rook-ceph";
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

/// `is_our_osd` used to be hardcoded `true` here on the assumption the system
/// LV is always ours — true right up until the disk-removal flow gained the
/// ability to actually drain, purge, and wipe *any* OSD including this one.
/// Once that happens, the hardcoded `true` never updates: the UI computes
/// "being removed" from `!desired_on && connected && is_our_osd`, so a wiped
/// system disk stayed stuck on that label forever with no way to reach the
/// "safe to switch on again" state. Now reads the real on-disk label, exactly
/// like `disk_meta` does for pluggable disks.
fn system_osd_meta(our_fsid: &str) -> Value {
    let fsid = bluestore_fsid(SYSTEM_OSD_DEV);
    let is_our_osd = !our_fsid.is_empty() && fsid.as_deref() == Some(our_fsid);
    let foreign_ceph = fsid.is_some() && !is_our_osd;
    json!({
        "device": SYSTEM_OSD_DEV,
        "model": "System disk",
        "size_bytes": system_osd_size_bytes(),
        "is_loop": true, // the UI renders is_loop as the built-in "System disk"
        "is_our_osd": is_our_osd,
        "foreign_ceph": foreign_ceph,
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
        // The lease is still taken even though there is no longer a shared CR to
        // write: each node now owns the OSDs on its own disks, so the disk loop
        // itself needs no single writer. Other leader-only controllers (the
        // topology controller, which sets pool size and mon count) gate on
        // `is_reconcile_leader`, so this keeps electing one.
        let _ = leader::try_acquire(&node).await;
        sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Build a disk_id → osd_id map from `ceph-volume lvm list`, which reads the
/// LVM tags ceph-volume itself wrote when it created each OSD.
///
/// This used to read `ceph osd metadata` over the mon. The local view is
/// strictly better here: it is the same information, but it needs no mon, so
/// the disk list keeps working when the cluster is unhealthy — which is exactly
/// when someone is looking at the Storage page. (The `node` argument is no
/// longer needed to filter by hostname, since ceph-volume only ever reports
/// this host's OSDs, but is kept so callers read the same.)
async fn fetch_disk_to_osd(_node: &str, meta: &HashMap<String, Value>) -> HashMap<String, i64> {
    // Build full device path → disk_id from our local inventory.
    // Index both the stored path and its canonical (symlink-resolved) path so
    // that /dev/mapper/pool-ceph (a symlink → /dev/dm-1) matches whichever
    // path Ceph actually opened and reports in bluestore_bdev_dev_node.
    let mut device_to_disk_id: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        let Some(dev) = m["device"].as_str() else { continue };
        device_to_disk_id.insert(canonical_device(dev), disk_id.clone());
    }

    let local = match ceph_cli::local_osds().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fetch_disk_to_osd: {e}");
            return HashMap::new();
        }
    };

    let mut result = HashMap::new();
    for (dev_path, osd_id) in local {
        // Canonicalise BOTH sides. Matching only our own paths against
        // ceph-volume's raw string silently fails for LVM: we hold
        // /dev/mapper/pool-ceph while ceph-volume reports /dev/pool/ceph, and
        // the two never compare equal even though both are symlinks to the same
        // /dev/dm-N. Observed live — the system disk's OSD went unrecognised, so
        // the reconciler treated it as an unprovisioned disk on every tick.
        let key = canonical_device(&dev_path);
        if let Some(disk_id) = device_to_disk_id.get(&key) {
            result.insert(disk_id.clone(), osd_id);
        }
    }
    result
}

/// Resolve a device path to its canonical form, falling back to the input when
/// it cannot be resolved (the device is gone, or we are in a unit test).
///
/// Split out and used on every path that compares device identities, because
/// /dev holds several names for the same LVM volume — /dev/mapper/pool-ceph,
/// /dev/pool/ceph and /dev/dm-1 are all the same disk — and comparing the
/// wrong pair makes an existing OSD look like a blank disk.
fn canonical_device(dev: &str) -> String {
    let full = if dev.starts_with('/') {
        dev.to_string()
    } else {
        format!("/dev/{dev}")
    };
    std::fs::canonicalize(&full)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(full)
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
        meta.insert(SYSTEM_OSD_ID.to_string(), system_osd_meta(&our_fsid));
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

/// Reconcile each local disk toward its desired ON/OFF state.
///
///   ON, no OSD yet  → `ceph-volume lvm create` + enable yolab-ceph-osd@<id>
///   ON, OSD exists  → crush_weight > 0 (set from disk size if 0) + osd in
///   OFF             → osd out (drains PGs) → safe-to-destroy → stop+disable the
///                     unit → `ceph osd purge` → wipe the BlueStore label
///
/// Unlike the Rook version, nothing else is trying to manage these OSDs, so the
/// teardown no longer has to complete inside one tick to beat an operator to
/// the punch. Each step is idempotent and simply resumes on the next pass.
async fn reconcile_local_osds(
    node: &str,
    meta: &HashMap<String, Value>,
    desired: &HashMap<String, String>,
    disk_to_osd: &HashMap<String, i64>,
) {
    // Everything below needs a live cluster. Bail rather than misread silence
    // as "no OSDs", which would look like every disk needs creating.
    if !ceph_cli::reachable().await {
        tracing::debug!("reconcile_local_osds: ceph unreachable, skipping this tick");
        return;
    }

    // Create OSDs for disks switched ON that do not have one yet. This is the
    // half Rook used to do in response to a CephCluster patch.
    for (disk_id, m) in meta {
        let key = format!("{node}--{disk_id}");
        let want_on = desired.get(&key).map(|v| v == "ON" || v == "USING").unwrap_or(false);
        if !want_on || disk_to_osd.contains_key(disk_id) {
            continue;
        }
        if let Some(reason) = refuse_osd_creation(m) {
            tracing::warn!("{disk_id}: desired ON but not creating an OSD — {reason}");
            continue;
        }
        let Some(device) = m["device"].as_str().filter(|d| !d.is_empty()) else {
            continue;
        };
        let dev_path = if device.starts_with('/') {
            device.to_string()
        } else {
            format!("/dev/{device}")
        };

        tracing::info!("{disk_id} ({dev_path}): desired ON with no OSD — creating");
        match ceph_cli::ceph_volume(&["lvm", "create", "--bluestore", "--data", &dev_path, "--no-systemd"]).await {
            Ok(_) => {
                // Re-read the mapping so we learn the id Ceph just assigned;
                // it cannot be known before creation.
                if let Ok(local) = ceph_cli::local_osds().await {
                    let want = canonical_device(&dev_path);
                    if let Some((_, osd_id)) =
                        local.iter().find(|(d, _)| canonical_device(d) == want)
                    {
                        start_osd_unit(*osd_id).await;
                    } else {
                        tracing::warn!(
                            "{disk_id}: created an OSD but could not match it back to {dev_path} — \
                             it will be started by the next reconcile tick"
                        );
                    }
                }
            }
            Err(e) => tracing::warn!("{disk_id}: ceph-volume create failed: {e}"),
        }
    }

    let crush_nodes: Vec<Value> = match ceph_cli::ceph_json(&["osd", "df", "tree"]).await {
        Ok(v) => v["nodes"].as_array().cloned().unwrap_or_default(),
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
            // Converge the daemon's own state, not just Ceph's view of it.
            // Enabling used to happen only in the creation branch, so an OSD
            // whose unit was stopped — a failed enable, a manual systemctl, a
            // crash past the restart limit — stayed down forever with the
            // reconciler reporting nothing wrong. This is a reconciler; it has
            // to assert the desired state every tick, not once at birth.
            ensure_osd_unit_running(osd_id).await;

            // A freshly created OSD starts at weight 0 (osd_crush_initial_weight)
            // so it attracts no data until the user activates it.
            if crush_weight == 0.0 {
                let weight = weight_tib_from(kb, m["size_bytes"].as_u64().unwrap_or(0));
                if weight > 0.0 {
                    tracing::info!("{osd} ({disk_id}): crush_weight=0 — setting weight={weight:.5}");
                    let _ = ceph_cli::ceph(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")]).await;
                }
            }
            if reweight < 0.5 {
                tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=ON — marking in");
                let _ = ceph_cli::ceph(&["osd", "in", &osd]).await;
            }
        } else if reweight > 0.5 {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=OFF — marking out");
            let _ = ceph_cli::ceph(&["osd", "out", &osd]).await;
        } else {
            // Already out and drained. Under Rook this was the hard part: its
            // operator would rediscover the still-valid BlueStore data and
            // recreate the OSD deployment within ~15-35s, so teardown had to be
            // one atomic burst to beat it, and `removeOSDsIfOutAndSafeToRemove`
            // could stall for 45h with no fallback.
            //
            // Nothing competes for these OSDs now — this process is the only
            // supervisor — so the sequence is just: stop the daemon, purge,
            // wipe. Each step is idempotent and resumes on the next tick if
            // interrupted.
            //
            // safe-to-destroy is re-confirmed immediately before the purge and
            // is never inferred from reweight or PG counts; inferring it from
            // `pg ls-by-osd` once caused real data loss.
            if !ceph_cli::osd_safe_to_destroy(osd_id).await {
                tracing::debug!("{osd} ({disk_id}): out but not yet safe-to-destroy — waiting");
                continue;
            }

            // Stop the daemon before purging. Purging while it still runs is
            // the EBUSY race the old code had to guard against separately.
            disable_osd_unit(osd_id).await;

            if !ceph_cli::osd_safe_to_destroy(osd_id).await {
                tracing::warn!("{osd} ({disk_id}): stopped, but no longer reports safe-to-destroy — leaving it alone");
                continue;
            }

            match ceph_cli::ceph(&["osd", "purge", &osd, "--yes-i-really-mean-it"]).await {
                Ok(_) => {
                    tracing::info!("{osd} ({disk_id}): purged");
                    if let Some(device) = m["device"].as_str().filter(|d| !d.is_empty()) {
                        tracing::info!("{osd} ({disk_id}): wiping BlueStore label on {device}");
                        wipe_device(device).await;
                    }
                }
                Err(e) => tracing::warn!("{osd} ({disk_id}): purge failed: {e}"),
            }
        }
    }

    purge_drained_osds(node, &crush_nodes, disk_to_osd).await;
}

/// Whether a disk switched ON must NOT be handed to `ceph-volume lvm create`,
/// and why. `None` means it is safe to create.
///
/// This is the last thing standing between a transient error and destroyed user
/// data, so it is deliberately a pure function over the disk's own metadata and
/// is tested exhaustively below.
///
/// The danger is not the obvious one. `ceph-volume lvm create` wipes whatever is
/// on the device, and the creation loop fires for any ON disk missing from
/// `disk_to_osd` — a map built from `ceph-volume lvm list`, which returns an
/// EMPTY map when that command fails. So a single transient failure makes every
/// healthy OSD look like a blank disk awaiting provisioning. Checking
/// `foreign_ceph` alone does not save us: our *own* OSDs are not foreign.
///
/// Therefore: only ever create on a disk carrying no BlueStore label at all.
/// A disk that has one already holds an OSD — ours or a stranger's — and the
/// right response to it missing from the map is to complain, never to wipe.
/// Both flags come from reading the on-disk superblock, so they stay correct
/// even when no mon is reachable.
fn refuse_osd_creation(m: &Value) -> Option<&'static str> {
    if m["foreign_ceph"].as_bool().unwrap_or(false) {
        return Some("it holds data from another Ceph cluster");
    }
    if m["is_our_osd"].as_bool().unwrap_or(false) {
        return Some(
            "it already carries this cluster's BlueStore label, so an OSD exists on it \
             (if Ceph does not list that OSD, ceph-volume metadata is the problem — \
             creating here would destroy live data)",
        );
    }
    if m["device"].as_str().filter(|d| !d.is_empty()).is_none() {
        return Some("it has no device path");
    }
    None
}

/// Parse `ceph-volume lvm list --format json` into (device path, osd id) pairs.
///
/// Split out and made pure so the shape of ceph-volume's output is pinned by
/// tests rather than discovered in production. ceph-volume prints `-->` progress
/// lines to stdout when it cannot write its own log file, so anything before the
/// first `{` is stripped rather than failing the parse — that failure would
/// otherwise surface as an empty OSD map, which is the dangerous state described
/// on `refuse_osd_creation`.
pub(crate) fn parse_lvm_list(raw: &str) -> Result<Vec<(String, i64)>> {
    let json_start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object in ceph-volume output"))?;
    let v: Value = serde_json::from_str(&raw[json_start..]).context("parse ceph-volume lvm list")?;

    let mut out = Vec::new();
    let Some(map) = v.as_object() else {
        return Ok(out);
    };
    for (osd_id, entries) in map {
        let Ok(id) = osd_id.parse::<i64>() else {
            continue;
        };
        let Some(list) = entries.as_array() else {
            continue;
        };
        for e in list {
            if let Some(devs) = e["devices"].as_array() {
                for d in devs {
                    if let Some(path) = d.as_str().filter(|p| !p.is_empty()) {
                        out.push((path.to_string(), id));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Start this OSD's unit if it is not already running. Cheap enough to call on
/// every tick: `is-active` is a bus query, and the start only runs when
/// something is actually wrong.
///
/// This is what converges an OSD whose daemon died — a failed start, a manual
/// systemctl stop, a crash past the restart limit. Without it an OSD could sit
/// created-but-down indefinitely with the reconciler reporting nothing wrong,
/// which is exactly what a read-only /etc/systemd/system produced.
async fn ensure_osd_unit_running(osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    let active = tokio::process::Command::new("systemctl")
        .args(["is-active", "--quiet", &unit])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if active {
        return;
    }
    tracing::warn!("osd.{osd_id}: {unit} is not running — starting it");
    start_osd_unit(osd_id).await;
}

/// Start this OSD's systemd instance.
///
/// `start`, never `enable`. Enabling writes a symlink into
/// /etc/systemd/system, which on NixOS is a read-only Nix store path, so
/// `systemctl enable` fails with "Read-only file system" — observed live with
/// both OSDs created and neither running. Persistence across reboots comes from
/// the declarative yolab-ceph-osd-activate unit, which enumerates prepared OSDs
/// from ceph-volume and starts an instance for each.
async fn start_osd_unit(osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: starting {unit}");
    match tokio::process::Command::new("systemctl")
        .args(["start", &unit])
        .output()
        .await
    {
        Ok(o) if o.status.success() => tracing::info!("osd.{osd_id}: {unit} started"),
        Ok(o) => tracing::warn!(
            "osd.{osd_id}: starting {unit} failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("osd.{osd_id}: could not run systemctl: {e}"),
    }
}

/// Stop this OSD's systemd instance and wait for the process to actually be
/// gone. Purging an OSD whose daemon still holds the device fails with EBUSY,
/// so this must complete before any purge.
async fn disable_osd_unit(osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: stopping {unit}");
    // `stop`, not `disable --now`, for the same reason start is not enable:
    // disabling touches the read-only /etc/systemd/system. Nothing needs
    // un-enabling anyway — yolab-ceph-osd-activate derives what to start from
    // ceph-volume, and a purged OSD disappears from there on its own.
    let _ = tokio::process::Command::new("systemctl")
        .args(["stop", &unit])
        .output()
        .await;

    // `systemctl disable --now` returns once systemd has reaped the unit, but
    // give the device a moment to be released before anything touches it.
    for _ in 0..15 {
        let active = tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", &unit])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !active {
            return;
        }
        sleep(Duration::from_secs(1)).await;
    }
    tracing::warn!("osd.{osd_id}: {unit} still active after 15s");
}

/// Zero enough of a device's start to destroy its BlueStore superblock and any
/// LVM headers, so Rook's OSD discovery no longer finds a valid OSD there and
/// stops re-provisioning it. Only ever called after our own successful `ceph
/// osd purge` of that exact OSD in this same call — never on a disk we merely
/// suspect is drained.
async fn wipe_device(device: &str) {
    let dev_path = if device.starts_with('/') { device.to_string() } else { format!("/dev/{device}") };
    let out = tokio::process::Command::new("dd")
        .args(["if=/dev/zero", &format!("of={dev_path}"), "bs=1M", "count=100", "oflag=direct"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => tracing::info!("wipe_device: {dev_path} zeroed"),
        Ok(o) => tracing::warn!("wipe_device: dd on {dev_path} failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => tracing::warn!("wipe_device: could not run dd on {dev_path}: {e}"),
    }
}

/// Purge OSDs on this node that have been fully drained and whose Rook
/// deployment is already gone. Safe conditions (all must hold):
///   1. OSD is in the CRUSH tree under this node's host bucket
///   2. Disk is no longer locally present (not in disk_to_osd)
///   3. OSD is down + reweight ≤ 0.5 (out)
///   4. Rook deployment is gone (no EBUSY — daemon is not running)
///   5. `ceph osd safe-to-destroy` confirms no PG data remains
async fn purge_drained_osds(
    node: &str,
    crush_nodes: &[Value],
    disk_to_osd: &HashMap<String, i64>,
) {
    // OSD IDs that belong to this node's host bucket in the CRUSH tree.
    let host_osd_ids: std::collections::HashSet<i64> = crush_nodes
        .iter()
        .find(|n| n["type"].as_str() == Some("host") && n["name"].as_str() == Some(node))
        .and_then(|h| h["children"].as_array())
        .map(|c| c.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    // OSD IDs whose disk is currently present on this node.
    let active_osd_ids: std::collections::HashSet<i64> = disk_to_osd.values().copied().collect();

    for n in crush_nodes {
        if n["type"].as_str() != Some("osd") { continue; }
        let Some(osd_id) = n["id"].as_i64() else { continue };
        if !host_osd_ids.contains(&osd_id) { continue; }   // not this node's OSD
        if active_osd_ids.contains(&osd_id) { continue; }  // disk still here, main loop handles it

        let reweight = n["reweight"].as_f64().unwrap_or(1.0);
        let status   = n["status"].as_str().unwrap_or("up");
        if reweight > 0.5 || status != "down" { continue; } // not fully drained/stopped

        // The daemon must not be running — never purge underneath a live OSD.
        disable_osd_unit(osd_id).await;

        // Confirm no PG data remains before destroying the OSD record.
        if !ceph_cli::osd_safe_to_destroy(osd_id).await {
            tracing::info!("osd.{osd_id}: disk gone but not yet safe-to-destroy — waiting");
            continue;
        }

        tracing::info!("osd.{osd_id}: disk gone, out, safe-to-destroy — purging from Ceph");
        match ceph_cli::ceph(&["osd", "purge", &format!("osd.{osd_id}"), "--yes-i-really-mean-it"]).await {
            Ok(_) => tracing::info!("osd.{osd_id}: purged"),
            Err(e) => tracing::warn!("osd.{osd_id}: purge failed: {e}"),
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
    let serial = std::fs::read_to_string(format!("/sys/block/{device}/device/serial")).ok();
    disk_id_from(device, serial.as_deref())
}

/// Split from `disk_id` so the sanitizing rules can be tested without a real
/// /sys/block entry. A disk's id ends up as a ConfigMap *key*, so anything the
/// vendor put in the serial has to come out as `[a-z0-9-]`.
fn disk_id_from(device: &str, serial: Option<&str>) -> String {
    if let Some(serial) = serial {
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

/// The cluster's own fsid, straight from the local mon. Returns None when Ceph
/// is unreachable — never a default — because callers compare it against a
/// disk's BlueStore label to tell our disks from a stranger's, and an empty
/// string would make every foreign disk match.
async fn cluster_fsid() -> Option<String> {
    ceph_cli::cluster_fsid().await
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn node_name() -> Result<String> {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .context("read /etc/hostname")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const OURS: &str = "11111111-2222-3333-4444-555555555555";
    const THEIRS: &str = "99999999-8888-7777-6666-555555555555";

    /// Build a 4096-byte BlueStore superblock carrying `fsid`, exactly as
    /// `bluestore_fsid` expects to find it: magic at offset 0, then the
    /// length-prefixed `ceph_fsid` key/value pair somewhere in the block.
    fn bluestore_label(fsid: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];
        buf[..BLUESTORE_MAGIC.len()].copy_from_slice(BLUESTORE_MAGIC);
        let at = 512;
        buf[at..at + CEPH_FSID_KEY.len()].copy_from_slice(CEPH_FSID_KEY);
        let vs = at + CEPH_FSID_KEY.len();
        buf[vs..vs + 4].copy_from_slice(&36u32.to_le_bytes());
        buf[vs + 4..vs + 4 + fsid.len()].copy_from_slice(fsid.as_bytes());
        buf
    }

    /// Writes `bytes` into `dir` and returns the absolute path.
    /// `read_bluestore_header` takes any path starting with `/` verbatim, so a
    /// regular file stands in for a block device.
    fn fake_device(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path.to_str().unwrap().to_string()
    }

    // ── is_uuid ───────────────────────────────────────────────────────────────

    #[test]
    fn is_uuid_accepts_a_canonical_uuid() {
        assert!(is_uuid(OURS));
        assert!(is_uuid("deadbeef-DEAD-beef-DEAD-beefdeadbeef")); // hex is case-insensitive
    }

    /// The single most important assertion in this file.
    ///
    /// `classify`'s caller passes `cluster_fsid().await.unwrap_or_default()`, so
    /// `our_fsid` is `""` whenever Ceph is unreachable. If `bluestore_fsid` could
    /// ever return `Some("")`, that empty string would compare *equal* to
    /// `our_fsid` and every foreign disk on the machine would be handed to Rook
    /// and wiped. The only thing standing between that and a user's data is this
    /// function returning false for the empty string.
    #[test]
    fn is_uuid_rejects_the_empty_string() {
        assert!(!is_uuid(""));
    }

    #[test]
    fn is_uuid_rejects_malformed_shapes() {
        assert!(!is_uuid("1111-2222-3333-4444")); // 4 groups, not 5
        assert!(!is_uuid("11111111-2222-3333-4444-555555555555-6")); // 6 groups
        assert!(!is_uuid("1111111-2222-3333-4444-555555555555")); // group 1 too short
        assert!(!is_uuid("gggggggg-2222-3333-4444-555555555555")); // not hex
        assert!(!is_uuid("11111111 2222 3333 4444 555555555555")); // spaces, not dashes
        assert!(!is_uuid("----")); // five empty groups
    }

    // ── bluestore_fsid ────────────────────────────────────────────────────────

    #[test]
    fn bluestore_fsid_reads_a_well_formed_label() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        assert_eq!(bluestore_fsid(&dev).as_deref(), Some(OURS));
    }

    #[test]
    fn bluestore_fsid_returns_none_without_the_magic() {
        let dir = tempfile::tempdir().unwrap();
        // A blank disk: right size, no BlueStore magic.
        let dev = fake_device(&dir, "sdb", &vec![0u8; 4096]);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_for_a_device_shorter_than_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sdc", b"bluestore block device\n");
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_when_the_device_does_not_exist() {
        assert_eq!(bluestore_fsid("/nonexistent/definitely-not-a-device"), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_when_the_length_prefix_is_not_36() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = bluestore_label(OURS);
        let vs = 512 + CEPH_FSID_KEY.len();
        buf[vs..vs + 4].copy_from_slice(&16u32.to_le_bytes());
        let dev = fake_device(&dir, "sdd", &buf);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    /// A label whose value is present but garbage must read as "no label", never
    /// as an empty-string fsid — see `is_uuid_rejects_the_empty_string`.
    #[test]
    fn bluestore_fsid_never_returns_a_non_uuid_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = bluestore_label(OURS);
        let vs = 512 + CEPH_FSID_KEY.len();
        buf[vs + 4..vs + 40].fill(b' '); // 36 bytes of whitespace: right length, not a uuid
        let dev = fake_device(&dir, "sde", &buf);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    // ── classify ──────────────────────────────────────────────────────────────

    #[test]
    fn classify_includes_disks_with_no_ceph_label() {
        let dir = tempfile::tempdir().unwrap();
        let blank = fake_device(&dir, "sda", &vec![0u8; 4096]);
        assert_eq!(classify(std::slice::from_ref(&blank), OURS), vec![blank]);
    }

    #[test]
    fn classify_includes_our_own_osds_so_rook_can_reintegrate_them() {
        let dir = tempfile::tempdir().unwrap();
        let ours = fake_device(&dir, "sda", &bluestore_label(OURS));
        assert_eq!(classify(std::slice::from_ref(&ours), OURS), vec![ours]);
    }

    #[test]
    fn classify_excludes_disks_labelled_by_another_ceph_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = fake_device(&dir, "sda", &bluestore_label(THEIRS));
        assert!(
            classify(&[theirs], OURS).is_empty(),
            "a disk holding another cluster's data must never be offered to Rook"
        );
    }

    /// `cluster_fsid()` returns None — and the caller `unwrap_or_default()`s it to
    /// `""` — whenever the CephCluster CR has no status yet: first boot, a restart,
    /// or any API blip. In that window we cannot tell our own disks from a
    /// stranger's, so every labelled disk must be held back. Only genuinely blank
    /// disks stay eligible.
    #[test]
    fn classify_excludes_every_labelled_disk_when_our_fsid_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ours = fake_device(&dir, "sda", &bluestore_label(OURS));
        let theirs = fake_device(&dir, "sdb", &bluestore_label(THEIRS));
        let blank = fake_device(&dir, "sdc", &vec![0u8; 4096]);

        let effective = classify(&[ours, theirs, blank.clone()], "");
        assert_eq!(effective, vec![blank]);
    }

    #[test]
    fn classify_is_sorted_and_independent_of_input_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = fake_device(&dir, "aaa", &vec![0u8; 4096]);
        let b = fake_device(&dir, "bbb", &vec![0u8; 4096]);
        let forward = classify(&[a.clone(), b.clone()], OURS);
        let reverse = classify(&[b, a], OURS);
        assert_eq!(forward, reverse);
        assert!(forward[0] < forward[1]);
    }

    #[test]
    fn classify_handles_an_empty_device_list() {
        assert!(classify(&[], OURS).is_empty());
    }

    // ── disk_meta ─────────────────────────────────────────────────────────────

    #[test]
    fn disk_meta_flags_our_own_osd() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        let m = disk_meta(&dev, OURS);
        assert_eq!(m["is_our_osd"], json!(true));
        assert_eq!(m["foreign_ceph"], json!(false));
    }

    #[test]
    fn disk_meta_flags_a_foreign_cluster_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(THEIRS));
        let m = disk_meta(&dev, OURS);
        assert_eq!(m["is_our_osd"], json!(false));
        assert_eq!(
            m["foreign_ceph"],
            json!(true),
            "the UI needs this flag to ask before erasing"
        );
    }

    /// With an unknown cluster fsid, our own disk is indistinguishable from a
    /// stranger's, so it is reported as foreign — the conservative reading, and
    /// the one that makes the UI ask rather than assume.
    #[test]
    fn disk_meta_treats_a_labelled_disk_as_foreign_when_our_fsid_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        let m = disk_meta(&dev, "");
        assert_eq!(m["is_our_osd"], json!(false));
        assert_eq!(m["foreign_ceph"], json!(true));
    }

    #[test]
    fn disk_meta_reports_a_blank_disk_as_neither_ours_nor_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &vec![0u8; 4096]);
        let m = disk_meta(&dev, OURS);
        assert_eq!(m["is_our_osd"], json!(false));
        assert_eq!(m["foreign_ceph"], json!(false));
    }

    // ── disk_id ───────────────────────────────────────────────────────────────

    #[test]
    fn disk_id_prefers_the_serial_number() {
        assert_eq!(disk_id_from("sda", Some("S3Z1NB0K")), "serial-s3z1nb0k");
    }

    #[test]
    fn disk_id_replaces_characters_a_configmap_key_cannot_hold() {
        // ConfigMap keys are [-._a-zA-Z0-9]; vendors ship spaces, slashes and colons.
        assert_eq!(
            disk_id_from("sda", Some("WD/Blue 500:GB")),
            "serial-wd-blue-500-gb"
        );
    }

    #[test]
    fn disk_id_trims_surrounding_whitespace_and_dashes() {
        assert_eq!(disk_id_from("sda", Some("  ABC123  ")), "serial-abc123");
        assert_eq!(disk_id_from("sda", Some("__ABC__")), "serial-abc");
    }

    #[test]
    fn disk_id_falls_back_to_the_device_name_without_a_usable_serial() {
        assert_eq!(disk_id_from("sda", None), "dev-sda");
        assert_eq!(disk_id_from("sda", Some("")), "dev-sda");
        assert_eq!(disk_id_from("sda", Some("   \n")), "dev-sda");
    }

    // ── weight_tib_from ───────────────────────────────────────────────────────

    #[test]
    fn weight_prefers_cephs_own_kb_over_lsblk_bytes() {
        // 1 TiB expressed in KB; the byte figure is deliberately different so a
        // regression that reads the wrong argument shows up as a wrong weight.
        let kb = 1u64 << 30;
        assert_eq!(weight_tib_from(kb, 999), 1.0);
    }

    #[test]
    fn weight_falls_back_to_size_bytes_when_ceph_reports_nothing() {
        assert_eq!(weight_tib_from(0, 1u64 << 40), 1.0);
        assert_eq!(weight_tib_from(0, 1u64 << 39), 0.5);
    }

    #[test]
    fn weight_is_zero_when_no_size_is_known() {
        // A zero weight keeps a disk of unknown size from attracting data.
        assert_eq!(weight_tib_from(0, 0), 0.0);
    }

    // ── refuse_osd_creation ───────────────────────────────────────────────────
    //
    // The single most dangerous function in this file. `ceph-volume lvm create`
    // wipes the device it is given, and the creation loop calls it for any ON
    // disk missing from a map that is EMPTY whenever `ceph-volume lvm list`
    // fails. These tests exist so that failure mode can never become data loss.

    fn disk(is_our_osd: bool, foreign_ceph: bool) -> Value {
        json!({
            "device": "sdb",
            "size_bytes": 1_000_000_000u64,
            "is_our_osd": is_our_osd,
            "foreign_ceph": foreign_ceph,
        })
    }

    #[test]
    fn a_blank_disk_may_be_turned_into_an_osd() {
        assert_eq!(refuse_osd_creation(&disk(false, false)), None);
    }

    /// The whole point. A healthy OSD of ours that Ceph momentarily fails to
    /// report must never be re-created over — that is destroying live data in
    /// response to a transient command failure.
    #[test]
    fn our_own_osd_is_never_recreated_over() {
        assert!(
            refuse_osd_creation(&disk(true, false)).is_some(),
            "a disk carrying our own BlueStore label must never be handed to ceph-volume create"
        );
    }

    #[test]
    fn another_clusters_disk_is_refused() {
        assert!(refuse_osd_creation(&disk(false, true)).is_some());
    }

    /// Both flags set is contradictory, but if it ever happens the answer is
    /// still "do not wipe".
    #[test]
    fn a_contradictory_disk_is_refused() {
        assert!(refuse_osd_creation(&disk(true, true)).is_some());
    }

    /// Missing flags are the shape `meta` takes when the superblock could not be
    /// read at all. `unwrap_or(false)` makes both default to "blank", so this
    /// pins the one case that stays permissive — and proves the device check is
    /// what catches a genuinely empty entry.
    #[test]
    fn an_entry_with_no_flags_but_a_device_is_allowed() {
        assert_eq!(refuse_osd_creation(&json!({"device": "sdb"})), None);
    }

    #[test]
    fn an_entry_without_a_device_is_refused() {
        assert!(refuse_osd_creation(&json!({"is_our_osd": false})).is_some());
        assert!(refuse_osd_creation(&json!({"device": ""})).is_some());
    }

    // ── canonical_device ──────────────────────────────────────────────────────
    //
    // /dev holds several names for one LVM volume: /dev/mapper/pool-ceph,
    // /dev/pool/ceph and /dev/dm-1 are the same disk. Our inventory records one
    // spelling and ceph-volume reports another, so comparing the raw strings
    // made an existing OSD look like a blank disk — the reconciler then tried to
    // provision over it on every tick, and only refuse_osd_creation stopped that
    // becoming a wipe. Observed live on the system disk.

    #[test]
    fn canonical_device_leaves_an_unresolvable_path_alone() {
        // Must be lossless rather than empty: an empty key would collide with
        // every other unresolvable device and mismatch OSDs onto wrong disks.
        assert_eq!(
            canonical_device("/dev/definitely-not-here"),
            "/dev/definitely-not-here"
        );
    }

    #[test]
    fn canonical_device_expands_a_bare_kernel_name() {
        // lsblk gives "sdb"; ceph-volume gives "/dev/sdb". They must agree.
        assert_eq!(canonical_device("sdb"), canonical_device("/dev/sdb"));
    }

    /// The two spellings that actually collided in production.
    #[test]
    fn the_two_lvm_spellings_agree_via_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dm-1");
        std::fs::write(&real, b"x").unwrap();
        let alias = dir.path().join("pool-ceph");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert_eq!(
            canonical_device(alias.to_str().unwrap()),
            canonical_device(real.to_str().unwrap()),
            "a symlink and its target must resolve to the same key"
        );
    }

    // ── parse_lvm_list ────────────────────────────────────────────────────────

    /// The real shape of `ceph-volume lvm list --format json`: an object keyed
    /// by OSD id, each holding a list of LV records naming their backing
    /// devices.
    #[test]
    fn lvm_list_maps_devices_to_osd_ids() {
        let raw = r#"{
          "0": [{"devices": ["/dev/sdb"], "tags": {"ceph.osd_fsid": "abc"}}],
          "1": [{"devices": ["/dev/pool/ceph"], "tags": {"ceph.osd_fsid": "def"}}]
        }"#;
        let mut got = parse_lvm_list(raw).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("/dev/pool/ceph".to_string(), 1),
                ("/dev/sdb".to_string(), 0),
            ]
        );
    }

    /// ceph-volume prints `-->` progress lines to stdout when it cannot write
    /// its own logfile. A parse failure here returns an empty OSD map, which is
    /// precisely the state `refuse_osd_creation` exists to survive — so the
    /// preamble is stripped instead.
    #[test]
    fn lvm_list_tolerates_a_progress_preamble() {
        let raw = "--> Falling back to /tmp/ for logging\n{\"0\": [{\"devices\": [\"/dev/sdb\"]}]}";
        assert_eq!(parse_lvm_list(raw).unwrap(), vec![("/dev/sdb".to_string(), 0)]);
    }

    #[test]
    fn lvm_list_handles_an_osd_spanning_several_devices() {
        let raw = r#"{"3": [{"devices": ["/dev/sdc", "/dev/sdd"]}]}"#;
        assert_eq!(
            parse_lvm_list(raw).unwrap(),
            vec![("/dev/sdc".to_string(), 3), ("/dev/sdd".to_string(), 3)]
        );
    }

    #[test]
    fn lvm_list_reports_no_osds_for_an_empty_object() {
        assert!(parse_lvm_list("{}").unwrap().is_empty());
    }

    /// Must be an error, never `Ok(vec![])`. An empty result reads as "this host
    /// has no OSDs", which is the input that makes the reconciler start wiping.
    #[test]
    fn lvm_list_errors_on_output_with_no_json() {
        assert!(parse_lvm_list("").is_err());
        assert!(parse_lvm_list("--> something went wrong").is_err());
    }

    #[test]
    fn lvm_list_errors_on_malformed_json() {
        assert!(parse_lvm_list("{\"0\": [").is_err());
    }

    #[test]
    fn lvm_list_skips_entries_that_are_not_osd_ids() {
        let raw = r#"{"notanid": [{"devices": ["/dev/sdz"]}], "2": [{"devices": ["/dev/sde"]}]}"#;
        assert_eq!(parse_lvm_list(raw).unwrap(), vec![("/dev/sde".to_string(), 2)]);
    }

    #[test]
    fn lvm_list_skips_records_with_no_devices() {
        let raw = r#"{"0": [{"tags": {}}], "1": [{"devices": [""]}]}"#;
        assert!(parse_lvm_list(raw).unwrap().is_empty());
    }

}
