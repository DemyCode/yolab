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
use std::{collections::HashMap, io::Read, path::Path};
use tokio::time::{sleep, Duration};

use crate::host::{Host, RealHost};
use crate::kubectl;

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

// ── Per-disk progress ─────────────────────────────────────────────────────────
//
// The toggle is a promise: ON drives a disk all the way into the pool, OFF
// drives it all the way to safely unpluggable, and both keep trying. What was
// missing was any way to SAY where a disk is on that journey.
//
// `DiskInfo` carried only desired/connected/is_our_osd/osd_id, so the UI derived
// "Setting up…" from "switched on, present, no OSD yet" — a description that is
// identical whether the create started five seconds ago or has failed fourteen
// times. Every failure lived in a `tracing::warn!` nobody reads.
//
// This is deliberately in memory rather than persisted. It describes what the
// reconciler is doing right now, and every value in it is re-derived from the
// cluster within one tick of a restart. The one field that would hurt to lose is
// `orphan_osd_id`, and losing it costs one leaked id rather than corrupting
// anything — see `spawn_create`.

/// Where a disk is between "plugged in" and the state its toggle asks for.
///
/// These strings cross the API to the UI. They are the vocabulary the Storage
/// page speaks, so they name situations a person can act on, not internal steps.
pub mod phase {
    /// ON, OSD exists, daemon running, carrying data. The destination.
    pub const ACTIVE: &str = "active";
    /// ON, a create is running right now.
    pub const CREATING: &str = "creating";
    /// ON, the last attempt failed; another is coming. `message` says why.
    pub const RETRYING: &str = "retrying";
    /// ON, but something must be decided by a person before it can proceed —
    /// foreign Ceph data, an existing filesystem. Retrying will not help.
    pub const BLOCKED: &str = "blocked";
    /// OFF, data still moving off it. Cannot be unplugged yet.
    pub const DRAINING: &str = "draining";
    /// OFF, drained; the OSD is being purged and the disk wiped.
    pub const REMOVING: &str = "removing";
    /// OFF and finished. Safe to physically unplug. The destination for OFF.
    pub const REMOVABLE: &str = "removable";
    /// Ceph could not be reached, so nothing here is known. Never treated as
    /// "nothing exists" — see `reconcile_local_osds`.
    pub const UNKNOWN: &str = "unknown";
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DiskProgress {
    pub phase: String,
    /// Shown verbatim to the user, so it says what happened and what follows.
    pub message: String,
    pub attempts: u32,
    /// An OSD id this node allocated for a create that never finished.
    ///
    /// `ceph-volume lvm create` takes an id from the mon before it zaps, builds
    /// the LVM stack or mkfs's BlueStore. A create that times out therefore
    /// leaves an id in the osdmap with no CRUSH location and nothing on disk —
    /// and the next attempt, seeing no OSD for the disk, allocated *another*.
    /// One node reached osd.1 with a blank host that way, and nothing ever
    /// cleaned it up.
    ///
    /// Holding the id here means the retry reuses it instead of leaking a new
    /// one. Only ever an id this process watched appear.
    pub orphan_osd_id: Option<i64>,
    /// When the last create attempt was spawned. `Instant`, not a wall-clock
    /// time: this only ever compares against `Instant::now()` on the same
    /// process, and never survives a restart — which is fine, since a
    /// restarted local-api starting the backoff over is a smaller cost than
    /// the retry-forever bug this exists to fix.
    pub last_attempt: Option<std::time::Instant>,
}

static PROGRESS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, DiskProgress>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Disks with a create running off-tick. Prevents a second one being started
/// for the same disk while the first is still going.
static CREATING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// What a device's own BlueStore label says about who owns it.
///
/// Replaces a pair of independent booleans (`is_our_osd`, `foreign_ceph`) that
/// could encode states no device can be in, and that were read back out of
/// untyped JSON with `as_bool().unwrap_or(false)` — so a renamed or absent key
/// silently meant "not ours, not foreign", i.e. safe to wipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// Labelled with this cluster's fsid.
    Ours,
    /// Labelled with a different cluster's fsid.
    Foreign,
    /// No BlueStore label at offset 0.
    Blank,
    /// Labelled, but our own fsid could not be read, so it cannot be
    /// attributed. Distinct from `Foreign` in meaning and identical to it in
    /// consequence: both refuse creation.
    Unknown,
}

impl Ownership {
    fn read(device_fsid: Option<&str>, our_fsid: &str) -> Self {
        match device_fsid {
            None => Ownership::Blank,
            Some(_) if our_fsid.is_empty() => Ownership::Unknown,
            Some(f) if f == our_fsid => Ownership::Ours,
            Some(_) => Ownership::Foreign,
        }
    }

    fn is_ours(self) -> bool {
        self == Ownership::Ours
    }

    /// True for anything carrying a label this cluster cannot claim. Drives
    /// both the wire field and the refusal, so they cannot disagree.
    fn is_foreign(self) -> bool {
        matches!(self, Ownership::Foreign | Ownership::Unknown)
    }
}

/// One device as this node sees it.
///
/// The published shape is produced in exactly one place (`to_value`) so the
/// ConfigMap the UI reads cannot drift field by field.
#[derive(Clone, Debug)]
pub(crate) struct Disk {
    pub device: String,
    pub model: String,
    pub size_bytes: u64,
    /// The UI renders this as the built-in "System disk".
    pub is_loop: bool,
    pub ownership: Ownership,
    pub has_partitions: bool,
    pub mounted: bool,
    pub osd_id: Option<i64>,
    pub progress: Option<DiskProgress>,
}

impl Disk {
    fn to_value(&self) -> Value {
        let mut v = json!({
            "device": self.device,
            "model": self.model,
            "size_bytes": self.size_bytes,
            "is_our_osd": self.ownership.is_ours(),
            "foreign_ceph": self.ownership.is_foreign(),
            "has_partitions": self.has_partitions,
            "mounted": self.mounted,
            "osd_id": self.osd_id,
        });
        // Only the system disk carried this key before, and the UI branches on
        // its presence, so it stays absent rather than present-and-false.
        if self.is_loop {
            v["is_loop"] = json!(true);
        }
        if let Some(p) = self.progress.as_ref().filter(|p| !p.phase.is_empty()) {
            v["phase"] = json!(p.phase);
            v["message"] = json!(p.message);
            v["attempts"] = json!(p.attempts);
        }
        v
    }

    /// Full device path, as the ceph tools want it.
    fn dev_path(&self) -> Option<String> {
        let d = self.device.trim();
        if d.is_empty() {
            return None;
        }
        Some(if d.starts_with('/') {
            d.to_string()
        } else {
            format!("/dev/{d}")
        })
    }
}

fn set_phase(disk_id: &str, phase: &str, message: impl Into<String>) {
    let Ok(mut p) = PROGRESS.lock() else { return };
    let e = p.entry(disk_id.to_string()).or_default();
    e.phase = phase.to_string();
    e.message = message.into();
}

fn progress_of(disk_id: &str) -> DiskProgress {
    PROGRESS
        .lock()
        .ok()
        .and_then(|p| p.get(disk_id).cloned())
        .unwrap_or_default()
}

/// Merge each disk's progress into the metadata published to the UI.
fn mark_progress(meta: &mut HashMap<String, Disk>) {
    for (disk_id, d) in meta.iter_mut() {
        d.progress = Some(progress_of(disk_id));
    }
}

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
fn system_osd_meta(our_fsid: &str) -> Disk {
    Disk {
        device: SYSTEM_OSD_DEV.to_string(),
        model: "System disk".to_string(),
        size_bytes: system_osd_size_bytes(),
        is_loop: true,
        ownership: Ownership::read(bluestore_fsid(SYSTEM_OSD_DEV).as_deref(), our_fsid),
        // Never looked up: a dedicated LVM volume disko carves out for Ceph at
        // install. It has no partition table of its own and is never mounted —
        // the OS lives on a sibling volume. Reporting either would make
        // refuse_osd_creation block the one disk meant to be on by default.
        has_partitions: false,
        mounted: false,
        osd_id: None,
        progress: None,
    }
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

        let lease_json =
            crate::kubectl::get_json(&["get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json"])
                .await;

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
                let dur = spec["leaseDurationSeconds"]
                    .as_i64()
                    .unwrap_or(LEASE_SECS as i64);
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
                    spec["acquireTime"]
                        .as_str()
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
        let Ok(v) =
            crate::kubectl::get_json(&["get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json"])
                .await
        else {
            return false;
        };
        let spec = &v["spec"];
        let holder = spec["holderIdentity"].as_str().unwrap_or("");
        let dur = spec["leaseDurationSeconds"]
            .as_i64()
            .unwrap_or(LEASE_SECS as i64);
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
    let host = RealHost;
    loop {
        // Every node: publish its own disk inventory + run its own OSD lifecycle.
        if let Err(e) = publish_local(&host, &node).await {
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
async fn fetch_disk_to_osd<H: Host>(
    host: &H,
    _node: &str,
    meta: &HashMap<String, Disk>,
) -> Option<HashMap<String, i64>> {
    // Build full device path → disk_id from our local inventory.
    // Index both the stored path and its canonical (symlink-resolved) path so
    // that /dev/mapper/pool-ceph (a symlink → /dev/dm-1) matches whichever
    // path Ceph actually opened and reports in bluestore_bdev_dev_node.
    let mut device_to_disk_id: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        let dev = m.device.as_str();
        if dev.is_empty() {
            continue;
        }
        device_to_disk_id.insert(canonical_device(dev), disk_id.clone());
    }

    // None, never an empty map. The difference is the whole safety property
    // here: an empty map means "this host has no OSDs", which makes every
    // switched-on disk look like it needs creating — and `ceph-volume lvm
    // create` wipes what it is pointed at.
    //
    // `refuse_osd_creation` is not a sufficient backstop, because it reads a
    // BlueStore label from offset 0 and an OSD that ceph-volume wrapped in LVM
    // has no label there (see `mark_known_osds`). So a timeout here used to put
    // the reconciler one weak check away from re-creating over a live OSD. It
    // happened for real: `fetch_disk_to_osd: ceph-volume timed out after 600s`,
    // repeatedly, on a node whose disks were switched on.
    // ceph-volume first: it reads LVM tags on this host, so it is right even
    // when the cluster is unreachable. When it fails, fall back to the mon,
    // which keeps each OSD's metadata even while that OSD is down. The two have
    // unrelated failure modes, and needing both is not hypothetical — a wedged
    // `lvs` took ceph-volume out on node1 and left an OSD marked `out` with no
    // way back in, because marking it in needs this map.
    let local = match local_osds(host).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("fetch_disk_to_osd: ceph-volume failed ({e}) — asking the mon instead");
            match host.ceph_json(&["osd", "metadata"]).await {
                Ok(v) => {
                    let from_mon = parse_osd_metadata(&v, _node);
                    if from_mon.is_empty() {
                        // Genuinely no OSDs on this host is indistinguishable
                        // here from metadata we could not interpret, and one of
                        // those two is safe to act on while the other is not.
                        tracing::warn!(
                            "fetch_disk_to_osd: the mon reported no OSDs for this host either — \
                             treating the map as UNKNOWN, not empty"
                        );
                        return None;
                    }
                    tracing::info!(
                        "fetch_disk_to_osd: recovered {} OSD(s) from the mon",
                        from_mon.len()
                    );
                    from_mon
                }
                Err(e2) => {
                    tracing::warn!(
                        "fetch_disk_to_osd: the mon could not answer either ({e2}) — treating the \
                         OSD map as UNKNOWN, not empty"
                    );
                    return None;
                }
            }
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
    Some(result)
}

/// Parse `ceph osd metadata -f json` into (device identity, osd id) pairs for
/// one host.
///
/// The SECOND source for the disk→OSD map, and the reason there is a second one
/// at all: the first (`ceph-volume lvm list`) reads LVM tags on this machine, so
/// it works with no mon — but it dies with LVM. When `lvs` wedged on node1,
/// ceph-volume stopped answering, the map became unknown, and the reconciler
/// correctly refused to touch anything. Correct, and also stuck: an OSD that was
/// already marked `out` could not be marked back `in`, because deciding that
/// needs the very map that was unavailable.
///
/// This one comes from the mon instead, and the mon keeps an OSD's metadata even
/// while that OSD is down — which is exactly the case that matters. Two sources
/// with unrelated failure modes means a wedged LVM no longer freezes recovery.
///
/// Both identities are collected for the same reason `parse_lvm_list` collects
/// both: `devices` names the physical disk under an LVM OSD, while
/// `bluestore_bdev_dev_node` names the volume itself, and our inventory holds
/// one or the other depending on the disk.
pub(crate) fn parse_osd_metadata(raw: &Value, host: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Some(items) = raw.as_array() else {
        return out;
    };
    for m in items {
        if m["hostname"].as_str() != Some(host) {
            continue;
        }
        let Some(id) = m["id"].as_i64() else { continue };
        if let Some(node) = m["bluestore_bdev_dev_node"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "unknown")
        {
            out.push((node.to_string(), id));
        }
        // `devices` is a comma-separated list of bare kernel names.
        if let Some(devs) = m["devices"].as_str() {
            for d in devs.split(',').map(str::trim).filter(|d| !d.is_empty()) {
                out.push((d.to_string(), id));
            }
        }
    }
    out
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

async fn local_osds<H: Host>(host: &H) -> Result<Vec<(String, i64)>> {
    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await?;
    let our_fsid = host.cluster_fsid().await.unwrap_or_default();
    parse_lvm_list(&raw, &our_fsid)
}

/// Per-node: publish this node's disk inventory + its effective device list to
/// the status ConfigMap, and run this node's own OSD lifecycle. No shared-CR
/// writes happen here, so every node can run it concurrently without racing.
async fn publish_local<H: Host + 'static>(host: &H, node: &str) -> Result<()> {
    let Some(scanned) = scan_devices(host).await else {
        anyhow::bail!("could not read the disk list from lsblk — skipping this tick");
    };
    let our_fsid = host.cluster_fsid().await.unwrap_or_default();

    let mut meta: HashMap<String, Disk> = scanned
        .iter()
        .map(|(d, flags)| (disk_id(d), disk_meta(d, &our_fsid, *flags)))
        .collect();
    if system_osd_present() {
        meta.insert(SYSTEM_OSD_ID.to_string(), system_osd_meta(&our_fsid));
    }

    // Use Ceph's own metadata as the authoritative disk→OSD mapping — no
    // bluestore header parsing, no size heuristics, no deployment env scraping.
    // None means "could not tell", which is NOT the same as "no OSDs" and must
    // never be flattened into one. An empty fsid means the cluster is
    // unreachable, which is equally unknown.
    let disk_to_osd: Option<HashMap<String, i64>> = if our_fsid.is_empty() {
        None
    } else {
        fetch_disk_to_osd(host, node, &meta).await
    };
    if let Some(map) = &disk_to_osd {
        mark_known_osds(&mut meta, map);
    }

    // Publish the inventory whatever happens: the Storage page has to keep
    // working when Kubernetes does not, and this is the only source it has.
    let desired = read_desired(host).await;

    match &desired {
        Some(d) => {
            // Register first, so a disk that has just been plugged in is
            // reconciled on this tick rather than reported as "not in use" for
            // 30 seconds before its real state is known.
            auto_register_all_disks(host, node, &meta, d).await;
            let d = read_desired(host).await.unwrap_or_else(|| d.clone());
            reconcile_local_osds(host, node, &meta, &d, disk_to_osd.as_ref()).await;
        }
        None => {
            tracing::warn!(
                "disk reconciler: cannot read which disks are switched on — making no changes"
            );
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    phase::UNKNOWN,
                    "Cannot reach this machine's settings right now. Nothing will be changed until it can.",
                );
            }
        }
    }

    mark_progress(&mut meta);
    write_status(host, node, &meta).await;
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
/// What to tell someone whose disk is draining.
///
/// The honest answer depends on whether the drain can finish at all. Ceph moves
/// a disk's data onto the others before it will call it safe to remove, so with
/// only one OSD left there is nowhere for that data to go — `osd out` never
/// reaches safe-to-destroy and the disk drains forever. That is the correct
/// behaviour (the alternative is discarding data), but sitting on "Being
/// removed" with no explanation is not.
///
/// Pure, so the wording is pinned by tests rather than discovered by someone
/// watching a progress bar that will never move.
/// Places Ceph could still put a copy if this OSD went away.
///
/// With `failure_domain=osd` that is every other OSD which is up and in; with
/// `host` it is every other MACHINE holding one. Pure, because the arithmetic
/// it feeds decides whether a drain can ever finish and the answer is not
/// observable from a running cluster until it is too late.
pub(crate) fn drain_targets_remaining(
    crush_nodes: &[Value],
    leaving: i64,
    failure_domain: &str,
) -> usize {
    // Which OSDs could accept data: up, in, and not the one being emptied.
    let usable: Vec<i64> = crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd"))
        .filter(|n| n["status"].as_str() == Some("up"))
        .filter(|n| n["reweight"].as_f64().unwrap_or(0.0) > 0.5)
        .filter_map(|n| n["id"].as_i64())
        .filter(|id| *id != leaving)
        .collect();

    if failure_domain != "host" {
        return usable.len();
    }

    // Host domain: copies must land on distinct machines, so what counts is how
    // many machines still hold a usable OSD — not how many OSDs there are.
    crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("host"))
        .filter(|h| {
            h["children"].as_array().is_some_and(|c| {
                c.iter()
                    .filter_map(|x| x.as_i64())
                    .any(|id| usable.contains(&id))
            })
        })
        .count()
}

/// What to tell someone whose disk is being emptied.
///
/// THE CASE THIS EXISTS FOR: a drain that can never finish.
///
/// Keeping `size` copies needs `size` distinct places to put them. Marking a
/// disk out leaves fewer places, and when that drops below `size` CRUSH cannot
/// build a valid set — so it keeps the outgoing OSD in the acting set, its PGs
/// stay `remapped`, `safe-to-destroy` answers EBUSY forever, and the disk never
/// leaves.
///
/// Seen exactly this way: three disks, three copies, one switched off. Ceph
/// reported `49 active+clean+remapped`, 33% of objects misplaced, no backfill
/// running, and `Error EBUSY: OSD(s) 1 have 49 pgs currently mapped to them` —
/// while this function cheerfully said "do not unplug it until this finishes".
/// It was never going to finish, and the only thing that ends it is a decision
/// the owner has to make.
///
/// `size` is None when the policy could not be read; the message then promises
/// nothing it cannot check.
fn drain_message(targets: usize, size: Option<u32>) -> String {
    let Some(size) = size else {
        return "Moving this disk's files onto the others. Do not unplug it while this \
                is happening."
            .to_string();
    };

    if targets == 0 {
        return "Waiting to move this disk's files somewhere else — but there is no other \
                disk running to move them to. Switch on another disk, or add one, and this \
                will finish on its own."
            .to_string();
    }

    if targets < size as usize {
        return format!(
            "This disk cannot be emptied yet. You have asked for {size} copies of \
             everything, and taking this disk out leaves only {targets} other \
             place{plural} to keep them — so there is nowhere for its files to go. \
             Lower the number of copies to {targets}, or add another disk, and this \
             finishes on its own.",
            plural = if targets == 1 { "" } else { "s" }
        );
    }

    "Moving this disk's files onto the others. Do not unplug it until this finishes.".to_string()
}

/// The steering one OSD needs this tick, computed from its observed state and
/// the owner's intent. Pure so every branch is pinned by a test rather than by
/// watching a cluster.
#[derive(Debug, PartialEq)]
struct Steer {
    set_weight: Option<f64>,
    mark_in: bool,
    mark_out: bool,
    needs_purge: bool,
    phase: Option<&'static str>,
    message: Option<String>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
struct OsdState {
    crush_weight: f64,
    reweight: f64,
    kb: u64,
    up: bool,
}

fn decide_steer(
    want_on: bool,
    size_bytes: u64,
    state: OsdState,
    osd_id: i64,
    crush_nodes: &[Value],
    failure_domain: &str,
    want_copies: Option<u32>,
) -> Steer {
    if want_on {
        let set_weight = if state.crush_weight == 0.0 {
            let weight = weight_tib_from(state.kb, size_bytes);
            (weight > 0.0).then_some(weight)
        } else {
            None
        };
        let (phase, message) = if state.up {
            (phase::ACTIVE, "In use, storing your files.".to_string())
        } else {
            (
                phase::RETRYING,
                "This disk is switched on but is not currently serving data. \
                 YoLab keeps trying to bring it back."
                    .to_string(),
            )
        };
        return Steer {
            set_weight,
            mark_in: state.reweight < 0.5,
            mark_out: false,
            needs_purge: false,
            phase: Some(phase),
            message: Some(message),
        };
    }

    if state.reweight > 0.5 {
        let targets = drain_targets_remaining(crush_nodes, osd_id, failure_domain);
        return Steer {
            set_weight: None,
            mark_in: false,
            mark_out: true,
            needs_purge: false,
            phase: Some(phase::DRAINING),
            message: Some(drain_message(targets, want_copies)),
        };
    }

    Steer {
        set_weight: None,
        mark_in: false,
        mark_out: false,
        needs_purge: true,
        phase: None,
        message: None,
    }
}

/// The phase to report after steering. When the effects failed, the optimistic
/// ACTIVE/DRAINING answer would hide that the OSD never moved, so it becomes
/// RETRYING instead.
fn steer_report(steer: &Steer, applied: bool) -> (&'static str, String) {
    if applied {
        (steer.phase.unwrap(), steer.message.clone().unwrap())
    } else {
        (
            phase::RETRYING,
            "YoLab could not finish this step and will keep trying.".to_string(),
        )
    }
}

async fn reconcile_local_osds<H: Host + 'static>(
    host: &H,
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
    disk_to_osd: Option<&HashMap<String, i64>>,
) {
    // The two guards that stand between a bad answer and `ceph-volume lvm
    // create` over live data. See `plan_tick`, where they are asserted.
    match plan_tick(host.reachable().await, disk_to_osd.is_some()) {
        TickPlan::Unreachable => {
            tracing::debug!("reconcile_local_osds: ceph unreachable, skipping this tick");
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    phase::UNKNOWN,
                    "Waiting for the storage cluster to answer.",
                );
            }
            return;
        }
        TickPlan::UnknownOsdMap => {
            tracing::warn!(
                "reconcile_local_osds: the local OSD map is unknown this tick — making no changes"
            );
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    phase::UNKNOWN,
                    "Cannot read this machine's disk setup right now. Nothing will be changed until it can.",
                );
            }
            return;
        }
        TickPlan::Proceed => {}
    }
    let Some(disk_to_osd) = disk_to_osd else {
        return;
    };

    // Create OSDs for disks switched ON that do not have one yet. This is the
    // half Rook used to do in response to a CephCluster patch.
    for (disk_id, m) in meta {
        let creating = CREATING
            .lock()
            .map(|c| c.contains(disk_id))
            .unwrap_or(false);
        let (attempts, since_last) = PROGRESS
            .lock()
            .ok()
            .and_then(|p| p.get(disk_id).cloned())
            .map(|p| (p.attempts, p.last_attempt.map(|t| t.elapsed())))
            .unwrap_or((0, None));
        match plan_create(
            node,
            disk_id,
            m,
            desired,
            disk_to_osd,
            creating,
            attempts,
            since_last,
        ) {
            CreatePlan::Skip => continue,
            CreatePlan::Waiting => continue,
            CreatePlan::Blocked(reason) => {
                tracing::warn!("{disk_id}: desired ON but not creating an OSD — {reason}");
                set_phase(disk_id, phase::BLOCKED, reason);
            }
            CreatePlan::Create { dev_path } => {
                spawn_create(host.clone(), disk_id.clone(), dev_path)
            }
        }
    }

    // ── Bring back anything that is simply not running ───────────────────────
    //
    // FIRST, and deliberately before anything that needs cluster statistics.
    //
    // This used to live inside the loop below, which runs after `ceph osd df
    // tree`. That command reports usage and needs the mgr, so it fails on a
    // degraded cluster — and the failure was a bare `return`. The single most
    // important recovery action in this system was therefore gated behind a
    // STATISTICS query that breaks exactly when recovery is needed.
    //
    // It happened: a nixos-rebuild stopped osd.2 on node3 ("Deactivated
    // successfully" — a clean stop, so Restart=on-failure does not apply), the
    // cluster then had no OSDs up, `osd df tree` stopped answering, and the
    // reconciler returned before reaching the line that would have started the
    // daemon again. A self-healing system sat there not healing.
    //
    // Starting a stopped daemon needs no cluster state at all. It must not
    // depend on the cluster being healthy enough to describe itself.
    for disk_id in meta.keys() {
        let want_on = wants_on(desired, node, disk_id);
        if !want_on {
            continue;
        }
        if let Some(&osd_id) = disk_to_osd.get(disk_id) {
            ensure_osd_unit_running(host, osd_id).await;
        }
    }

    stop_foreign_osd_units(host).await;

    let crush_nodes: Vec<Value> = match host.ceph_json(&["osd", "df", "tree"]).await {
        Ok(v) => v["nodes"].as_array().cloned().unwrap_or_default(),
        Err(e) => {
            // Not silent any more. This return skips weighting, in/out and the
            // whole OFF path, so it has to be visible when it happens.
            tracing::warn!(
                "reconcile: `ceph osd df tree` did not answer ({e}) — daemons were started, but \
                 nothing else can be decided this tick"
            );
            return;
        }
    };

    // How many copies the owner asked for, and where they must go. Needed to
    // tell a drain that is progressing from one that is deadlocked — see
    // `drain_message`. Read once per tick, not per disk.
    let (want_copies, failure_domain) = match crate::topology::read_policy().await {
        Some(crate::topology::PolicyState::Chosen(p)) => (Some(p.size), p.failure_domain),
        _ => (None, "osd".to_string()),
    };

    let osd_state_up: std::collections::HashSet<i64> = crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd") && n["status"].as_str() == Some("up"))
        .filter_map(|n| n["id"].as_i64())
        .collect();

    let mut osd_state: HashMap<i64, (f64, f64, u64)> = HashMap::new();
    for n in &crush_nodes {
        if n["type"].as_str() != Some("osd") {
            continue;
        }
        if let Some(id) = n["id"].as_i64() {
            osd_state.insert(
                id,
                (
                    n["crush_weight"].as_f64().unwrap_or(0.0),
                    n["reweight"].as_f64().unwrap_or(1.0),
                    n["kb"].as_u64().unwrap_or(0),
                ),
            );
        }
    }

    for (disk_id, m) in meta {
        let want_on = wants_on(desired, node, disk_id);
        let Some(&osd_id) = disk_to_osd.get(disk_id) else {
            // No OSD on this disk. If it is switched OFF that is the finished
            // state, not a missing one — say so, because "you can unplug this
            // now" is the whole point of the OFF toggle and nothing used to
            // report it.
            if !want_on {
                set_phase(disk_id, phase::REMOVABLE, "Not in use. Safe to unplug.");
            }
            continue;
        };
        let (crush_weight, reweight, kb) = osd_state.get(&osd_id).copied().unwrap_or((0.0, 1.0, 0));
        let osd = format!("osd.{osd_id}");

        let state = OsdState {
            crush_weight,
            reweight,
            kb,
            up: osd_state_up.contains(&osd_id),
        };
        let steer = decide_steer(
            want_on,
            m.size_bytes,
            state,
            osd_id,
            &crush_nodes,
            &failure_domain,
            want_copies,
        );

        let mut applied = true;
        if let Some(weight) = steer.set_weight {
            tracing::info!("{osd} ({disk_id}): crush_weight=0 — setting weight={weight:.5}");
            if host
                .ceph(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")])
                .await
                .is_err()
            {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not set its weight");
            }
        }
        if steer.mark_in {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=ON — marking in");
            if host.ceph(&["osd", "in", &osd]).await.is_err() {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not mark it in");
            }
        }
        if steer.mark_out {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=OFF — marking out");
            if host.ceph(&["osd", "out", &osd]).await.is_err() {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not mark it out");
            }
        }

        if steer.needs_purge {
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
            // Asked before the daemon is stopped, again after, and combined
            // with the cluster's data-loss state. See `plan_purge`.
            let safe_before = host.osd_safe_to_destroy(osd_id).await;
            if safe_before {
                set_phase(
                    disk_id,
                    phase::REMOVING,
                    "Finishing up — do not unplug yet.",
                );
                // Stop the daemon before purging. Purging while it still runs
                // is the EBUSY race the old code guarded against separately.
                disable_osd_unit(host, osd_id).await;
            }
            let safe_after = safe_before && host.osd_safe_to_destroy(osd_id).await;
            let loss = if safe_after {
                crate::routers::ceph::assess_pg_loss_via(host).await
            } else {
                None
            };

            match plan_purge(safe_before, safe_after, loss.as_ref()) {
                PurgeVerdict::Wait => {
                    tracing::debug!("{osd} ({disk_id}): out but not yet safe-to-destroy — waiting");
                    let targets = drain_targets_remaining(&crush_nodes, osd_id, &failure_domain);
                    set_phase(
                        disk_id,
                        phase::DRAINING,
                        drain_message(targets, want_copies),
                    );
                    continue;
                }
                PurgeVerdict::Recheck => {
                    tracing::warn!("{osd} ({disk_id}): stopped, but no longer reports safe-to-destroy — leaving it alone");
                    continue;
                }
                PurgeVerdict::RefuseDataAtRisk => {
                    let l = loss.as_ref().expect("a refusal names the loss it saw");
                    tracing::warn!(
                        "{osd} ({disk_id}): NOT purging — {} of {} placement groups are unreadable \
                         and cannot be rebuilt, and this disk may hold the only copy",
                        l.stuck,
                        l.total
                    );
                    set_phase(
                        disk_id,
                        phase::REMOVING,
                        "Not removing this disk: data elsewhere in the cluster is unreadable and \
                         cannot be rebuilt, and this disk may hold the only copy of it.",
                    );
                    continue;
                }
                PurgeVerdict::Purge => {}
            }

            match host
                .ceph(&["osd", "purge", &osd, "--yes-i-really-mean-it"])
                .await
            {
                Ok(_) => {
                    let ids = host.osd_ids().await.ok();
                    if purge_confirmed(ids.as_deref(), osd_id) {
                        tracing::info!("{osd} ({disk_id}): purged");
                        if let Some(device) = Some(m.device.as_str()).filter(|d| !d.is_empty()) {
                            tracing::info!("{osd} ({disk_id}): wiping BlueStore label on {device}");
                            wipe_device(host, device).await;
                        }
                        set_phase(
                            disk_id,
                            phase::REMOVABLE,
                            "Removed from the pool. Safe to unplug.",
                        );
                    } else {
                        tracing::warn!(
                            "{osd} ({disk_id}): purge reported success but {osd} is still listed — leaving the disk alone"
                        );
                        set_phase(
                            disk_id,
                            phase::REMOVING,
                            "Still finishing up — do not unplug this disk yet.",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("{osd} ({disk_id}): purge failed: {e}");
                    set_phase(
                        disk_id,
                        phase::REMOVING,
                        "Still finishing up — do not unplug this disk yet.",
                    );
                }
            }
        } else {
            let (phase, message) = steer_report(&steer, applied);
            set_phase(disk_id, phase, message);
        }
    }

    let unplugged_but_wanted = any_unplugged_but_wanted(desired, node, meta);

    purge_drained_osds(host, node, &crush_nodes, disk_to_osd, unplugged_but_wanted).await;
}

/// Is any disk on this node switched ON but not physically here? If so, a
/// leftover OSD whose disk vanished could belong to an unplugged disk, so none
/// of them may be purged. Switching that disk OFF clears the block.
fn any_unplugged_but_wanted(
    desired: &HashMap<String, String>,
    node: &str,
    meta: &HashMap<String, Disk>,
) -> bool {
    let prefix = format!("{node}--");
    desired.iter().any(|(k, v)| {
        if v != "ON" && v != "USING" {
            return false;
        }
        match k.strip_prefix(&prefix) {
            Some(d) => !meta.contains_key(d),
            None if is_globally_unique_id(k) => !meta.contains_key(k.as_str()),
            None => false,
        }
    })
}

/// Run `ceph-volume lvm create` off the reconcile tick.
///
/// It used to be awaited inline, and ceph-volume's timeout is ten minutes. One
/// unresponsive device therefore froze every disk on the node: no inventory, no
/// status, no other disk reconciled, for the whole ten minutes. The log showed
/// ticks 10 and 21 minutes apart against a 30s interval.
///
/// Off-tick, the loop keeps running at 30s and reports what this create is
/// doing while it does it. `CREATING` is what stops a second one being started
/// for the same disk on the next pass.
fn spawn_create<H: Host + 'static>(host: H, disk_id: String, dev_path: String) {
    {
        let Ok(mut running) = CREATING.lock() else {
            return;
        };
        // insert() returns false when it was already there — a create for this
        // disk is still going, so leave it alone.
        if !running.insert(disk_id.clone()) {
            return;
        }
    }

    let attempt = {
        let mut n = 1;
        if let Ok(mut p) = PROGRESS.lock() {
            let e = p.entry(disk_id.clone()).or_default();
            e.attempts += 1;
            e.last_attempt = Some(std::time::Instant::now());
            n = e.attempts;
        }
        n
    };
    set_phase(
        &disk_id,
        phase::CREATING,
        if attempt == 1 {
            "Setting this disk up for storage…".to_string()
        } else {
            // The count, not the device path: "/dev/sdb" means nothing to the
            // person who plugged the disk in, but "this has been tried a few
            // times" tells them something is wrong.
            format!("Still setting this disk up… (attempt {attempt})")
        },
    );
    tracing::info!(
        "{disk_id} ({dev_path}): switched ON with no OSD — creating (attempt {attempt})"
    );

    tokio::spawn(async move {
        create_osd(&host, &disk_id, &dev_path).await;
        if let Ok(mut c) = CREATING.lock() {
            c.remove(&disk_id);
        }
    });
}

fn is_stale_signature(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("bluestore signature") || e.contains("has a filesystem signature")
}

/// The id of an OSD from another cluster sitting on this device, if any.
///
/// ceph-volume reports it because the LVM tags are on this host; the label
/// sniff in `disk_meta` does not, because on a disk ceph-volume wrapped in LVM
/// the BlueStore label lives inside the LV rather than at offset 0. So a disk
/// like this reads as blank, passes `refuse_osd_creation`, and then fails
/// creation forever against its own leftover volume group.
async fn foreign_osd_on<H: Host>(host: &H, dev_path: &str) -> Option<i64> {
    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await
        .ok()?;
    let our_fsid = host.cluster_fsid().await.filter(|f| !f.is_empty())?;
    foreign_osd_in_list(&raw, &our_fsid, dev_path)
}

/// One creation attempt, including cleaning up after the previous one.
async fn create_osd<H: Host>(host: &H, disk_id: &str, dev_path: &str) {
    // Clear the wreckage of an earlier attempt first, so ids stop accumulating.
    reclaim_orphan(host, disk_id).await;

    // ceph-volume takes an id from the mon before it zaps the device, builds the
    // LVM stack or mkfs's BlueStore. Snapshotting ids around the call is what
    // lets a failure name the id it left behind — there is no other way to know
    // it, because ceph-volume prints nothing usable when it is killed.
    // Option, never a default. An empty "before" makes every OSD that already
    // exists look newly allocated, and the first of them would then be recorded
    // as this disk's orphan and offered to `reclaim_orphan` — which purges. On a
    // cluster whose data happens to have moved, that purges a live OSD. If the
    // snapshot fails, the leak simply goes undetected this round.
    let before = host.osd_ids().await.ok();

    // Before creating, not after a failure. A disk still carrying an earlier
    // install's LVM stack does not make ceph-volume fail — it exits 0 in about
    // two seconds having done nothing, because as far as it is concerned the
    // device is already an OSD. Nothing is created, the disk never joins any
    // map, and the reconciler tries again every tick forever. Erasing only on
    // Err never fired here.
    if let Some(id) = foreign_osd_on(host, dev_path).await {
        tracing::warn!(
            "{disk_id}: {dev_path} still holds osd.{id} from another cluster — erasing it first"
        );
        if let Err(e) = host
            .ceph_volume(&["lvm", "zap", "--destroy", dev_path])
            .await
        {
            tracing::warn!("{disk_id}: zap failed, leaving the disk alone: {e}");
            set_phase(
                disk_id,
                phase::RETRYING,
                "Could not erase this disk yet. YoLab will keep trying.",
            );
            return;
        }
    }

    let mut result = host
        .ceph_volume(&[
            "lvm",
            "create",
            "--bluestore",
            "--data",
            dev_path,
            "--no-systemd",
        ])
        .await;

    // The system LV keeps the previous cluster's BlueStore signature, because
    // disko recreates the LV but LVM does not zero reused extents. Unlike the
    // foreign stack above this one does fail, so it is cleared on the way out.
    // No --destroy: that volume is disko's to own, only its contents are stale.
    if result.is_err() {
        let err = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        if let Some(args) = zap_args(dev_path, false, &err) {
            tracing::warn!(
                "{disk_id}: stale BlueStore signature on {dev_path} — zapping and retrying"
            );
            if let Err(e) = host.ceph_volume(&args).await {
                tracing::warn!("{disk_id}: zap failed: {e}");
            } else {
                result = host
                    .ceph_volume(&[
                        "lvm",
                        "create",
                        "--bluestore",
                        "--data",
                        dev_path,
                        "--no-systemd",
                    ])
                    .await;
            }
        }
    }

    match result {
        Ok(_) => {
            // Re-read the mapping so we learn the id Ceph just assigned; it
            // cannot be known before creation.
            match local_osds(host).await {
                Ok(local) => {
                    let want = canonical_device(dev_path);
                    if let Some((_, osd_id)) =
                        local.iter().find(|(d, _)| canonical_device(d) == want)
                    {
                        start_osd_unit(host, *osd_id).await;
                        set_phase(disk_id, phase::ACTIVE, "Added to the storage pool.");
                        if let Ok(mut p) = PROGRESS.lock() {
                            let e = p.entry(disk_id.to_string()).or_default();
                            e.attempts = 0;
                            e.last_attempt = None;
                            e.orphan_osd_id = None;
                        }
                    } else {
                        // Silent until now, and it is the state a stuck disk
                        // actually sits in: creation reported success, the disk
                        // is still in no OSD map, and the next tick tries again.
                        // Twenty-odd rounds of that left nothing in the journal
                        // to say why.
                        tracing::warn!(
                            "{disk_id}: ceph-volume reported success but {dev_path} is in no OSD map — will retry"
                        );
                        set_phase(
                            disk_id,
                            phase::CREATING,
                            "Added to the storage pool; waiting for it to come online.",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("{disk_id}: created, but could not confirm it: {e}");
                    set_phase(disk_id, phase::CREATING, "Added. Checking it is working…")
                }
            }
        }
        Err(e) => {
            // Name the id this attempt allocated, so the next one can clear it
            // instead of leaving another blank OSD in the cluster. Only when
            // BOTH snapshots are real — a guess here ends in a purge.
            let leaked: Vec<i64> = match (&before, host.osd_ids().await.ok()) {
                (Some(before), Some(after)) => after
                    .iter()
                    .copied()
                    .filter(|id| !before.contains(id))
                    .collect(),
                _ => Vec::new(),
            };
            if let Some(&id) = leaked.first() {
                tracing::warn!(
                    "{disk_id}: create failed after allocating osd.{id} — reclaiming it"
                );
                if let Ok(mut p) = PROGRESS.lock() {
                    p.entry(disk_id.to_string()).or_default().orphan_osd_id = Some(id);
                }
                // Try now; if it does not work, the next attempt retries it.
                reclaim_orphan(host, disk_id).await;
            }
            // The error text is ceph-volume's, and it is for whoever reads the
            // journal. Putting it on the Storage page turns a clear "this is not
            // working yet" into a wall of Ceph vocabulary.
            tracing::warn!("{disk_id}: ceph-volume create failed: {e}");
            set_phase(
                disk_id,
                phase::RETRYING,
                "Could not add this disk yet. YoLab will keep trying.",
            );
        }
    }
}

/// Remove an OSD id left behind by a create that did not finish.
///
/// Narrow on purpose. It only ever touches an id this process watched appear
/// during its own failed `ceph-volume lvm create`, and only when Ceph itself
/// confirms destroying it loses nothing. A phantom has no CRUSH location and has
/// therefore never held a PG, so that check passes trivially for a real one and
/// refuses anything else.
///
/// It is deliberately NOT a sweep for id-with-no-host across the cluster: on a
/// multi-node cluster a phantom carries nothing that says which machine made it,
/// so a sweep on one node could purge an id another node is mid-way through
/// creating.
async fn reclaim_orphan<H: Host>(host: &H, disk_id: &str) {
    let Some(id) = PROGRESS
        .lock()
        .ok()
        .and_then(|p| p.get(disk_id).and_then(|d| d.orphan_osd_id))
    else {
        return;
    };

    // Only forget the id when Ceph actually says it is gone. `unwrap_or_default`
    // here would read an unreadable OSD list as "gone" and drop the only record
    // of the id, leaking it permanently — the exact bug this function exists to
    // prevent.
    let Ok(existing) = host.osd_ids().await else {
        return;
    };
    if !existing.contains(&id) {
        if let Ok(mut p) = PROGRESS.lock() {
            p.entry(disk_id.to_string()).or_default().orphan_osd_id = None;
        }
        return;
    }

    if !host.osd_safe_to_destroy(id).await {
        tracing::warn!(
            "{disk_id}: osd.{id} was left by a failed setup but Ceph will not confirm it is empty — leaving it"
        );
        return;
    }

    match host.osd_purge(id).await {
        Ok(_) => {
            let ids = host.osd_ids().await.ok();
            if purge_confirmed(ids.as_deref(), id) {
                tracing::info!("{disk_id}: removed osd.{id}, left behind by a failed setup");
                if let Ok(mut p) = PROGRESS.lock() {
                    p.entry(disk_id.to_string()).or_default().orphan_osd_id = None;
                }
            } else {
                tracing::warn!(
                    "{disk_id}: purge of osd.{id} reported success but it is still listed — keeping the record"
                );
            }
        }
        Err(e) => tracing::warn!("{disk_id}: could not remove leftover osd.{id}: {e}"),
    }
}

/// Stamp every disk Ceph knows about with its OSD id, and mark it as ours.
///
/// `disk_meta` decides `is_our_osd` by reading a BlueStore label from offset 0,
/// which only finds one when BlueStore was written straight to the device. Hand
/// ceph-volume a RAW disk and it wraps it in LVM first, so the label lives on the
/// LV inside and /dev/sdX itself reads as LVM2_member — no label. The disk was
/// therefore reported `is_our_osd: false` while carrying `osd_id: 1`, and the UI
/// reads that combination as "Setting up…": a healthy, fully-backfilled OSD sat
/// pulsing forever. The system disk escaped it only because it was already an LV,
/// so its label really is at offset 0.
///
/// An id from `ceph-volume lvm list` is authoritative — it comes from the LVM tags
/// ceph-volume itself wrote — so it overrides the label sniff, including
/// `foreign_ceph`: a disk cannot be OSD N of this cluster and another cluster's.
fn mark_known_osds(meta: &mut HashMap<String, Disk>, disk_to_osd: &HashMap<String, i64>) {
    for (disk_id, &osd_id) in disk_to_osd {
        if let Some(d) = meta.get_mut(disk_id) {
            d.osd_id = Some(osd_id);
            d.ownership = Ownership::Ours;
        }
    }
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
/// Whether a drained OSD may actually be purged and its disk wiped.
///
/// `safe-to-destroy` alone is not enough, and that is not a theoretical
/// objection. It answers "can the CLUSTER carry on without this disk" — and at
/// one copy, after the disk is marked out, the answer is yes precisely BECAUSE
/// Ceph has already written that data off as lost. The condition that makes
/// destruction look safe is the condition that makes it unrecoverable. Live,
/// that combination purged and zapped a disk holding the only copy of 63
/// placement groups, seconds after it came back.
#[derive(Debug, PartialEq, Eq)]
enum PurgeVerdict {
    /// Still draining. Ceph has not agreed yet.
    Wait,
    /// It agreed, then stopped agreeing once the daemon went down. Something
    /// changed underneath; leave the disk alone.
    Recheck,
    /// Data elsewhere is unreadable and unrebuildable, so this disk may hold
    /// the only copy of it.
    RefuseDataAtRisk,
    Purge,
}

fn plan_purge(
    safe_before_stop: bool,
    safe_after_stop: bool,
    loss: Option<&crate::routers::ceph::PgLoss>,
) -> PurgeVerdict {
    if !safe_before_stop {
        return PurgeVerdict::Wait;
    }
    if !safe_after_stop {
        return PurgeVerdict::Recheck;
    }
    if loss.is_some_and(|l| l.unrecoverable && l.stuck > 0) {
        return PurgeVerdict::RefuseDataAtRisk;
    }
    PurgeVerdict::Purge
}

/// A purge is only "done" when the OSD list confirms the id is gone. An
/// unreadable list never counts as gone — reading it that way would let a
/// spurious purge success proceed to wipe a disk.
fn purge_confirmed(ids: Option<&[i64]>, osd_id: i64) -> bool {
    ids.is_some_and(|ids| !ids.contains(&osd_id))
}

/// Whether this tick may act at all.
///
/// Both arms exist because acting on a guess here means `ceph-volume lvm
/// create` over live data. Split out from the reconcile loop so they can be
/// asserted without a cluster.
#[derive(Debug, PartialEq, Eq)]
enum TickPlan {
    /// Ceph did not answer. Silence is not "no OSDs".
    Unreachable,
    /// ceph-volume could not be read, so which disks already carry an OSD is
    /// unknown.
    UnknownOsdMap,
    Proceed,
}

fn plan_tick(reachable: bool, disk_to_osd_known: bool) -> TickPlan {
    if !reachable {
        TickPlan::Unreachable
    } else if !disk_to_osd_known {
        TickPlan::UnknownOsdMap
    } else {
        TickPlan::Proceed
    }
}

/// What the create half of a tick decides for one disk.
#[derive(Debug, PartialEq, Eq)]
enum CreatePlan {
    Create {
        dev_path: String,
    },
    Blocked(&'static str),
    /// Wants an OSD, nothing is blocking it, but the last attempt was too
    /// recent — see `retry_backoff`. Distinct from `Skip` so a test can tell
    /// "there is nothing to do here" from "there is, just not yet".
    Waiting,
    Skip,
}

/// How long to wait before retrying a failed create, given how many attempts
/// have already run. Exponential and capped: nothing existed to slow this
/// down before, and a permanently-stuck disk retried every 30s tick forever —
/// the Easystore was seen at attempt 22, each one still a full ceph-volume
/// invocation on the same interval as attempt 1.
fn retry_backoff(attempts: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 30;
    const CAP_SECS: u64 = 600; // 10 minutes
    let shift = attempts.saturating_sub(1).min(6); // 2^6 * 30s already exceeds the cap
    std::time::Duration::from_secs((BASE_SECS.saturating_mul(1 << shift)).min(CAP_SECS))
}

/// Whether enough time has passed since the last attempt to try again.
/// `since_last: None` — never attempted, or the timestamp did not survive a
/// process restart — always means "yes": this backs off *repeated* attempts,
/// it must never be the reason the very first one is delayed.
fn retry_due(attempts: u32, since_last: Option<std::time::Duration>) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => elapsed >= retry_backoff(attempts),
    }
}

/// The decision to run `ceph-volume lvm create` over a device, as a pure
/// function of the tick's inputs. This is the one place in the system that
/// turns somebody's disk into an OSD, and doing so destroys whatever was on it.
#[allow(clippy::too_many_arguments)]
fn plan_create(
    node: &str,
    disk_id: &str,
    d: &Disk,
    desired: &HashMap<String, String>,
    disk_to_osd: &HashMap<String, i64>,
    already_creating: bool,
    attempts: u32,
    since_last_attempt: Option<std::time::Duration>,
) -> CreatePlan {
    if !wants_on(desired, node, disk_id) || disk_to_osd.contains_key(disk_id) {
        return CreatePlan::Skip;
    }
    // A create started on an earlier tick may still be running. Starting a
    // second one for the same disk is how ceph-volume invocations used to stack
    // up.
    if already_creating {
        return CreatePlan::Skip;
    }
    if let Some(reason) = refuse_osd_creation(d) {
        return CreatePlan::Blocked(reason);
    }
    let Some(dev_path) = d.dev_path() else {
        return CreatePlan::Skip;
    };
    if !retry_due(attempts, since_last_attempt) {
        return CreatePlan::Waiting;
    }
    CreatePlan::Create { dev_path }
}

/// The owner's intent for one disk on one node. "USING" is the same intent as
/// "ON"; anything else, including an absent record, means off.
fn wants_on(desired: &HashMap<String, String>, node: &str, disk_id: &str) -> bool {
    desired
        .get(&record_key(node, disk_id))
        .map(|v| v == "ON" || v == "USING")
        .unwrap_or(false)
}

fn refuse_osd_creation(d: &Disk) -> Option<&'static str> {
    // These strings are shown to the person using the machine, not written to a
    // log, so they say what is true and what to do — no Ceph vocabulary, no
    // internal state names. The detail that used to live here is in the log line
    // at the call site instead.
    //
    // Matched exhaustively on purpose: a new Ownership variant has to be given
    // an answer here rather than defaulting into "safe to wipe", which is what
    // the boolean pair this replaced did whenever a key was missing.
    match d.ownership {
        Ownership::Foreign | Ownership::Unknown => {
            return Some(
                "This disk holds files from another storage system. Erase it first if you \
                 no longer need them.",
            )
        }
        Ownership::Ours => {
            return Some(
                "This disk already holds your files, but YoLab has lost track of it. It is \
                 being left alone rather than risk erasing it.",
            )
        }
        Ownership::Blank => {}
    }
    if d.dev_path().is_none() {
        return Some("This disk disappeared before it could be set up.");
    }
    // Both of these became load-bearing when partitioned disks started being
    // listed. Before that, `get_devices` dropped any disk with a partition
    // table, which hid every external drive that ships formatted — and hid the
    // OS disk as a side effect. Listing them is right; wiping them silently is
    // not.
    //
    // `mounted` first, and it is the important one: it is what now keeps the
    // disk this machine is running from out of reach. It is also stronger
    // evidence than "has a partition table" ever was, because it describes use
    // rather than shape.
    if d.mounted {
        return Some("This machine is using this disk for something else.");
    }
    if d.has_partitions {
        return Some("There is already something on this disk. Erase it to use it for storage.");
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
pub(crate) fn parse_lvm_list(raw: &str, our_fsid: &str) -> Result<Vec<(String, i64)>> {
    let json_start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object in ceph-volume output"))?;
    let v: Value =
        serde_json::from_str(&raw[json_start..]).context("parse ceph-volume lvm list")?;

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
            // ceph-volume lists every OSD whose LVM tags are on this host,
            // including ones belonging to another cluster — a disk carried over
            // from an earlier install still reads as "osd.1" here. Callers treat
            // an id from this list as proof the disk is ours, so a foreign OSD
            // got auto-enabled and its unit restarted forever against a mon that
            // rightly refused its key. Trust the tag, not the id.
            let tag_fsid = e["tags"]["ceph.cluster_fsid"].as_str().unwrap_or_default();
            if !tag_fsid.is_empty() && !our_fsid.is_empty() && tag_fsid != our_fsid {
                tracing::info!(
                    "parse_lvm_list: skipping osd.{id} — belongs to cluster {tag_fsid}, not {our_fsid}"
                );
                continue;
            }
            // BOTH identities, because ceph-volume reports different ones
            // depending on how the OSD was made:
            //
            //   osd on a raw disk : devices = ["/dev/sdb"]        <- matches us
            //   osd on an LVM LV  : devices = ["/dev/sda2"]       <- the *PV*
            //                       lv_path = "/dev/pool/ceph"    <- matches us
            //
            // The system OSD lives on an LV, so its `devices` entry names the
            // physical partition underneath the volume group — an identity our
            // inventory never holds. Parsing only `devices` left that OSD
            // permanently unmatched: the reconciler saw it as an unprovisioned
            // disk, never set its CRUSH weight, and every PG piled onto the
            // other OSD. Observed live as osd.0 sitting at weight 0 with 0 PGs
            // while the pool reported 81 undersized PGs that could never heal.
            if let Some(lv) = e["lv_path"].as_str().filter(|p| !p.is_empty()) {
                out.push((lv.to_string(), id));
            }
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

/// Ids in `raw` that do not belong to this cluster.
fn foreign_osd_ids(raw: &str, our_fsid: &str) -> Vec<i64> {
    let (Ok(all), Ok(ours)) = (parse_lvm_list(raw, ""), parse_lvm_list(raw, our_fsid)) else {
        return Vec::new();
    };
    let our_ids: Vec<i64> = ours.iter().map(|(_, id)| *id).collect();
    all.iter()
        .map(|(_, id)| *id)
        .filter(|id| !our_ids.contains(id))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The foreign OSD sitting on one specific device, if any.
fn foreign_osd_in_list(raw: &str, our_fsid: &str, dev_path: &str) -> Option<i64> {
    let foreign = foreign_osd_ids(raw, our_fsid);
    let want = canonical_device(dev_path);
    parse_lvm_list(raw, "")
        .ok()?
        .into_iter()
        .find(|(d, id)| foreign.contains(id) && canonical_device(d) == want)
        .map(|(_, id)| id)
}

/// Which zap, if any, clears the way for a create that just failed.
///
/// `--destroy` removes the volume group. Right for a foreign stack, which is
/// the leftover being erased; wrong for the system LV, which disko owns and
/// only whose contents are stale.
fn zap_args<'a>(dev_path: &'a str, foreign: bool, err: &str) -> Option<Vec<&'a str>> {
    if foreign {
        Some(vec!["lvm", "zap", "--destroy", dev_path])
    } else if is_stale_signature(err) {
        Some(vec!["lvm", "zap", dev_path])
    } else {
        None
    }
}

/// Stop OSD units belonging to another cluster.
///
/// The counterpart to `ensure_osd_unit_running`. A disk left by an earlier
/// install still has this host's LVM tags, so its unit can be started — once,
/// by a release that had not yet learned to read `ceph.cluster_fsid`, or by
/// hand. The mon then refuses its key and `Restart=on-failure` flaps it
/// forever. Skipping such an OSD is enough to stop starting it, but not to
/// stop one already running: nothing else ever looks at it again.
fn is_running(state: Option<&str>) -> bool {
    matches!(state, Some("active" | "activating" | "reloading"))
}

/// "Stopped" is only ever a *known* dead state. None is the systemctl-error
/// case, and reading it as stopped would let a purge run against a daemon that
/// may still hold the device.
fn is_stopped(state: Option<&str>) -> bool {
    matches!(state, Some("inactive" | "failed"))
}

async fn osd_unit_state<H: Host>(host: &H, osd_id: i64) -> Option<String> {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    host.systemctl(&["show", "-p", "ActiveState", "--value", &unit])
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn stop_foreign_osd_units<H: Host>(host: &H) {
    let Ok(raw) = host.ceph_volume(&["lvm", "list", "--format", "json"]).await else {
        return;
    };
    let Some(our_fsid) = host.cluster_fsid().await.filter(|f| !f.is_empty()) else {
        return;
    };
    for id in foreign_osd_ids(&raw, &our_fsid) {
        let unit = format!("yolab-ceph-osd@{id}.service");
        // ActiveState, not is-active/is-failed: a flapping unit sits in
        // `activating (auto-restart)`, and both of those report non-zero for
        // it — the one state this exists to catch would have been skipped.
        let state = host
            .systemctl(&["show", "-p", "ActiveState", "--value", &unit])
            .await
            .ok()
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        if state.is_empty() || state == "inactive" {
            continue;
        }
        tracing::warn!("osd.{id}: belongs to another cluster — stopping {unit}");
        let _ = host.systemctl(&["stop", &unit]).await;
    }
}

/// Start this OSD's unit if it is not already running. Cheap enough to call on
/// every tick: `is-active` is a bus query, and the start only runs when
/// something is actually wrong.
///
/// This is what converges an OSD whose daemon died — a failed start, a manual
/// systemctl stop, a crash past the restart limit. Without it an OSD could sit
/// created-but-down indefinitely with the reconciler reporting nothing wrong,
/// which is exactly what a read-only /etc/systemd/system produced.
async fn ensure_osd_unit_running<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    if is_running(osd_unit_state(host, osd_id).await.as_deref()) {
        return;
    }
    tracing::warn!("osd.{osd_id}: {unit} is not running — starting it");
    start_osd_unit(host, osd_id).await;
}

/// Start this OSD's systemd instance.
///
/// `start`, never `enable`. Enabling writes a symlink into
/// /etc/systemd/system, which on NixOS is a read-only Nix store path, so
/// `systemctl enable` fails with "Read-only file system" — observed live with
/// both OSDs created and neither running. Persistence across reboots comes from
/// the declarative yolab-ceph-osd-activate unit, which enumerates prepared OSDs
/// from ceph-volume and starts an instance for each.
async fn start_osd_unit<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: starting {unit}");
    match host.systemctl(&["start", &unit]).await {
        Ok(o) if o.success => tracing::info!("osd.{osd_id}: {unit} started"),
        Ok(o) => tracing::warn!("osd.{osd_id}: starting {unit} failed: {}", o.stderr.trim()),
        Err(e) => tracing::warn!("osd.{osd_id}: could not run systemctl: {e}"),
    }
}

/// Stop this OSD's systemd instance and wait for the process to actually be
/// gone. Purging an OSD whose daemon still holds the device fails with EBUSY,
/// so this must complete before any purge.
async fn disable_osd_unit<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: stopping {unit}");
    // `stop`, not `disable --now`, for the same reason start is not enable:
    // disabling touches the read-only /etc/systemd/system. Nothing needs
    // un-enabling anyway — yolab-ceph-osd-activate derives what to start from
    // ceph-volume, and a purged OSD disappears from there on its own.
    let _ = host.systemctl(&["stop", &unit]).await;

    // `systemctl disable --now` returns once systemd has reaped the unit, but
    // give the device a moment to be released before anything touches it.
    for _ in 0..15 {
        if is_stopped(osd_unit_state(host, osd_id).await.as_deref()) {
            return;
        }
        sleep(Duration::from_secs(1)).await;
    }
    tracing::warn!("osd.{osd_id}: {unit} still active after 15s");
}

/// Return a purged OSD's disk to the state it was in before it was added, so it
/// can be switched back ON — or unplugged and used elsewhere.
///
/// WHY NOT `dd`
/// ------------
/// This used to zero the first 100 MiB, which destroys the BlueStore superblock
/// and satisfies anything that looks for a label. It does not undo what
/// ceph-volume actually did.
///
/// `ceph-volume lvm create` puts the OSD inside LVM: a physical volume on the
/// disk, a volume group, and a logical volume holding BlueStore. Zeroing the
/// front of the disk erases the PV label but leaves the volume group in LVM's
/// metadata and the logical volume ACTIVE in device-mapper, still holding the
/// device open. The disk then looks blank while remaining busy, and the next
/// `ceph-volume lvm create` on it fails — permanently. Switching a disk OFF and
/// back ON is the ordinary thing to do with a toggle, and it could not work.
///
/// `zap` is ceph-volume's own undo: it deactivates the LV, removes the VG and
/// PV, and wipes the device.
///
/// `--destroy` ONLY for whole disks. On the system OSD the BlueStore volume is
/// an LVM volume disko created at install and the OS depends on the volume group
/// around it — `--destroy` there would delete the volume itself, and there is
/// nothing to recreate it. Plain `zap` wipes the contents and leaves the volume.
///
/// Only ever called after our own successful `ceph osd purge` of that exact OSD
/// in this same call — never on a disk we merely suspect is drained.
async fn wipe_device<H: Host>(host: &H, device: &str) {
    let dev_path = if device.starts_with('/') {
        device.to_string()
    } else {
        format!("/dev/{device}")
    };

    // A device-mapper path is a logical volume we did not create and must not
    // remove; a plain disk is one ceph-volume built its own LVM stack on.
    let is_lv = dev_path.starts_with("/dev/mapper/") || dev_path.starts_with("/dev/dm-");
    let mut args: Vec<&str> = vec!["lvm", "zap"];
    if !is_lv {
        args.push("--destroy");
    }
    args.push(&dev_path);

    match host.ceph_volume(&args).await {
        Ok(_) => tracing::info!("wipe_device: {dev_path} zapped and returned to a blank state"),
        Err(e) => tracing::warn!(
            "wipe_device: could not zap {dev_path}: {e} — the disk stays registered and this \
             is retried on the next tick"
        ),
    }
}

/// Purge OSDs on this node that have been fully drained and whose Rook
/// deployment is already gone. Safe conditions (all must hold):
///   1. OSD is in the CRUSH tree under this node's host bucket
///   2. Disk is no longer locally present (not in disk_to_osd)
///   3. OSD is down + reweight ≤ 0.5 (out)
///   4. Rook deployment is gone (no EBUSY — daemon is not running)
///   5. `ceph osd safe-to-destroy` confirms no PG data remains
async fn purge_drained_osds<H: Host>(
    host: &H,
    node: &str,
    crush_nodes: &[Value],
    disk_to_osd: &HashMap<String, i64>,
    unplugged_but_wanted: bool,
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
        if n["type"].as_str() != Some("osd") {
            continue;
        }
        let Some(osd_id) = n["id"].as_i64() else {
            continue;
        };
        if !host_osd_ids.contains(&osd_id) {
            continue;
        } // not this node's OSD
        if active_osd_ids.contains(&osd_id) {
            continue;
        } // disk still here, main loop handles it

        // Some disk on this node is switched ON and not here. Any of these
        // leftovers could be its, so none of them are touched.
        //
        // Without this, unplugging a disk for ten minutes destroyed it: Ceph
        // marks an OSD out after mon_osd_down_out_interval (600s), which
        // satisfies every condition below, so the next tick purged it. Plug the
        // disk back in and it still carries our BlueStore label while Ceph has
        // no such OSD — precisely the state refuse_osd_creation blocks — so it
        // could never be re-added without being erased first.
        //
        // A disk someone unplugged and a disk someone switched off are different
        // things, and only the second one asked to be taken apart.
        if unplugged_but_wanted {
            tracing::info!(
                "osd.{osd_id}: leaving it alone — a disk on this node is switched on but not \
                 connected, and this may be it"
            );
            continue;
        }

        let reweight = n["reweight"].as_f64().unwrap_or(1.0);
        let status = n["status"].as_str().unwrap_or("up");
        if reweight > 0.5 || status != "down" {
            continue;
        } // not fully drained/stopped

        // The daemon must not be running — never purge underneath a live OSD.
        disable_osd_unit(host, osd_id).await;

        // Confirm no PG data remains before destroying the OSD record.
        if !host.osd_safe_to_destroy(osd_id).await {
            tracing::info!("osd.{osd_id}: disk gone but not yet safe-to-destroy — waiting");
            continue;
        }

        tracing::info!("osd.{osd_id}: disk gone, out, safe-to-destroy — purging from Ceph");
        match host
            .ceph(&[
                "osd",
                "purge",
                &format!("osd.{osd_id}"),
                "--yes-i-really-mean-it",
            ])
            .await
        {
            Ok(_) => tracing::info!("osd.{osd_id}: purged"),
            Err(e) => tracing::warn!("osd.{osd_id}: purge failed: {e}"),
        }
    }
}

/// Convert raw capacity to TiB for a CRUSH weight.
/// Prefers Ceph's own `kb` (from `osd df tree`) over lsblk size_bytes when available.
fn weight_tib_from(kb: u64, size_bytes: u64) -> f64 {
    if kb > 0 {
        kb as f64 / (1u64 << 30) as f64 // KB → TiB: divide by 2^30 (1 TiB = 2^30 KB)
    } else if size_bytes > 0 {
        size_bytes as f64 / (1u64 << 40) as f64
    } else {
        0.0
    }
}

/// Carry a disk's setting across a change in how disks are identified.
///
/// Records are keyed by `<node>--<disk_id>`, and disk_id changed shape in this release:
/// it used to fall back to the kernel name (`dev-sdb`) on hardware where the old serial
/// path did not exist, which on some machines was every disk. After the change the same
/// disk is `serial-wwn-0x…`, so every existing record would look like it belongs to a
/// disk that is no longer present, and every present disk would look brand new.
///
/// Brand new means OFF. Applied to a disk already carrying an OSD, OFF means drain,
/// purge and wipe — which is exactly the accident this release exists to prevent, and
/// it would have been triggered BY the release. `auto_register_all_disks` refuses to
/// register a live OSD's disk as OFF, which covers the disks that are running; this
/// covers the rest, so a disk deliberately switched OFF does not silently come back ON,
/// and one switched ON before an unplug is still ON when it returns.
///
/// Pure so the mapping can be tested without a cluster: given the old records and the
/// devices seen now, return the entries to write.
/// What a migration pass wants written and removed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Migration {
    /// Keys to set, in the current shape.
    pub write: HashMap<String, String>,
    /// Keys to delete. Carrying a value forward and leaving the source behind is what
    /// produced two rows for one disk — and worse, two records for one disk that can
    /// disagree, with only one of them connected to anything.
    pub remove: Vec<String>,
}

fn migrate_disk_records(
    node: &str,
    // `current`: disk_id -> the kernel device it is at now, for every disk seen.
    current: &[(String, String)],
    // `desired`: existing records, `<node>--<disk_id>` -> "ON"/"OFF".
    desired: &HashMap<String, String>,
) -> Migration {
    let mut out = HashMap::new();
    let mut remove: Vec<String> = Vec::new();
    for (disk_id, device) in current {
        let new_key = record_key(node, disk_id);
        // Not `continue` when the new key already exists.
        //
        // That is the state a half-finished migration leaves — and the one the live
        // cluster was in: the bare key written, the node-scoped source still sitting
        // beside it. Skipping meant the duplicate could never be cleared, so one disk
        // kept two records that were free to disagree, and did.
        let already_migrated = desired.contains_key(&new_key);
        // Two shapes can precede this one, and both are checked: the kernel-name key
        // from before disks were identified by hardware, and the node-scoped hardware
        // key from before hardware ids stopped being node-scoped. A cluster can be
        // updated straight from the first to the third, so neither may be skipped.
        // Order matters, and getting it wrong cost a live drain.
        //
        // The node-scoped HARDWARE key is tried first because it is the most recent
        // and the most specific: it was written by the previous release for this exact
        // disk. The kernel-name key is a last resort — it belongs to whatever was at
        // that letter under a scheme that could not tell disks apart, and stale ones
        // linger for devices that have long since moved.
        //
        // Reversed, the stale one won. `node1--dev-sdc` was an orphan reading OFF;
        // `node1--serial-wwn-…` was the live record reading ON. The migration carried
        // OFF, the reconciler read OFF as an instruction, and started draining a disk
        // nobody had asked to remove.
        for old_key in [
            format!("{node}--{disk_id}"),
            format!("{node}--dev-{device}"),
        ] {
            if old_key == new_key {
                continue; // nothing to carry onto itself
            }
            if let Some(setting) = desired.get(&old_key) {
                // Only the first match supplies the value — they are ordered
                // best-first — but EVERY predecessor is superseded and every one goes.
                // Keeping any of them leaves a second record for the same hardware,
                // which is how one disk ended up with an ON record carrying the
                // visible toggle and an OFF record doing the deciding.
                if !already_migrated && !out.contains_key(&new_key) {
                    out.insert(new_key.clone(), setting.clone());
                }
                remove.push(old_key);
            }
        }
    }

    // Kernel-name records for disks that are not at that device any more.
    //
    // These cannot be matched by identity ever again: a disk that publishes any
    // hardware id is now keyed by it, so a `dev-*` record can only ever be picked up
    // by whatever unrelated disk next lands on that letter. That is not a stale
    // record, it is a trap — and it has already sprung once, when a stale `dev-sdc`
    // reading OFF was carried onto a live disk and started draining it.
    //
    // Losing a setting is the cheap failure here: a disk with no record is OFF, and
    // OFF can no longer destroy anything now that a live OSD's disk is never
    // registered off.
    let devices_now: std::collections::HashSet<&str> =
        current.iter().map(|(_, d)| d.as_str()).collect();
    for key in desired.keys() {
        let Some(id) = key.strip_prefix(&format!("{node}--")) else {
            continue;
        };
        let Some(device) = id.strip_prefix("dev-") else {
            continue;
        };
        if !devices_now.contains(device) && !remove.contains(key) {
            remove.push(key.clone());
        }
    }

    Migration { write: out, remove }
}

/// Register every newly detected disk in the config CM on first sight.
/// System disk (is_loop) defaults ON — it's always the primary storage.
/// All other disks default OFF; the user must explicitly enable them.
/// Foreign-Ceph disks are registered too so they show in the UI.
async fn auto_register_all_disks<H: Host>(
    host: &H,
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
) {
    // Before deciding anything is new: carry over records this release's change in
    // disk identity would otherwise have orphaned.
    let current: Vec<(String, String)> = meta
        .iter()
        .filter(|(_, m)| !m.device.is_empty())
        .map(|(id, m)| (id.clone(), m.device.clone()))
        .collect();
    let migration = migrate_disk_records(node, &current, desired);
    let mut new_entries: HashMap<String, String> = migration.write;
    if !new_entries.is_empty() {
        tracing::info!(
            "auto_register_all_disks: carried {} disk setting(s) onto their hardware ids on {node}",
            new_entries.len()
        );
    }
    // A merge patch deletes by setting a key to null, so removals ride along with the
    // writes in one request — the two must not be able to half-apply, or a value could
    // be dropped without its replacement landing.
    let mut patch_data: serde_json::Map<String, Value> = new_entries
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    if !migration.remove.is_empty() {
        tracing::info!(
            "auto_register_all_disks: removing {} superseded disk record(s) on {node}: {}",
            migration.remove.len(),
            migration.remove.join(", ")
        );
        for key in &migration.remove {
            patch_data.insert(key.clone(), Value::Null);
        }
    }

    for (disk_id, m) in meta {
        let key = record_key(node, disk_id);
        if desired.contains_key(&key) || new_entries.contains_key(&key) {
            continue;
        }

        // A disk with no record is normally new, and new disks are OFF until someone
        // asks for them. But "no record" also happens when a disk's IDENTITY changes
        // while the disk itself does not — a replug landing on a different kernel
        // name, or this very release moving from `dev-sdb` to the hardware id.
        //
        // Defaulting those to OFF is how a running OSD carrying live data gets read as
        // "the owner does not want this disk", then drained, purged and wiped. That is
        // not hypothetical: it happened, twice, and destroyed the only copy of 63
        // placement groups.
        //
        // A disk that is already running one of OUR OSDs is therefore ON, whatever the
        // record says, because the OSD is itself the evidence that somebody switched it
        // on. The record was lost; the decision it recorded was not.
        let default = if m.is_loop || m.ownership.is_ours() {
            "ON"
        } else {
            "OFF"
        };
        if default == "ON" && !m.is_loop {
            tracing::info!(
                "auto_register_all_disks: {disk_id} has no record but is running one of our OSDs — registering it ON, not OFF"
            );
        }
        new_entries.insert(key.clone(), default.to_string());
        patch_data.insert(key, Value::String(default.to_string()));
    }
    if patch_data.is_empty() {
        return;
    }

    let patch = json!({ "data": Value::Object(patch_data) }).to_string();
    if let Err(e) = host
        .kubectl(&[
            "patch",
            "configmap",
            CONFIG_CM,
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
    {
        let _ = host
            .kubectl(&["create", "configmap", CONFIG_CM, "-n", NS])
            .await;
        if let Err(e2) = host
            .kubectl(&[
                "patch",
                "configmap",
                CONFIG_CM,
                "-n",
                NS,
                "--type",
                "merge",
                "-p",
                &patch,
            ])
            .await
        {
            tracing::warn!("auto_register_all_disks: {e}, then {e2}");
        } else {
            tracing::info!(
                "auto_register_all_disks: registered {} new disk(s) on {node}",
                new_entries.len()
            );
        }
    } else {
        tracing::info!(
            "auto_register_all_disks: registered {} new disk(s) on {node}",
            new_entries.len()
        );
    }
}

// ── Device discovery ──────────────────────────────────────────────────────────

/// Returns pluggable physical disks without partition tables.
/// Uses `lsblk -J` so device-type classification and partition detection are
/// handled by the kernel rather than manual prefix matching on device names.
/// Disks WITH partition children are OS/boot disks — excluded here so they
/// never appear in the Ceph disk list.
/// Whether a block device is a real disk a user could switch on, rather than
/// something the system created for its own use.
///
/// lsblk reports several virtual devices as `type: "disk"` with no partitions,
/// so the type check alone lets them through. The one that matters is **rbd**:
/// /dev/rbd0 is our own container image store, mapped from the images pool. It
/// showed up on the Storage page as an activatable 303 GB disk, and switching it
/// on would have run `ceph-volume lvm create` over the image store — destroying
/// it. `refuse_osd_creation` would NOT have caught that: rbd0 carries an xfs
/// filesystem, not a BlueStore label, so it reads as a blank disk.
///
/// zram/zd (ZFS zvols), md (software RAID) and dm (LVM/crypt mappings) are
/// excluded for the same reason — none is a physical disk a user plugged in.
fn is_user_disk(name: &str) -> bool {
    const VIRTUAL_PREFIXES: [&str; 6] = ["rbd", "loop", "zram", "zd", "md", "dm-"];
    !VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// What lsblk knows about a disk that /sys does not: whether it is carved into
/// partitions, and whether this machine is using any of them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DiskFlags {
    pub has_partitions: bool,
    /// The disk, or anything on it, is mounted. This is what keeps the OS disk
    /// out of reach now that partitioned disks are listed.
    pub mounted: bool,
}

/// Walk one lsblk tree into `DiskFlags`.
///
/// Pure, and separated from the lsblk call, because the safety of listing
/// partitioned disks rests entirely on this being right: `mounted` is what
/// stands between "every disk gets a toggle" and someone switching on the disk
/// their operating system is running from.
///
/// Mount state is inherited downward-to-upward — a disk counts as mounted when
/// ANY descendant is, at any depth. The root filesystem is usually two levels
/// down (disk -> partition -> LVM volume), so checking only direct children
/// would miss exactly the case that matters most.
pub(crate) fn parse_disk_flags(dev: &Value) -> DiskFlags {
    fn mounted_anywhere(n: &Value) -> bool {
        let own = match &n["mountpoints"] {
            Value::Array(a) => a.iter().any(|m| m.as_str().is_some_and(|s| !s.is_empty())),
            v => v.as_str().is_some_and(|s| !s.is_empty()),
        };
        // `mountpoint` (singular) on older util-linux.
        let legacy = n["mountpoint"].as_str().is_some_and(|s| !s.is_empty());
        own || legacy
            || n["children"]
                .as_array()
                .is_some_and(|c| c.iter().any(mounted_anywhere))
    }

    DiskFlags {
        has_partitions: dev["children"]
            .as_array()
            .is_some_and(|c| c.iter().any(|ch| ch["type"].as_str() == Some("part"))),
        mounted: mounted_anywhere(dev),
    }
}

/// Every switchable disk on this machine, with the facts that decide whether it
/// may be switched on.
/// None when lsblk could not be read. Not an empty list: "this machine has no
/// disks" would make `purge_drained_osds` believe every OSD's disk had been
/// unplugged.
async fn scan_devices<H: Host>(host: &H) -> Option<Vec<(String, DiskFlags)>> {
    // MOUNTPOINTS, not just NAME/TYPE: mount state is what keeps the OS disk
    // from being offered as storage now that partitioned disks are listed.
    let out = host
        .run_cmd("lsblk", &["-J", "-o", "NAME,TYPE,MOUNTPOINTS"])
        .await
        .ok()?;
    if !out.success {
        return None;
    }
    let json = serde_json::from_slice::<Value>(out.stdout.as_bytes()).ok()?;
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
            if !is_user_disk(&name) {
                continue;
            }
            // Partitioned disks are INCLUDED. They used to be dropped here as
            // "OS/boot disks", which meant most external drives — nearly all
            // ship with one exFAT or NTFS partition — simply never appeared in
            // the list. Plug one in and nothing happens, with no explanation.
            //
            // The real OS disk is excluded elsewhere and by better evidence:
            // it is mounted, and the system OSD is injected separately. What is
            // left is a disk with data on it, which is a question for the person
            // who plugged it in, not a reason to pretend it is not there.
            // `refuse_osd_creation` is what stops it being wiped without a
            // decision; see `has_partitions` in disk_meta.
            let flags = parse_disk_flags(dev);
            // A mounted disk is never offerable, and listing it is actively
            // confusing: the OS disk would appear twice, once as itself and once
            // as the "System disk" row that represents the Ceph volume carved
            // out of it. `refuse_osd_creation` still checks `mounted` as a
            // backstop against something being mounted between this scan and a
            // create.
            if flags.mounted {
                continue;
            }
            devices.push((name, flags));
        }
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    Some(devices)
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
    let pos = buf
        .windows(CEPH_FSID_KEY.len())
        .position(|w| w == CEPH_FSID_KEY)?;
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

/// The config-map key holding a disk's ON/OFF setting.
///
/// Node-scoped only when the id is not unique on its own. That distinction is the
/// whole reason the prefix existed: under the old scheme a disk could be `dev-sda`,
/// and node1's `dev-sda` and node3's `dev-sda` are different disks, so a bare key
/// would have had one machine's setting silently governing another's hardware. The
/// same is true of `system`, which every node has exactly one of.
///
/// A hardware id is not like that. `serial-wwn-0x50014ee214caf529` is that disk
/// anywhere on earth, so scoping it to a node makes the record describe "this disk,
/// while it happens to be in this machine" — and moving the disk to another node makes
/// it a stranger again: no record, registered new, and new means OFF.
///
/// Which is the thing a person most wants to work. A machine dies, the disks are fine,
/// they go in another box. Ceph can already do it — an OSD carries its own identity in
/// the BlueStore label and reports its new location when it starts — so the only thing
/// standing in the way was this key.
pub(crate) fn record_key(node: &str, disk_id: &str) -> String {
    if is_globally_unique_id(disk_id) {
        disk_id.to_string()
    } else {
        format!("{node}--{disk_id}")
    }
}

/// True when an id identifies one physical disk rather than a slot on one machine.
///
/// `serial-` is what disk_id emits once udev gave it something from the hardware —
/// a WWN, an NVMe EUI, a model+serial. Everything else (`dev-<name>`, `system`) names
/// a position, and positions repeat across machines.
pub(crate) fn is_globally_unique_id(disk_id: &str) -> bool {
    disk_id.starts_with("serial-")
}

// ── Stable disk identity ──────────────────────────────────────────────────────

/// Identifiers udev publishes in /dev/disk/by-id, best first.
///
/// `wwn-` is a World Wide Name: IEEE-registered, burned in at manufacture, and the
/// closest thing a disk has to a UUID. `nvme-eui`/`nvme-` are its NVMe equivalents.
/// `ata-`/`scsi-` carry model plus serial. `usb-` is last because a cheap enclosure
/// may synthesise it from the BRIDGE rather than the drive — stable per caddy, not per
/// disk — so it identifies the slot, not what is in it. Still far better than a kernel
/// name, and honest about being weaker.
const ID_PREFIXES: [&str; 6] = ["wwn-", "nvme-eui.", "nvme-", "ata-", "scsi-", "usb-"];
const BY_ID_DIR: &str = "/dev/disk/by-id";

/// A stable name for a disk, from what udev knows about the hardware.
///
/// This used to read `/sys/block/<dev>/device/serial` and fall back to `dev-<name>`.
/// That path does not exist for libata or USB disks — on the machine that prompted
/// this, it was absent for EVERY disk, internal SSD included — so every disk was
/// identified by its kernel name, and a kernel name is assignment order, not identity.
///
/// The cost of that was not cosmetic. A disk was unplugged as `sdb` and came back as
/// `sdc`; the reconciler saw an unknown disk, registered it as new (and new disks are
/// OFF), then read that OFF back as an instruction and purged and wiped the OSD living
/// on it. Same physical disk, two names, and the data was destroyed automatically.
fn disk_id(device: &str) -> String {
    disk_id_from(device, stable_id_for(device).as_deref())
}

/// The best `/dev/disk/by-id` link pointing at `device`.
///
/// Symlinks are resolved rather than parsed: `by-id` also contains partition links
/// (`...-part1`), and matching on the name alone would happily identify a whole disk
/// by one of its partitions.
fn stable_id_for(device: &str) -> Option<String> {
    let target = std::fs::canonicalize(format!("/dev/{device}")).ok()?;
    let mut best: Option<(usize, String)> = None;
    for entry in std::fs::read_dir(BY_ID_DIR).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("-part") || name.contains("-part") {
            continue;
        }
        let Some(rank) = ID_PREFIXES.iter().position(|p| name.starts_with(p)) else {
            continue;
        };
        if std::fs::canonicalize(entry.path()).ok().as_deref() != Some(target.as_path()) {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, name));
        }
    }
    best.map(|(_, name)| name)
}

/// Split from `disk_id` so the sanitizing rules can be tested without a real device.
/// A disk's id ends up as a ConfigMap *key*, so whatever the vendor wrote in the
/// serial has to come out as `[a-z0-9-]`.
fn disk_id_from(device: &str, stable: Option<&str>) -> String {
    if let Some(serial) = stable {
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

fn disk_meta(device: &str, our_fsid: &str, flags: DiskFlags) -> Disk {
    let model = std::fs::read_to_string(format!("/sys/block/{device}/device/model"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let size_bytes: u64 = std::fs::read_to_string(format!("/sys/block/{device}/size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 512;
    Disk {
        device: device.to_string(),
        model,
        size_bytes,
        is_loop: false,
        ownership: Ownership::read(bluestore_fsid(device).as_deref(), our_fsid),
        has_partitions: flags.has_partitions,
        mounted: flags.mounted,
        osd_id: None,
        progress: None,
    }
}

// ── ConfigMap helpers ─────────────────────────────────────────────────────────

/// The cluster's own fsid, straight from the local mon. Returns None when Ceph
/// is unreachable — never a default — because callers compare it against a
/// disk's BlueStore label to tell our disks from a stranger's, and an empty
/// string would make every foreign disk match.
async fn write_status<H: Host>(host: &H, node: &str, meta: &HashMap<String, Disk>) {
    // Only `disks`. There used to be an `effective` device list here, assembled
    // for the leader to write into the Rook CephCluster CR. There is no CR any
    // more — each node creates its own OSDs — and nothing had read the field
    // since, so it was ~30 lines (and a `classify` pass over every disk) whose
    // only effect was to make the format look like it still meant something.
    // One place turns Disk into wire JSON, so the ConfigMap the UI reads
    // cannot drift field by field.
    let wire: HashMap<&str, Value> = meta
        .iter()
        .map(|(k, d)| (k.as_str(), d.to_value()))
        .collect();
    let payload = json!({ "disks": wire });
    let json_val = serde_json::to_string(&payload).unwrap_or_default();
    let patch = json!({"data": {node: json_val}}).to_string();
    if host
        .kubectl(&[
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
        let _ = host
            .kubectl(&["create", "configmap", STATUS_CM, "-n", NS])
            .await;
        let _ = host
            .kubectl(&[
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

/// The ON/OFF the user set, or None when it could not be read.
///
/// THE MOST DANGEROUS FUNCTION IN THIS FILE. It used to return an empty map on
/// any failure, and the reconciler reads a missing entry as OFF — so a
/// Kubernetes API that was briefly unreachable meant *every disk on the node is
/// switched off*. The OFF path is fully automatic and ends in `ceph osd out`,
/// `ceph osd purge`, and wiping the device.
///
/// On one machine that is survivable: with nowhere to move the data,
/// safe-to-destroy never passes and the purge never happens. On a cluster it is
/// not: one node losing kubectl marks its own OSDs out, the data drains
/// correctly onto its peers, safe-to-destroy then passes, and that node's disks
/// are purged and zeroed — because an API server restarted.
///
/// The API server restarts on every nixos-rebuild. This is not a rare path.
///
/// `-o json` on the whole object rather than `jsonpath={.data}`, because a
/// ConfigMap that exists with no data yet produces an empty jsonpath result that
/// is indistinguishable from a failed call — and those two must never be
/// confused. The `kind` check is what proves a real object came back.
async fn read_desired<H: Host>(host: &H) -> Option<HashMap<String, String>> {
    let v = match host
        .kubectl_json(&["get", "configmap", CONFIG_CM, "-n", NS, "-o", "json"])
        .await
    {
        Ok(v) => v,
        Err(e) => {
            // A missing ConfigMap is the genuine first-run state: nothing has
            // been switched on or off yet. It must not read as "cannot reach
            // Kubernetes" — that reading is what deadlocked a fresh install,
            // because the only path that creates this map
            // (auto_register_all_disks) runs behind read_desired. NotFound is
            // the one error that means "no one has ever set a disk", so it is
            // safe to return an empty map and let the caller register every
            // disk and create the map.
            //
            // Every other error still means None: an unreachable API server
            // read as "all disks OFF" is the drain/purge/wipe accident the None
            // return exists to prevent (see the header above).
            if kubectl::is_not_found(&e) {
                return Some(HashMap::new());
            }
            return None;
        }
    };
    if v["kind"].as_str() != Some("ConfigMap") {
        return None;
    }
    Some(
        v["data"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
    )
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
    use std::future::Future;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use crate::host::CommandOutput;

    const OURS: &str = "11111111-2222-3333-4444-555555555555";
    const THEIRS: &str = "99999999-8888-7777-6666-555555555555";

    /// A host that never reaches the cluster or the machine, but counts the one
    /// call that would destroy data if it slipped past the guards.
    #[derive(Clone, Default)]
    struct RecordingHost {
        ceph_volume_calls: Arc<Mutex<usize>>,
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for RecordingHost {
        fn ceph<'a>(&self, _args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
            async move { Err(anyhow::anyhow!("ceph unreachable")) }
        }

        fn ceph_json<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            async move { Err(anyhow::anyhow!("ceph unreachable")) }
        }

        fn ceph_volume<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            let calls = self.ceph_volume_calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!("ceph unreachable"))
            }
        }

        fn kubectl<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            async move { Err(anyhow::anyhow!("kubectl unreachable")) }
        }

        fn kubectl_json<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            async move { Err(anyhow::anyhow!("kubectl unreachable")) }
        }

        fn systemctl<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            async move { Err(anyhow::anyhow!("systemctl unreachable")) }
        }

        fn run_cmd<'a>(
            &self,
            _bin: &'a str,
            _args: &'a [&'a str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            async move { Err(anyhow::anyhow!("command unreachable")) }
        }
    }

    /// A host that records every command and answers from a script.
    ///
    /// `RecordingHost` above can only say "unreachable", which is why exactly
    /// one test ever drove the reconcile path through it — leaving the
    /// orchestration untested while its constituent decisions were covered
    /// well. That gap is how a8ba132 shipped: the decision function was right
    /// and the wiring that called it was not.
    ///
    /// Every effect the reconciler has on a disk is a command, so scripting
    /// commands is what makes the wiring assertable. Unscripted commands fail
    /// by default: a test must say what the machine answers, rather than
    /// silently getting a plausible one.
    type ScriptedAnswer = std::result::Result<String, String>;
    type Script = Vec<(String, std::collections::VecDeque<ScriptedAnswer>)>;

    #[derive(Clone, Default)]
    struct FakeHost {
        calls: Arc<Mutex<Vec<String>>>,
        // Each prefix maps to a queue of answers, consumed in call order; the
        // last answer repeats once the queue is empty, so a test only has to
        // spell out the calls whose answer actually changes (e.g. `ceph-volume
        // lvm list` before and after a create) and can leave a steady-state
        // answer for everything else.
        script: Arc<Mutex<Script>>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self::default()
        }

        fn push(&self, prefix: &str, answer: ScriptedAnswer) {
            let mut script = self.script.lock().unwrap();
            match script.iter_mut().find(|(p, _)| p == prefix) {
                Some((_, q)) => q.push_back(answer),
                None => {
                    let mut q = std::collections::VecDeque::new();
                    q.push_back(answer);
                    script.push((prefix.to_string(), q));
                }
            }
        }

        fn ok(self, prefix: &str, out: &str) -> Self {
            self.push(prefix, Ok(out.to_string()));
            self
        }

        fn fail(self, prefix: &str, err: &str) -> Self {
            self.push(prefix, Err(err.to_string()));
            self
        }

        /// Longest matching prefix wins, so a general steady-state answer and
        /// a more specific override can coexist without colliding.
        fn answer(&self, cmd: &str) -> Result<String> {
            self.calls.lock().unwrap().push(cmd.to_string());
            let mut script = self.script.lock().unwrap();
            let best = script
                .iter_mut()
                .filter(|(p, _)| cmd.starts_with(p.as_str()))
                .max_by_key(|(p, _)| p.len());
            match best {
                Some((_, q)) => {
                    let out = if q.len() > 1 {
                        q.pop_front().unwrap()
                    } else {
                        q.front().unwrap().clone()
                    };
                    out.map_err(|e| anyhow::anyhow!("{e}"))
                }
                None => Err(anyhow::anyhow!("unscripted command: {cmd}")),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn ran(&self, needle: &str) -> bool {
            self.calls().iter().any(|c| c.contains(needle))
        }

        /// Index of the first call containing `needle`, for ordering asserts.
        fn position(&self, needle: &str) -> Option<usize> {
            self.calls().iter().position(|c| c.contains(needle))
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for FakeHost {
        fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph {}", args.join(" "))) }
        }

        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.answer(&format!("ceph {}", args.join(" ")))?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            }
        }

        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph-volume {}", args.join(" "))) }
        }

        fn kubectl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl {}", args.join(" "))) }
        }

        fn kubectl_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.answer(&format!("kubectl {}", args.join(" ")))?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            }
        }

        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let out = me.answer(&format!("systemctl {}", args.join(" ")));
                Ok(CommandOutput {
                    success: out.is_ok(),
                    stdout: out.unwrap_or_default(),
                    stderr: String::new(),
                })
            }
        }

        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let out = me.answer(&format!("{bin} {}", args.join(" ")));
                Ok(CommandOutput {
                    success: out.is_ok(),
                    stdout: out.unwrap_or_default(),
                    stderr: String::new(),
                })
            }
        }
    }
    #[tokio::test]
    async fn create_osd_the_happy_path_confirms_and_starts_the_unit() {
        let host = FakeHost::new()
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .ok("ceph-volume lvm create", "")
            .ok(
                "ceph-volume lvm list",
                r#"{"5":[{"devices":["/dev/sdb"],"tags":{"ceph.cluster_fsid":"11111111-2222-3333-4444-555555555555"}}]}"#,
            )
            .ok("systemctl start yolab-ceph-osd@5.service", "");

        create_osd(&host, "disk-happy", "/dev/sdb").await;

        assert_eq!(progress_of("disk-happy").phase, phase::ACTIVE);
        assert!(host.ran("start yolab-ceph-osd@5.service"));
        assert!(
            !host.ran("lvm zap"),
            "a blank disk must never be zapped: {:?}",
            host.calls()
        );
    }

    /// Regression test for a8ba132: gating the erase on the create's `Err`
    /// missed the actual failure mode, because a disk still carrying another
    /// cluster's LVM stack makes `ceph-volume lvm create` exit 0 having done
    /// nothing. The fix moved the erase before the create attempt — this pins
    /// that ordering so it cannot silently move back to "after a failure".
    #[tokio::test]
    async fn create_osd_erases_a_foreign_disk_before_attempting_create() {
        let host = FakeHost::new()
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph osd ls", "[]")
            .ok(
                "ceph-volume lvm list",
                r#"{"1":[{"devices":["/dev/sdc"],"tags":{"ceph.cluster_fsid":"99999999-8888-7777-6666-555555555555"}}]}"#,
            )
            .ok("ceph-volume lvm zap", "")
            .ok("ceph-volume lvm create", "");

        create_osd(&host, "disk-foreign", "/dev/sdc").await;

        let zap_at = host
            .position("lvm zap")
            .expect("a foreign disk must be zapped");
        let create_at = host
            .position("lvm create")
            .expect("create must still be attempted after the erase");
        assert!(
            zap_at < create_at,
            "zap must happen before create, not after a failure: {:?}",
            host.calls()
        );
        assert!(
            host.ran("lvm zap --destroy"),
            "a foreign stack is a volume group and must be removed, not just its contents: {:?}",
            host.calls()
        );
    }

    /// The system LV keeps a previous cluster's BlueStore signature after
    /// disko recreates it, because LVM does not zero reused extents. Unlike
    /// the foreign-disk case this really does fail the first create, and the
    /// fix must retry exactly once rather than loop.
    #[tokio::test]
    async fn create_osd_retries_once_after_a_stale_bluestore_signature() {
        let host = FakeHost::new()
            .ok(
                "ceph fsid",
                r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#,
            )
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .fail(
                "ceph-volume lvm create",
                "RuntimeError: Device /dev/mapper/pool-ceph has bluestore signature.",
            )
            .ok("ceph-volume lvm create", "")
            .ok("ceph-volume lvm zap", "");

        create_osd(&host, "disk-stale", "/dev/mapper/pool-ceph").await;

        let creates = host
            .calls()
            .iter()
            .filter(|c| c.contains("lvm create"))
            .count();
        assert_eq!(
            creates,
            2,
            "must retry exactly once, not loop: {:?}",
            host.calls()
        );
        assert!(
            !host.ran("lvm zap --destroy"),
            "the system LV belongs to disko and must never be --destroy'd: {:?}",
            host.calls()
        );
    }

    #[test]
    fn retry_backoff_grows_exponentially_and_is_capped() {
        assert_eq!(retry_backoff(1), std::time::Duration::from_secs(30));
        assert_eq!(retry_backoff(2), std::time::Duration::from_secs(60));
        assert_eq!(retry_backoff(3), std::time::Duration::from_secs(120));
        assert_eq!(retry_backoff(7), std::time::Duration::from_secs(600));
        assert_eq!(
            retry_backoff(20),
            std::time::Duration::from_secs(600),
            "must stay capped, not overflow or keep growing forever"
        );
    }

    #[test]
    fn retry_due_is_always_true_on_the_first_attempt() {
        // None means "never attempted, or the timestamp did not survive a
        // restart" — either way this must never be the reason attempt 1 waits.
        assert!(retry_due(0, None));
        assert!(retry_due(1, None));
    }

    #[test]
    fn retry_due_blocks_until_the_backoff_elapses() {
        // attempts=3 -> retry_backoff(3) == 120s.
        assert!(!retry_due(3, Some(std::time::Duration::from_secs(100))));
        assert!(retry_due(3, Some(std::time::Duration::from_secs(120))));
        assert!(retry_due(3, Some(std::time::Duration::from_secs(121))));
    }

    /// Regression test for the Easystore sitting at "attempt 22": before this,
    /// nothing stood between a permanently-stuck disk and a fresh
    /// ceph-volume invocation every single 30s tick, forever.
    #[test]
    fn plan_create_waits_rather_than_hammering_a_disk_that_just_failed() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            3,
            Some(std::time::Duration::from_secs(5)),
        );
        assert_eq!(plan, CreatePlan::Waiting);
    }

    #[test]
    fn plan_create_retries_once_the_backoff_has_elapsed() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            3,
            Some(std::time::Duration::from_secs(120)),
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/sdb".to_string()
            }
        );
    }

    /// Regression test for 240cdde: a purge that reports success is not
    /// proof the OSD actually left the map, and wiping on that trust alone
    /// is how a disk Ceph still tracks as live gets erased.
    #[tokio::test]
    async fn purge_wipes_the_disk_once_the_osd_is_confirmed_gone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph-volume lvm list", "{}")
            .ok(
                "ceph osd df tree",
                r#"{"nodes":[{"id":7,"type":"osd","status":"down","crush_weight":0.5,"reweight":0.0,"kb":0}]}"#,
            )
            .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[7]}"#)
            .ok(
                "systemctl show -p ActiveState --value yolab-ceph-osd@7.service",
                "inactive",
            )
            .ok("ceph osd purge", "purged osd.7")
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm zap", "");

        let meta = HashMap::from([("disk-purge".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-purge".to_string(), "OFF".to_string())]);
        let disk_to_osd = HashMap::from([("disk-purge".to_string(), 7i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd)).await;

        assert!(
            host.ran("lvm zap"),
            "a confirmed purge must wipe the disk: {:?}",
            host.calls()
        );
        assert_eq!(progress_of("disk-purge").phase, phase::REMOVABLE);
    }

    /// The other half of 240cdde: `ceph osd ls` still listing the id after a
    /// purge that reported success must leave the disk alone, not wipe it.
    #[tokio::test]
    async fn purge_never_wipes_when_the_osd_is_still_listed_afterward() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph-volume lvm list", "{}")
            .ok(
                "ceph osd df tree",
                r#"{"nodes":[{"id":9,"type":"osd","status":"down","crush_weight":0.5,"reweight":0.0,"kb":0}]}"#,
            )
            .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[9]}"#)
            .ok(
                "systemctl show -p ActiveState --value yolab-ceph-osd@9.service",
                "inactive",
            )
            .ok("ceph osd purge", "purged osd.9")
            .ok("ceph osd ls", "[9]");

        let meta = HashMap::from([("disk-stuck".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-stuck".to_string(), "OFF".to_string())]);
        let disk_to_osd = HashMap::from([("disk-stuck".to_string(), 9i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd)).await;

        assert!(
            !host.ran("lvm zap"),
            "a purge that is not confirmed gone must never wipe the disk: {:?}",
            host.calls()
        );
        assert_eq!(progress_of("disk-stuck").phase, phase::REMOVING);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_reports_unknown_and_never_touches_a_disk() {
        let host = RecordingHost::default();
        let meta = HashMap::from([("disk-a".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-a".to_string(), "ON".to_string())]);
        let disk_to_osd = HashMap::new();

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd)).await;

        assert_eq!(progress_of("disk-a").phase, phase::UNKNOWN);
        assert_eq!(*host.ceph_volume_calls.lock().unwrap(), 0);
    }

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
    /// `disk_meta` decides `is_our_osd` by comparing a disk's BlueStore fsid
    /// against `cluster_fsid().await.unwrap_or_default()` — which is `""`
    /// whenever Ceph is unreachable. If `bluestore_fsid` could ever return
    /// `Some("")`, that empty string would compare *equal*, every foreign disk
    /// would read as one of ours, and `refuse_osd_creation` would wave it
    /// through to be wiped. The only thing standing between that and someone
    /// else's data is this function returning false for the empty string.
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

    // ── disk_meta / Ownership ─────────────────────────────────────────────────

    #[test]
    fn a_label_matching_our_cluster_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Ours
        );
    }

    #[test]
    fn a_label_from_another_cluster_is_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(THEIRS));
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Foreign
        );
    }

    /// With an unknown cluster fsid our own disk is indistinguishable from a
    /// stranger's. It is reported Unknown rather than guessed either way, and
    /// Unknown refuses creation exactly like Foreign — the conservative
    /// reading, and the one that makes the UI ask rather than assume.
    #[test]
    fn a_label_we_cannot_attribute_is_unknown_and_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        let d = disk_meta(&dev, "", DiskFlags::default());
        assert_eq!(d.ownership, Ownership::Unknown);
        assert!(refuse_osd_creation(&d).is_some());
    }

    #[test]
    fn an_unlabelled_disk_is_blank() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &vec![0u8; 4096]);
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Blank
        );
    }

    /// The published shape is a contract with the Storage page, which reads
    /// is_our_osd, foreign_ceph, is_loop, osd_id, size_bytes, device, model,
    /// has_partitions, mounted, phase, message and attempts. Ownership is one
    /// enum internally but has to keep arriving as the two booleans the UI
    /// branches on, and Unknown has to look like Foreign on the wire or a disk
    /// nobody can attribute would render as safe to erase.
    #[test]
    fn the_wire_shape_the_ui_reads_is_unchanged() {
        let base = Disk {
            device: "sdb".into(),
            model: "easystore".into(),
            size_bytes: 1000,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        };

        let v = base.to_value();
        assert_eq!(v["device"], json!("sdb"));
        assert_eq!(v["model"], json!("easystore"));
        assert_eq!(v["size_bytes"], json!(1000));
        assert_eq!(v["is_our_osd"], json!(false));
        assert_eq!(v["foreign_ceph"], json!(false));
        assert_eq!(v["has_partitions"], json!(false));
        assert_eq!(v["mounted"], json!(false));
        assert_eq!(v["osd_id"], json!(null));
        assert!(
            v.get("is_loop").is_none(),
            "absent on a normal disk, as before"
        );

        for (own, ours, foreign) in [
            (Ownership::Ours, true, false),
            (Ownership::Foreign, false, true),
            (Ownership::Unknown, false, true),
            (Ownership::Blank, false, false),
        ] {
            let v = Disk {
                ownership: own,
                ..base.clone()
            }
            .to_value();
            assert_eq!(v["is_our_osd"], json!(ours), "{own:?}");
            assert_eq!(v["foreign_ceph"], json!(foreign), "{own:?}");
        }

        let sys = Disk {
            is_loop: true,
            ..base.clone()
        }
        .to_value();
        assert_eq!(sys["is_loop"], json!(true));

        let running = Disk {
            osd_id: Some(3),
            progress: Some(DiskProgress {
                phase: phase::ACTIVE.into(),
                message: "In use".into(),
                attempts: 2,
                orphan_osd_id: None,
                last_attempt: None,
            }),
            ..base.clone()
        }
        .to_value();
        assert_eq!(running["osd_id"], json!(3));
        assert_eq!(running["phase"], json!(phase::ACTIVE));
        assert_eq!(running["message"], json!("In use"));
        assert_eq!(running["attempts"], json!(2));
    }

    /// An empty phase used to mean "write no phase key at all". Preserved so a
    /// disk that has never been acted on does not gain a blank phase the UI
    /// would have to special-case.
    #[test]
    fn a_disk_with_no_progress_publishes_no_phase() {
        let v = Disk {
            device: "sdb".into(),
            model: String::new(),
            size_bytes: 0,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: Some(DiskProgress::default()),
        }
        .to_value();
        assert!(v.get("phase").is_none());
    }

    // ── disk_id ───────────────────────────────────────────────────────────────

    // ── Migrating to hardware ids ─────────────────────────────────────────────
    //
    // disk_id changed shape in this release. Without carrying records across, every
    // disk looks new on the first tick after updating — and new means OFF, which for a
    // disk already carrying an OSD means drain, purge, wipe. The release that fixes
    // the accident would have caused it.

    fn recs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_setting_follows_the_disk_onto_its_hardware_id() {
        let desired = recs(&[("node1--dev-sdb", "ON"), ("node1--dev-sda", "OFF")]);
        let current = vec![
            (
                "serial-wwn-0x50014ee214caf529".to_string(),
                "sdb".to_string(),
            ),
            (
                "serial-wwn-0x500a0751191b8afa".to_string(),
                "sda".to_string(),
            ),
        ];
        let out = migrate_disk_records("node1", &current, &desired).write;
        assert_eq!(
            out.get("serial-wwn-0x50014ee214caf529").map(String::as_str),
            Some("ON")
        );
        assert_eq!(
            out.get("serial-wwn-0x500a0751191b8afa").map(String::as_str),
            Some("OFF")
        );
    }

    /// OFF has to travel too. Silently switching a deliberately-off disk back ON would
    /// hand a disk to Ceph that its owner did not offer.
    #[test]
    fn a_disk_switched_off_stays_off() {
        let desired = recs(&[("node1--dev-sdb", "OFF")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        assert_eq!(
            migrate_disk_records("node1", &current, &desired)
                .write
                .get("serial-wwn-0xabc")
                .map(String::as_str),
            Some("OFF")
        );
    }

    /// Already migrated: leave it alone rather than overwrite a newer decision with an
    /// older one that happens to still be in the map.
    #[test]
    fn an_existing_record_is_never_overwritten() {
        let desired = recs(&[("node1--dev-sdb", "ON"), ("serial-wwn-0xabc", "OFF")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        assert!(migrate_disk_records("node1", &current, &desired)
            .write
            .is_empty());
    }

    /// Hardware that publishes no id keeps the kernel-name key it already had; there
    /// is nothing to carry, and carrying it onto itself would be a no-op write.
    #[test]
    fn a_disk_still_identified_by_kernel_name_is_left_as_is() {
        let desired = recs(&[("node1--dev-sdb", "ON")]);
        let current = vec![("dev-sdb".to_string(), "sdb".to_string())];
        assert!(migrate_disk_records("node1", &current, &desired)
            .write
            .is_empty());
    }

    /// A genuinely new disk has no old record, and must not inherit one from whichever
    /// letter it happened to land on.
    #[test]
    fn a_new_disk_inherits_nothing() {
        let desired = recs(&[("node1--dev-sdb", "ON")]);
        // A different disk, now at sdc. sdb's record is not its to take.
        let current = vec![("serial-wwn-0xnew".to_string(), "sdc".to_string())];
        assert!(migrate_disk_records("node1", &current, &desired)
            .write
            .is_empty());
    }

    /// Records belong to a node. Another machine's disk must not pick one up.
    #[test]
    fn records_do_not_cross_between_machines() {
        let desired = recs(&[("node3--dev-sdb", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        assert!(migrate_disk_records("node1", &current, &desired)
            .write
            .is_empty());
    }

    /// The exact situation that drained a live disk: a stale kernel-name orphan saying
    /// OFF, sitting beside the real record saying ON. The newest, most specific
    /// predecessor has to win — a `dev-*` key belongs to a letter, not to a disk, and
    /// stale ones outlive the hardware that made them.
    #[test]
    fn a_stale_kernel_name_record_never_beats_the_hardware_one() {
        let desired = recs(&[
            ("node1--dev-sdc", "OFF"),              // orphan from the old scheme
            ("node1--serial-wwn-0x50014ee2", "ON"), // the record that means it
        ]);
        let current = vec![("serial-wwn-0x50014ee2".to_string(), "sdc".to_string())];
        let out = migrate_disk_records("node1", &current, &desired).write;
        assert_eq!(
            out.get("serial-wwn-0x50014ee2").map(String::as_str),
            Some("ON"),
            "the live hardware record must win over a stale letter record"
        );
    }

    /// With no hardware record to prefer, the kernel-name one is still the right
    /// answer — that is the upgrade path from the oldest scheme.
    #[test]
    fn the_kernel_name_record_is_still_used_when_it_is_all_there_is() {
        let desired = recs(&[("node1--dev-sdc", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdc".to_string())];
        assert_eq!(
            migrate_disk_records("node1", &current, &desired)
                .write
                .get("serial-wwn-0xabc")
                .map(String::as_str),
            Some("ON")
        );
    }

    // ── Removing what has been superseded ─────────────────────────────────────

    /// Carrying a value forward and leaving the source behind gives one disk two
    /// records that can disagree — which is exactly what happened: an ON record with
    /// the visible toggle, and an OFF record doing the deciding.
    #[test]
    fn the_record_a_value_came_from_is_removed() {
        let desired = recs(&[("node1--serial-wwn-0xabc", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        let m = migrate_disk_records("node1", &current, &desired);
        assert_eq!(
            m.write.get("serial-wwn-0xabc").map(String::as_str),
            Some("ON")
        );
        assert!(m.remove.contains(&"node1--serial-wwn-0xabc".to_string()));
    }

    /// The whole ConfigMap from the cluster this went wrong on, with the easystore
    /// present at sdc. Afterwards there must be exactly one record for that disk.
    #[test]
    fn the_live_configmap_collapses_to_one_record_per_disk() {
        let desired = recs(&[
            ("node1--dev-sda", "OFF"),
            ("node1--dev-sdb", "OFF"),
            ("node1--dev-sdc", "OFF"),
            ("node1--serial-wwn-0x50014ee214caf529", "ON"),
            ("node1--system", "ON"),
            ("node3--system", "ON"),
            ("serial-wwn-0x50014ee214caf529", "OFF"),
        ]);
        let current = vec![
            (
                "serial-wwn-0x50014ee214caf529".to_string(),
                "sdc".to_string(),
            ),
            ("system".to_string(), "dm-1".to_string()),
        ];
        let m = migrate_disk_records("node1", &current, &desired);

        // The node-scoped duplicate goes.
        assert!(m
            .remove
            .contains(&"node1--serial-wwn-0x50014ee214caf529".to_string()));
        // The kernel-name traps go — including the one that started the drain.
        for orphan in ["node1--dev-sda", "node1--dev-sdb"] {
            assert!(m.remove.contains(&orphan.to_string()), "{orphan} must go");
        }
        // system records are untouched: they name a machine's own disk, not a letter.
        assert!(!m.remove.iter().any(|k| k.ends_with("--system")));
    }

    /// A kernel-name record for a disk that IS at that letter is the only identity
    /// that disk has — some enclosures publish nothing — so it stays.
    #[test]
    fn a_kernel_name_record_for_a_present_disk_is_kept() {
        let desired = recs(&[("node1--dev-sdb", "ON")]);
        let current = vec![("dev-sdb".to_string(), "sdb".to_string())];
        let m = migrate_disk_records("node1", &current, &desired);
        assert!(m.remove.is_empty(), "the disk is still there under that id");
    }

    /// Another machine's records are never touched — this node cannot see that
    /// hardware and has no standing to judge it.
    #[test]
    fn another_nodes_records_are_never_removed() {
        let desired = recs(&[("node3--dev-sdb", "ON"), ("node3--system", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        let m = migrate_disk_records("node1", &current, &desired);
        assert!(m.remove.is_empty());
    }

    /// Nothing to do on an already-clean cluster: no writes, no deletes, no patch.
    #[test]
    fn a_settled_cluster_produces_no_changes() {
        let desired = recs(&[("serial-wwn-0xabc", "ON"), ("node1--system", "ON")]);
        let current = vec![
            ("serial-wwn-0xabc".to_string(), "sdb".to_string()),
            ("system".to_string(), "dm-1".to_string()),
        ];
        let m = migrate_disk_records("node1", &current, &desired);
        assert!(m.write.is_empty() && m.remove.is_empty());
    }

    // ── Whether a record belongs to a disk or to a machine ────────────────────

    /// A hardware id names one physical disk, so its record must not be tied to
    /// whichever machine currently holds it — otherwise moving a disk to another node
    /// makes it a stranger, and strangers are OFF, and OFF on a disk carrying an OSD
    /// means wipe.
    #[test]
    fn a_hardware_id_is_not_scoped_to_a_machine() {
        assert_eq!(
            record_key("node1", "serial-wwn-0x50014ee214caf529"),
            "serial-wwn-0x50014ee214caf529"
        );
        // Same disk, different machine, same record.
        assert_eq!(
            record_key("node1", "serial-wwn-0xabc"),
            record_key("node3", "serial-wwn-0xabc")
        );
    }

    /// The reason the prefix existed, and still has to: these ids name a position on a
    /// machine, and positions repeat. node1's sda and node3's sda are different disks.
    #[test]
    fn an_id_that_only_means_something_locally_stays_scoped() {
        assert_eq!(record_key("node1", "dev-sda"), "node1--dev-sda");
        assert_ne!(
            record_key("node1", "dev-sda"),
            record_key("node3", "dev-sda")
        );
        assert_eq!(record_key("node1", "system"), "node1--system");
        assert_ne!(record_key("node1", "system"), record_key("node3", "system"));
    }

    #[test]
    fn only_hardware_ids_count_as_globally_unique() {
        assert!(is_globally_unique_id("serial-wwn-0xabc"));
        assert!(is_globally_unique_id("serial-ata-wdc-wd10"));
        for local in ["dev-sda", "dev-nvme0n1", "system", "", "loop0"] {
            assert!(!is_globally_unique_id(local), "{local} names a position");
        }
    }

    // ── Migration, across both changes ────────────────────────────────────────

    /// Straight from the oldest scheme to the newest, which is what a cluster that
    /// skipped a release actually does.
    #[test]
    fn a_kernel_name_record_migrates_all_the_way_to_a_bare_hardware_key() {
        let desired = recs(&[("node1--dev-sdb", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        let out = migrate_disk_records("node1", &current, &desired).write;
        assert_eq!(out.get("serial-wwn-0xabc").map(String::as_str), Some("ON"));
    }

    /// And from the intermediate shape this release replaces.
    #[test]
    fn a_node_scoped_hardware_record_loses_its_prefix() {
        let desired = recs(&[("node1--serial-wwn-0xabc", "ON")]);
        let current = vec![("serial-wwn-0xabc".to_string(), "sdb".to_string())];
        let out = migrate_disk_records("node1", &current, &desired).write;
        assert_eq!(out.get("serial-wwn-0xabc").map(String::as_str), Some("ON"));
    }

    /// A disk that moves to another machine keeps the setting it already had, which is
    /// the point of the whole change.
    #[test]
    fn a_disk_moved_to_another_node_keeps_its_setting() {
        let desired = recs(&[("serial-wwn-0xabc", "ON")]);
        // Now plugged into node3, at a completely different letter.
        let current = vec![("serial-wwn-0xabc".to_string(), "sdd".to_string())];
        // Nothing to migrate — the record already applies, on any node.
        assert!(migrate_disk_records("node3", &current, &desired)
            .write
            .is_empty());
        assert_eq!(record_key("node3", "serial-wwn-0xabc"), "serial-wwn-0xabc");
    }

    // ── Stable identity ───────────────────────────────────────────────────────
    //
    // A disk was unplugged as sdb, came back as sdc, was registered as a new disk
    // (new disks are OFF), and the reconciler read that OFF as an instruction and
    // wiped the OSD on it. The kernel name was never identity; these pin what is.

    #[test]
    fn a_hardware_id_beats_the_kernel_name() {
        assert_eq!(
            disk_id_from("sdc", Some("wwn-0x50014ee214caf529")),
            "serial-wwn-0x50014ee214caf529".replace(['.', '_'], "-")
        );
    }

    /// The ranking is the point: the same disk publishes several ids, and the one
    /// chosen has to be the same one every time or the disk changes identity between
    /// boots for a different reason.
    #[test]
    fn the_id_ranking_prefers_hardware_identity_over_the_enclosure() {
        let rank = |n: &str| ID_PREFIXES.iter().position(|p| n.starts_with(p));
        // Exactly the three links the disk in question published.
        let wwn = rank("wwn-0x50014ee214caf529").unwrap();
        let ata = rank("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR33").unwrap();
        let usb = rank("usb-WD_easystore_2647_575844324135314C41523333-0:0").unwrap();
        assert!(wwn < ata, "the World Wide Name is the strongest identity");
        assert!(
            ata < usb,
            "a USB id may describe the caddy rather than the disk"
        );
    }

    #[test]
    fn unranked_links_are_ignored() {
        let rank = |n: &str| ID_PREFIXES.iter().position(|p| n.starts_with(p));
        // dm/lvm links point at logical volumes, not at a disk we could claim.
        assert!(rank("dm-name-pool-ceph").is_none());
        assert!(rank("lvm-pv-uuid-3MOvZ3-dMBQ").is_none());
    }

    /// Falling back to the kernel name is still allowed — some enclosures publish
    /// nothing — but it must be visible as the weak case it is, not silently equal to
    /// a hardware id.
    #[test]
    fn the_kernel_name_remains_the_last_resort() {
        assert_eq!(disk_id_from("sdc", None), "dev-sdc");
        assert_eq!(disk_id_from("sdc", Some("")), "dev-sdc");
        assert_eq!(disk_id_from("sdc", Some("  \n ")), "dev-sdc");
    }

    /// The id becomes a ConfigMap key, so a WWN's `0x` and an ATA id's underscores
    /// have to survive sanitising into something still unique per disk.
    #[test]
    fn two_different_disks_never_sanitise_to_the_same_id() {
        let a = disk_id_from("sdb", Some("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR33"));
        let b = disk_id_from("sdc", Some("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR34"));
        assert_ne!(a, b);
        assert!(a.starts_with("serial-"));
    }

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

    fn disk(ownership: Ownership) -> Disk {
        Disk {
            device: "sdb".into(),
            model: String::new(),
            size_bytes: 1_000_000_000,
            is_loop: false,
            ownership,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        }
    }

    fn seen(crush_weight: f64, reweight: f64, kb: u64, up: bool) -> OsdState {
        OsdState {
            crush_weight,
            reweight,
            kb,
            up,
        }
    }

    #[test]
    fn a_freshly_created_osd_is_activated_with_a_weight() {
        let steer = decide_steer(
            true,
            1u64 << 40,
            seen(0.0, 0.0, 0, true),
            0,
            &[],
            "osd",
            None,
        );
        assert_eq!(steer.set_weight, Some(1.0));
        assert!(steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(phase::ACTIVE));
    }

    #[test]
    fn an_on_osd_that_is_down_reports_retrying() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, false), 0, &[], "osd", None);
        assert_eq!(steer.set_weight, None);
        assert!(!steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(phase::RETRYING));
    }

    #[test]
    fn a_weighted_in_osd_that_is_up_needs_no_steering() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        assert_eq!(steer.set_weight, None);
        assert!(!steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(phase::ACTIVE));
    }

    #[test]
    fn an_off_disk_still_in_is_marked_out() {
        let steer = decide_steer(false, 0, seen(1.0, 1.0, 0, false), 0, &[], "osd", None);
        assert!(steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(phase::DRAINING));
        assert!(steer.message.is_some());
    }

    #[test]
    fn an_off_disk_already_out_needs_purge() {
        let steer = decide_steer(false, 0, seen(1.0, 0.0, 0, false), 0, &[], "osd", None);
        assert!(steer.needs_purge);
        assert!(!steer.mark_out);
        assert!(!steer.mark_in);
        assert_eq!(steer.phase, None);
        assert_eq!(steer.message, None);
    }

    #[test]
    fn a_purge_is_confirmed_only_when_the_id_is_gone() {
        assert!(purge_confirmed(Some(&[1, 2]), 3));
        assert!(!purge_confirmed(Some(&[1, 2, 3]), 3));
        assert!(!purge_confirmed(None, 3));
    }

    #[test]
    fn a_unit_is_stopped_only_in_a_definitely_dead_state() {
        assert!(is_stopped(Some("inactive")));
        assert!(is_stopped(Some("failed")));
        assert!(!is_stopped(Some("active")));
        assert!(!is_stopped(Some("activating")));
        assert!(!is_stopped(None));
    }

    #[test]
    fn a_unit_is_running_when_active_or_coming_up() {
        assert!(is_running(Some("active")));
        assert!(is_running(Some("activating")));
        assert!(is_running(Some("reloading")));
        assert!(!is_running(Some("inactive")));
        assert!(!is_running(None));
    }

    #[test]
    fn a_successful_steer_reports_its_own_phase() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        let (phase, _) = steer_report(&steer, true);
        assert_eq!(phase, phase::ACTIVE);
    }

    #[test]
    fn a_failed_steer_reports_retrying_not_active() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        let (phase, message) = steer_report(&steer, false);
        assert_eq!(phase, phase::RETRYING);
        assert!(message.contains("keep trying"));
    }

    #[test]
    fn a_wanted_disk_absent_from_the_node_is_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "ON".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_node_scoped_wanted_disk_absent_is_unplugged() {
        let desired = HashMap::from([("node1--dev-sdb".to_string(), "USING".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_present_disk_is_not_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "ON".to_string())]);
        let meta = HashMap::from([("serial-abc".to_string(), disk(Ownership::Blank))]);
        assert!(!any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn an_off_disk_is_not_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "OFF".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(!any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_blank_disk_may_be_turned_into_an_osd() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
    }

    /// A healthy OSD of ours that Ceph momentarily fails to report must never be
    /// re-created over — that is destroying live data in response to a transient
    /// command failure.
    #[test]
    fn our_own_osd_is_never_recreated_over() {
        assert!(
            refuse_osd_creation(&disk(Ownership::Ours)).is_some(),
            "a disk carrying our own BlueStore label must never be handed to ceph-volume create"
        );
    }

    #[test]
    fn another_clusters_disk_is_refused() {
        assert!(refuse_osd_creation(&disk(Ownership::Foreign)).is_some());
    }

    /// The state the boolean pair could not express: a label was read but the
    /// cluster fsid was unknown, so we cannot tell whose it is. It must refuse.
    #[test]
    fn a_disk_of_unknown_ownership_is_refused() {
        assert!(refuse_osd_creation(&disk(Ownership::Unknown)).is_some());
    }

    /// Blank is now the only ownership that permits a wipe, and it is reached
    /// only when the superblock was read and carried no fsid at all. The old
    /// `unwrap_or(false)` shape defaulted a missing key into this state; that is
    /// no longer representable.
    #[test]
    fn blank_is_the_only_ownership_that_permits_creation() {
        for o in [Ownership::Ours, Ownership::Foreign, Ownership::Unknown] {
            assert!(
                refuse_osd_creation(&disk(o)).is_some(),
                "{o:?} must never be handed to ceph-volume create"
            );
        }
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
    }

    #[test]
    fn an_entry_without_a_device_is_refused() {
        let d = Disk {
            device: String::new(),
            ..disk(Ownership::Blank)
        };
        assert!(refuse_osd_creation(&d).is_some());
    }

    /// A label sniff that reads nothing is not evidence a disk is empty. Blank
    /// only means "no fsid in the superblock", so mounted and has_partitions
    /// stay load-bearing on top of it.
    #[test]
    fn a_blank_but_occupied_disk_is_still_refused() {
        let mounted = Disk {
            mounted: true,
            ..disk(Ownership::Blank)
        };
        let partitioned = Disk {
            has_partitions: true,
            ..disk(Ownership::Blank)
        };
        assert!(refuse_osd_creation(&mounted).is_some());
        assert!(refuse_osd_creation(&partitioned).is_some());
    }

    // ── mark_known_osds ───────────────────────────────────────────────────────

    /// ceph-volume wraps a raw disk in LVM, so /dev/sdb has no BlueStore label
    /// at offset 0 and the sniff reads Blank — even though Ceph knows it as
    /// osd.1. The UI renders ON + connected + !is_our_osd as "Setting up…", so a
    /// fully-backfilled OSD pulsed forever.
    #[test]
    fn a_disk_ceph_knows_about_is_marked_as_ours() {
        let mut meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Blank))]);

        mark_known_osds(&mut meta, &HashMap::from([("dev-sdb".to_string(), 1i64)]));

        assert_eq!(meta["dev-sdb"].osd_id, Some(1));
        assert_eq!(
            meta["dev-sdb"].ownership,
            Ownership::Ours,
            "an OSD id from ceph-volume is authoritative over a missing on-disk label"
        );
    }

    /// A disk Ceph claims cannot also belong to a stranger. Leaving it Foreign
    /// would make the UI offer to erase an OSD holding live data.
    #[test]
    fn a_known_osd_is_never_left_marked_foreign() {
        let mut meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Foreign))]);
        mark_known_osds(&mut meta, &HashMap::from([("dev-sdb".to_string(), 4i64)]));
        assert_eq!(meta["dev-sdb"].ownership, Ownership::Ours);
    }

    /// Disks Ceph does not know about keep whatever the label sniff decided —
    /// that is what still catches a genuine foreign disk.
    #[test]
    fn disks_ceph_does_not_know_are_left_alone() {
        let mut meta = HashMap::from([("dev-sdc".to_string(), disk(Ownership::Foreign))]);
        mark_known_osds(&mut meta, &HashMap::new());
        assert_eq!(meta["dev-sdc"].ownership, Ownership::Foreign);
        assert_eq!(meta["dev-sdc"].osd_id, None);
    }

    /// An id for a disk no longer in the inventory must not resurrect an entry.
    #[test]
    fn an_id_for_an_absent_disk_adds_nothing() {
        let mut meta: HashMap<String, Disk> = HashMap::new();
        mark_known_osds(&mut meta, &HashMap::from([("dev-gone".to_string(), 9i64)]));
        assert!(meta.is_empty());
    }

    // ── refuse_osd_creation ───────────────────────────────────────────────────

    #[test]
    fn a_mounted_disk_is_refused_with_a_reason() {
        let d = Disk {
            device: "sda".into(),
            mounted: true,
            ..disk(Ownership::Blank)
        };
        let msg = refuse_osd_creation(&d).expect("a mounted disk must be refused");
        assert!(msg.contains("using this disk"), "{msg}");
    }

    #[test]
    fn a_partitioned_disk_is_refused_until_it_is_erased() {
        let d = Disk {
            has_partitions: true,
            ..disk(Ownership::Blank)
        };
        let msg = refuse_osd_creation(&d).expect("a disk with data must be refused");
        // It has to name the way out, not just the problem: erasing is the only
        // thing that moves this disk forward, and the page offers a button for it.
        assert!(msg.contains("Erase"), "{msg}");
    }

    #[test]
    fn a_blank_unmounted_disk_is_accepted() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
    }

    /// Ownership is checked before the shape checks, so a foreign disk is
    /// refused for being foreign rather than for happening to be partitioned.
    #[test]
    fn foreign_ceph_still_wins_over_the_new_checks() {
        let msg =
            refuse_osd_creation(&disk(Ownership::Foreign)).expect("a foreign disk must be refused");
        assert!(msg.contains("another storage system"), "{msg}");
    }

    // ── scan_devices filtering ────────────────────────────────────────────────

    /// The OS disk must not appear as a second row alongside "System disk".
    /// Listing partitioned disks made it show up twice — once as itself, once as
    /// the Ceph volume carved out of it.
    #[test]
    fn a_mounted_disk_is_not_offered_as_storage() {
        let v = json!({"name": "sda", "type": "disk", "children": [
            {"name": "sda1", "type": "part", "mountpoints": ["/boot"]}
        ]});
        assert!(parse_disk_flags(&v).mounted, "must be seen as mounted");
    }

    // ── refuse_osd_creation speaks to people ──────────────────────────────────
    //
    // These strings are rendered verbatim on the Storage page. The test is that
    // they contain no vocabulary the person who plugged the disk in would have
    // to look up.

    #[test]
    fn refusal_reasons_carry_no_jargon() {
        let cases = [
            disk(Ownership::Foreign),
            disk(Ownership::Unknown),
            disk(Ownership::Ours),
            Disk {
                mounted: true,
                ..disk(Ownership::Blank)
            },
            Disk {
                has_partitions: true,
                ..disk(Ownership::Blank)
            },
            Disk {
                device: String::new(),
                ..disk(Ownership::Blank)
            },
        ];
        // Every internal term that used to appear in these messages.
        const JARGON: [&str; 8] = [
            "BlueStore",
            "OSD",
            "ceph",
            "Ceph",
            "LVM",
            "device path",
            "metadata",
            "superblock",
        ];
        for c in cases {
            let msg = refuse_osd_creation(&c).expect("every case must refuse");
            for word in JARGON {
                assert!(
                    !msg.contains(word),
                    "refusal message leaks {word:?} to the user: {msg}"
                );
            }
            assert!(
                msg.ends_with('.'),
                "shown as a sentence to a person, so it needs to read as one: {msg}"
            );
        }
    }

    // ── parse_osd_metadata ────────────────────────────────────────────────────
    //
    // The mon-side fallback for the disk→OSD map. It only matters when
    // ceph-volume is unavailable, which is exactly when nobody is watching, so
    // the shape of `ceph osd metadata` is pinned here rather than discovered.

    fn meta_json() -> serde_json::Value {
        json!([
            {"id": 0, "hostname": "node1", "devices": "sda",
             "bluestore_bdev_dev_node": "/dev/dm-1"},
            {"id": 1, "hostname": "node1", "devices": "sdb",
             "bluestore_bdev_dev_node": "/dev/dm-2"},
            {"id": 2, "hostname": "node3", "devices": "sda",
             "bluestore_bdev_dev_node": "/dev/dm-9"}
        ])
    }

    #[test]
    fn osd_metadata_returns_only_this_hosts_osds() {
        let got = parse_osd_metadata(&meta_json(), "node1");
        let ids: Vec<i64> = got.iter().map(|(_, id)| *id).collect();
        assert!(ids.contains(&0) && ids.contains(&1));
        assert!(!ids.contains(&2), "osd.2 belongs to another machine");
    }

    /// Both identities, for the same reason parse_lvm_list collects both: our
    /// inventory holds the physical disk for a raw OSD and the volume path for
    /// an LVM-backed one, and which of the two appears depends on the disk.
    #[test]
    fn osd_metadata_reports_the_device_and_the_volume() {
        let got = parse_osd_metadata(&meta_json(), "node1");
        assert!(got.contains(&("sda".to_string(), 0)));
        assert!(got.contains(&("/dev/dm-1".to_string(), 0)));
    }

    #[test]
    fn osd_metadata_splits_a_multi_device_osd() {
        let v = json!([{"id": 4, "hostname": "n", "devices": "sdb,sdc"}]);
        let got = parse_osd_metadata(&v, "n");
        assert!(got.contains(&("sdb".to_string(), 4)));
        assert!(got.contains(&("sdc".to_string(), 4)));
    }

    /// Ceph writes "unknown" rather than omitting the field when it has no
    /// device node. Treating that as a path would map a real OSD onto a disk
    /// called "unknown" — which matches nothing, silently losing the OSD.
    #[test]
    fn osd_metadata_ignores_placeholder_device_nodes() {
        let v = json!([{"id": 5, "hostname": "n", "devices": "",
                        "bluestore_bdev_dev_node": "unknown"}]);
        assert!(parse_osd_metadata(&v, "n").is_empty());
    }

    /// An unrecognisable answer must yield nothing, so the caller keeps treating
    /// the map as unknown instead of acting on a half-parsed one.
    #[test]
    fn osd_metadata_yields_nothing_from_a_shape_it_does_not_understand() {
        assert!(parse_osd_metadata(&json!({}), "n").is_empty());
        assert!(parse_osd_metadata(&json!([]), "n").is_empty());
        assert!(parse_osd_metadata(&json!([{"hostname": "n"}]), "n").is_empty());
    }

    // ── drain_targets_remaining / drain_message ───────────────────────────────
    //
    // The case these exist for was seen live: three disks, three copies, one
    // switched off. Ceph reported 49 active+clean+remapped, 33% of objects
    // misplaced, no backfill running, and EBUSY on safe-to-destroy — while the
    // page said "do not unplug it until this finishes". It could not finish.

    fn osd(id: i64, up: bool, reweight: f64) -> Value {
        json!({"id": id, "type": "osd",
               "status": if up { "up" } else { "down" }, "reweight": reweight})
    }
    fn host(name: &str, children: Vec<i64>) -> Value {
        json!({"type": "host", "name": name, "children": children})
    }

    #[test]
    fn other_usable_osds_are_counted_for_the_osd_domain() {
        let nodes = vec![osd(0, true, 1.0), osd(1, true, 0.0), osd(2, true, 1.0)];
        // Leaving osd.1: osd.0 and osd.2 remain.
        assert_eq!(drain_targets_remaining(&nodes, 1, "osd"), 2);
    }

    /// A down or already-out disk cannot receive anything, so it is not a place
    /// to put a copy — counting it would promise a drain that cannot happen.
    #[test]
    fn down_and_out_osds_are_not_places_to_put_a_copy() {
        let nodes = vec![osd(0, false, 1.0), osd(1, true, 1.0), osd(2, true, 0.0)];
        assert_eq!(drain_targets_remaining(&nodes, 1, "osd"), 0);
    }

    /// With the host domain, two disks in one machine are ONE place — copies
    /// must land on distinct machines.
    #[test]
    fn the_host_domain_counts_machines_not_disks() {
        let nodes = vec![
            host("node1", vec![0, 1]),
            osd(0, true, 1.0),
            osd(1, true, 1.0),
            host("node2", vec![2]),
            osd(2, true, 1.0),
        ];
        assert_eq!(
            drain_targets_remaining(&nodes, 0, "host"),
            2,
            "node1 still has osd.1"
        );
        // Emptying the only disk on node2 removes that machine as a target.
        assert_eq!(drain_targets_remaining(&nodes, 2, "host"), 1);
    }

    /// THE LIVE FAILURE. Three copies, three disks, one leaving: two places for
    /// three copies. The message must say so and name both ways out.
    #[test]
    fn a_drain_with_nowhere_to_go_says_so_and_says_what_to_do() {
        let m = drain_message(2, Some(3));
        assert!(m.contains("cannot be emptied"), "{m}");
        assert!(m.contains("Lower the number of copies to 2"), "{m}");
        assert!(m.contains("add another disk"), "{m}");
        assert!(
            !m.contains("until this finishes"),
            "must not promise completion it cannot deliver: {m}"
        );
    }

    #[test]
    fn a_drain_that_can_finish_says_do_not_unplug() {
        let m = drain_message(3, Some(2));
        assert!(m.contains("Do not unplug"), "{m}");
        assert!(!m.contains("cannot be emptied"), "{m}");
    }

    #[test]
    fn no_other_disk_at_all_is_its_own_message() {
        let m = drain_message(0, Some(1));
        assert!(m.contains("no other disk"), "{m}");
    }

    /// An unreadable policy must not produce a confident sentence either way.
    #[test]
    fn an_unknown_copy_count_promises_nothing_it_cannot_check() {
        let m = drain_message(2, None);
        assert!(!m.contains("cannot be emptied"), "{m}");
        assert!(!m.contains("until this finishes"), "{m}");
    }

    // ── plan_tick: the two guards that stand in front of a disk wipe ──────────
    //
    // Both arms are "do nothing". That is the whole point: the failure these
    // prevent is not an error, it is a confident wrong answer. Silence from
    // Ceph, or an unreadable ceph-volume, both look exactly like "no disk here
    // carries an OSD" — and the reconciler's response to that is
    // `ceph-volume lvm create`, which destroys whatever the disk held.

    #[test]
    fn an_unreachable_cluster_does_nothing() {
        assert_eq!(plan_tick(false, true), TickPlan::Unreachable);
    }

    #[test]
    fn an_unreadable_osd_map_does_nothing() {
        assert_eq!(plan_tick(true, false), TickPlan::UnknownOsdMap);
    }

    /// Order matters only for the message; both refuse to act. Pinned so a
    /// reordering cannot quietly turn the outer guard into the inner one.
    #[test]
    fn an_unreachable_cluster_is_reported_before_an_unreadable_map() {
        assert_eq!(plan_tick(false, false), TickPlan::Unreachable);
    }

    #[test]
    fn a_reachable_cluster_with_a_readable_map_proceeds() {
        assert_eq!(plan_tick(true, true), TickPlan::Proceed);
    }

    // ── plan_create: the decision that turns somebody's disk into an OSD ─────

    fn osds(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn blank_disk(device: &str) -> Disk {
        Disk {
            device: device.into(),
            model: String::new(),
            size_bytes: 1_000_000_000,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        }
    }

    #[test]
    fn a_disk_switched_on_with_no_osd_is_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/sdb".to_string()
            }
        );
    }

    /// "USING" is the same intent as "ON" — it is what the page writes once a
    /// disk is actually carrying data. Reading it as "not ON" would make the
    /// reconciler try to create a second OSD on a disk already holding one.
    #[test]
    fn using_counts_as_on() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "USING")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert!(matches!(plan, CreatePlan::Create { .. }));
    }

    #[test]
    fn a_disk_switched_off_is_never_created() {
        for state in ["OFF", "", "on", "true", "1"] {
            let plan = plan_create(
                "node1",
                "dev-sdb",
                &blank_disk("sdb"),
                &recs(&[("node1--dev-sdb", state)]),
                &osds(&[]),
                false,
                0,    // attempts
                None, // since_last_attempt
            );
            assert_eq!(plan, CreatePlan::Skip, "state {state:?} must not create");
        }
    }

    /// An absent record is the state of every disk the moment it is plugged in.
    /// Defaulting that to ON would wipe a stranger's disk on sight.
    #[test]
    fn a_disk_with_no_record_at_all_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    /// Records are node-scoped. Another machine switching its disk on must not
    /// reach across and provision this one.
    #[test]
    fn another_nodes_record_never_creates_on_this_node() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node2--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    /// The disk already carries an OSD. Creating a second one over it is the
    /// exact accident the `disk_to_osd` guard in plan_tick exists to prevent,
    /// and this is the same refusal one level down.
    #[test]
    fn a_disk_that_already_has_an_osd_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[("dev-sdb", 3)]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    /// ceph-volume takes up to ten minutes. The tick is 30s, so without this
    /// the same disk gets twenty creates in flight at once — which is how
    /// invocations used to stack up.
    #[test]
    fn a_create_already_in_flight_is_not_started_again() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            true,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    /// In-flight is checked before refusal, so a disk that would be blocked
    /// reports Skip rather than flapping its phase to BLOCKED mid-create.
    #[test]
    fn an_in_flight_create_wins_over_a_refusal() {
        let mounted = Disk {
            mounted: true,
            ..blank_disk("sdb")
        };
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &mounted,
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            true,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    // Everything refuse_osd_creation rejects must come back as Blocked, never
    // as Create. These are the disks with something on them.

    #[test]
    fn a_mounted_disk_switched_on_is_blocked_not_created() {
        let plan = plan_create(
            "node1",
            "dev-sda",
            &Disk {
                mounted: true,
                ..blank_disk("sda")
            },
            &recs(&[("node1--dev-sda", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert!(matches!(plan, CreatePlan::Blocked(_)), "{plan:?}");
    }

    #[test]
    fn a_partitioned_disk_switched_on_is_blocked_not_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &Disk {
                has_partitions: true,
                ..blank_disk("sdb")
            },
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert!(matches!(plan, CreatePlan::Blocked(_)), "{plan:?}");
    }

    /// A disk carrying another cluster's BlueStore label. Wiping it is somebody
    /// else's data loss.
    #[test]
    fn a_foreign_cluster_disk_switched_on_is_blocked_not_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &Disk {
                ownership: Ownership::Foreign,
                ..blank_disk("sdb")
            },
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert!(matches!(plan, CreatePlan::Blocked(_)), "{plan:?}");
    }

    /// An empty device means the inventory did not identify this disk. Guessing
    /// a path from a disk id would be a path to somewhere, and ceph-volume would
    /// wipe whatever is at it.
    ///
    /// The missing-key and null variants this used to cover are no longer
    /// representable: `Disk::device` is a String, so "" is the only way to say
    /// "no device".
    #[test]
    fn a_disk_without_a_usable_device_name_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk(""),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert!(!matches!(plan, CreatePlan::Create { .. }), "{plan:?}");
    }

    /// And the reason reaches the page rather than being swallowed.
    #[test]
    fn a_disk_that_vanished_says_so() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk(""),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        let CreatePlan::Blocked(reason) = plan else {
            panic!("expected a reason, got {plan:?}");
        };
        assert!(reason.contains("disappeared"), "{reason}");
    }

    #[test]
    fn a_bare_device_name_is_rooted_under_dev() {
        let plan = plan_create(
            "node1",
            "d",
            &blank_disk("nvme0n1"),
            &recs(&[("node1--d", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/nvme0n1".to_string()
            }
        );
    }

    /// An absolute path is passed through untouched — "/dev//dev/sdb" would
    /// not exist, and ceph-volume's error for that is not obviously this.
    #[test]
    fn an_absolute_device_path_is_left_alone() {
        let plan = plan_create(
            "node1",
            "d",
            &blank_disk("/dev/disk/by-id/wwn-0x5000"),
            &recs(&[("node1--d", "ON")]),
            &osds(&[]),
            false,
            0,    // attempts
            None, // since_last_attempt
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/disk/by-id/wwn-0x5000".to_string()
            }
        );
    }

    // ── wants_on ─────────────────────────────────────────────────────────────

    #[test]
    fn an_absent_record_is_off() {
        assert!(!wants_on(&recs(&[]), "node1", "dev-sdb"));
    }

    #[test]
    fn only_on_and_using_are_on() {
        assert!(wants_on(&recs(&[("node1--d", "ON")]), "node1", "d"));
        assert!(wants_on(&recs(&[("node1--d", "USING")]), "node1", "d"));
        for off in ["OFF", "off", "On", "using", "", "REMOVING"] {
            assert!(
                !wants_on(&recs(&[("node1--d", off)]), "node1", "d"),
                "{off:?} must not read as on"
            );
        }
    }

    /// A hardware id is globally unique, so its record is bare rather than
    /// node-scoped, and record_key is what decides which shape to look for.
    #[test]
    fn a_hardware_id_record_is_found_without_a_node_prefix() {
        let id = "serial-wwn-0x50014ee214caf529";
        assert!(wants_on(&recs(&[(id, "ON")]), "node1", id));
        assert!(wants_on(&recs(&[(id, "ON")]), "node2", id));
    }

    // ── plan_purge: the last gate before a disk is wiped ─────────────────────

    use crate::routers::ceph::PgLoss;

    fn loss(stuck: u32, total: u32, unrecoverable: bool) -> PgLoss {
        PgLoss {
            stuck,
            total,
            unrecoverable,
        }
    }

    #[test]
    fn a_drain_still_in_progress_waits() {
        assert_eq!(plan_purge(false, false, None), PurgeVerdict::Wait);
    }

    /// The first answer is taken before the daemon is stopped, so a "no" there
    /// must not be overridden by anything later.
    #[test]
    fn an_unsafe_osd_waits_whatever_else_is_true() {
        assert_eq!(
            plan_purge(false, true, Some(&loss(0, 100, false))),
            PurgeVerdict::Wait
        );
    }

    /// Ceph agreed, the daemon went down, and now it does not agree. Something
    /// moved underneath; the disk is not ours to destroy on that basis.
    #[test]
    fn an_osd_that_stops_being_safe_after_the_daemon_stops_is_left_alone() {
        assert_eq!(plan_purge(true, false, None), PurgeVerdict::Recheck);
    }

    /// The incident this whole gate exists for. safe-to-destroy says yes
    /// BECAUSE Ceph has written the data off; that is exactly when the disk
    /// must not be wiped.
    #[test]
    fn a_cluster_with_unrecoverable_stuck_pgs_refuses_to_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(63, 200, true))),
            PurgeVerdict::RefuseDataAtRisk
        );
    }

    /// Both halves are required. Unreadable-but-rebuildable is ordinary
    /// recovery, and refusing there would mean a disk could never be removed
    /// from a degraded cluster.
    #[test]
    fn stuck_pgs_that_can_still_be_rebuilt_do_not_block_a_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(63, 200, false))),
            PurgeVerdict::Purge
        );
    }

    /// And a single-copy pool with nothing actually stuck is not at risk.
    #[test]
    fn a_single_copy_pool_with_nothing_stuck_does_not_block_a_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(0, 200, true))),
            PurgeVerdict::Purge
        );
    }

    #[test]
    fn a_healthy_cluster_purges() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(0, 200, false))),
            PurgeVerdict::Purge
        );
        assert_eq!(plan_purge(true, true, None), PurgeVerdict::Purge);
    }

    /// Every path that is not an unambiguous yes must not reach Purge. Written
    /// as an exhaustive sweep so a new condition cannot be added to the middle
    /// of the chain and default to destroying the disk.
    #[test]
    fn nothing_but_a_clear_yes_ever_reaches_purge() {
        for before in [false, true] {
            for after in [false, true] {
                for l in [
                    None,
                    Some(loss(0, 10, false)),
                    Some(loss(5, 10, false)),
                    Some(loss(0, 10, true)),
                    Some(loss(5, 10, true)),
                ] {
                    let at_risk = l.as_ref().is_some_and(|x| x.unrecoverable && x.stuck > 0);
                    let verdict = plan_purge(before, after, l.as_ref());
                    let should_purge = before && after && !at_risk;
                    assert_eq!(
                        verdict == PurgeVerdict::Purge,
                        should_purge,
                        "before={before} after={after} at_risk={at_risk} gave {verdict:?}"
                    );
                }
            }
        }
    }
}
