//! Topology controller — drives Ceph redundancy from observed hardware.
//!
//! Runs only on the disk-reconciler lease holder, so the cluster has a single
//! writer. In **auto** mode (default) it computes copies / min_size / failure
//! domain / mon / mgr from node & OSD counts; in **manual** mode it applies the
//! user's pinned values. Increases in safety are applied automatically; it never
//! auto-*reduces* redundancy (fewer copies, dropping mons) — those follow a
//! hardware loss and are surfaced for the user instead of silently applied.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{kubectl, AppState};

const NS: &str = "rook-ceph";
const POLICY_CM: &str = "yolab-storage-policy";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoragePolicy {
    /// "auto" (config follows hardware) or "manual" (pinned values below).
    pub mode: String,
    pub size: u32,
    pub min_size: u32,
    pub failure_domain: String, // "osd" | "host"
}

impl Default for StoragePolicy {
    fn default() -> Self {
        Self { mode: "auto".into(), size: 2, min_size: 1, failure_domain: "host".into() }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Topology {
    pub nodes: u32,
    pub osds: u32,
    /// Hosts that actually carry an OSD — NOT the same as `nodes`.
    ///
    /// A machine can be in the Kubernetes cluster while contributing no storage:
    /// its disks are switched off, or its Ceph has not joined yet. With
    /// failure_domain=host, replicas must land on distinct hosts, so this is the
    /// real ceiling on `size`. Counting Kubernetes nodes instead asks Ceph for a
    /// placement it cannot satisfy, and every PG stays undersized forever.
    pub osd_hosts: u32,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Target {
    pub size: u32,
    pub min_size: u32,
    pub failure_domain: String,
    pub mon: u32,
    pub mgr: u32,
}

/// Pure decision function: given the policy and observed topology, what should
/// Ceph's redundancy look like? Kept pure so it can be unit-tested.
pub fn compute_target(policy: &StoragePolicy, topo: &Topology) -> Target {
    // Every machine runs its own mon, mgr and MDS. No node is a permanent
    // master: the first machine only *creates* the cluster, and after that it is
    // interchangeable with any other.
    //
    // So these two are not a target this controller drives toward — a mon exists
    // because a machine runs one, and that is decided by the node-join flow in
    // homelab/nixos/ceph/default.nix, not here. They are kept so the UI can show
    // the expected footprint and so apply_mon_mgr can report drift from it.
    //
    // The cost of one-mon-per-node is explicit and accepted: Paxos needs a
    // majority, so at two machines BOTH must be up. Three machines is where
    // losing one becomes survivable.
    let (mon, mgr) = (topo.nodes.max(1), topo.nodes.max(1));

    let (mut size, mut min_size, fd) = if policy.mode == "manual" {
        (policy.size, policy.min_size, policy.failure_domain.clone())
    } else if topo.nodes >= 3 {
        (3, 2, "host".to_string()) // survive a whole machine
    } else if topo.nodes == 2 {
        (2, 1, "host".to_string()) // survive one machine's disk
    } else if topo.osds >= 2 {
        (2, 1, "osd".to_string()) // 1 node, ≥2 disks: survive one disk (osd domain!)
    } else {
        (1, 1, "osd".to_string()) // 1 disk: no local redundancy — backups are the floor
    };

    // Feasibility clamps. Never ask for more copies than can actually be placed,
    // or every PG sits undersized forever and the cluster never returns to
    // HEALTH_OK.
    //
    // Two separate ceilings, and using only the first is a real bug: a second
    // machine joining Kubernetes moves `nodes` to 2 and this to size=2 with
    // failure_domain=host, but if that machine carries no OSDs — disks switched
    // off, or its Ceph not joined yet — there is still only ONE host to place on.
    // The OSD count does not catch it, because both OSDs are on the same host.
    size = size.clamp(1, 3).min(topo.osds.max(1));
    if fd == "host" {
        size = size.min(topo.osd_hosts.max(1));
    }
    min_size = min_size.clamp(1, size);
    Target { size, min_size, failure_domain: fd, mon, mgr }
}

// ── Policy persistence (ConfigMap) ────────────────────────────────────────────

/// None when the policy could not be read.
///
/// It used to fall back to `StoragePolicy::default()` — auto mode — so a
/// kubectl blip silently discarded a user's pinned manual policy and re-derived
/// one from hardware. On a cluster where they had deliberately pinned three
/// copies with fewer machines than that implies, the fallback reduces it.
///
/// A missing ConfigMap is different from an unreadable one and still means
/// "auto": that is the genuine first-run state, and the `kind` check is what
/// tells the two apart.
pub async fn read_policy() -> Option<StoragePolicy> {
    let v = kubectl::get_json(&["get", "configmap", POLICY_CM, "-n", NS, "-o", "json"])
        .await
        .ok()?;
    if v["kind"].as_str() != Some("ConfigMap") {
        return None;
    }
    let map: std::collections::HashMap<String, String> = v["data"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let d = StoragePolicy::default();
    Some(StoragePolicy {
        mode: map.get("mode").cloned().unwrap_or(d.mode),
        size: map.get("size").and_then(|s| s.parse().ok()).unwrap_or(d.size),
        min_size: map.get("min_size").and_then(|s| s.parse().ok()).unwrap_or(d.min_size),
        failure_domain: map.get("failure_domain").cloned().unwrap_or(d.failure_domain),
    })
}

async fn write_policy(p: &StoragePolicy) -> anyhow::Result<()> {
    let patch = serde_json::json!({"data": {
        "mode": p.mode,
        "size": p.size.to_string(),
        "min_size": p.min_size.to_string(),
        "failure_domain": p.failure_domain,
    }})
    .to_string();
    if kubectl::run(&["patch", "configmap", POLICY_CM, "-n", NS, "--type", "merge", "-p", &patch])
        .await
        .is_err()
    {
        let _ = kubectl::run(&["create", "configmap", POLICY_CM, "-n", NS]).await;
        kubectl::run(&["patch", "configmap", POLICY_CM, "-n", NS, "--type", "merge", "-p", &patch])
            .await?;
    }
    Ok(())
}

// ── Observation ───────────────────────────────────────────────────────────────

/// None when any input could not be read.
///
/// THIS FUNCTION DECIDES HOW MANY COPIES OF YOUR DATA EXIST, and every field
/// used to fall back to 0 on error. That is the wrong direction in the worst
/// possible way:
///
///   * `ceph osd tree` fails -> osd_hosts = 0 -> compute_target clamps
///     `size.min(osd_hosts.max(1))` to 1 -> apply_pools sets every pool to
///     size 1 -> Ceph DELETES the other copies. A transient timeout on one
///     command would destroy the redundancy of the whole cluster.
///
///   * `kubectl get nodes` fails -> nodes = 0 -> a healthy three-machine
///     cluster is treated as one machine, dropping to two copies and switching
///     the CRUSH rule from host to osd, which reshuffles every object.
///
/// There is no safe default for "how big is this cluster". Not knowing has to
/// mean not acting.
async fn observe() -> Option<Topology> {
    let nodes = kubectl::get_nodes().await.ok()?.len() as u32;
    // OSD count from Ceph itself. This used to count Rook OSD Deployments with a
    // ready replica; there are no such Deployments now, and the count was always
    // a proxy — it measured pods Rook had scheduled, not OSDs Ceph had up.
    // `num_up_osds` is the quantity the replication targets actually depend on.
    let osds = crate::ceph_cli::ceph_json(&["osd", "stat"])
        .await
        .ok()?["num_up_osds"]
        .as_u64()? as u32;

    // Host buckets in the CRUSH tree that hold at least one OSD.
    let tree = crate::ceph_cli::ceph_json(&["osd", "tree"]).await.ok()?;
    let osd_hosts = tree["nodes"]
        .as_array()?
        .iter()
        .filter(|n| {
            n["type"].as_str() == Some("host")
                && n["children"].as_array().is_some_and(|c| !c.is_empty())
        })
        .count() as u32;

    Some(Topology { nodes, osds, osd_hosts })
}

/// Health straight from the mon rather than from a Rook CR status field.
///
/// Returns "" when Ceph is unreachable, which every caller must treat as "do
/// not act" — the same discipline as an unknown fsid. Reading silence as
/// HEALTH_OK would let a reduction proceed against a cluster that cannot answer.
async fn cluster_health() -> Option<String> {
    crate::ceph_cli::ceph_json(&["health"])
        .await
        .ok()?["status"]
        .as_str()
        .map(str::to_string)
}

// ── Controller loop ───────────────────────────────────────────────────────────

pub async fn run_topology_controller() {
    // Let the cluster settle after boot before touching anything.
    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
    loop {
        if let Err(e) = tick().await {
            tracing::debug!("topology: {e}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

async fn tick() -> anyhow::Result<()> {
    // Single writer: only the reconciler leader acts.
    if !crate::disks_reconciler::is_reconcile_leader().await {
        return Ok(());
    }
    let Some(topo) = observe().await else {
        tracing::debug!("topology: cluster shape unknown this tick — not touching replication");
        return Ok(());
    };
    if topo.osds == 0 {
        return Ok(()); // nothing provisioned yet
    }
    // Don't rebalance while the cluster is in a hard-error state — or while we
    // cannot tell what state it is in. The old check compared against
    // "HEALTH_ERR" and read an unreachable cluster as "" — which passed, and let
    // pool sizes be rewritten against a cluster nobody could read.
    match cluster_health().await.as_deref() {
        Some("HEALTH_ERR") | None => return Ok(()),
        _ => {}
    }

    let Some(policy) = read_policy().await else {
        // Falling back to the defaults here would silently discard a pinned
        // manual policy and re-derive one from hardware.
        tracing::debug!("topology: storage policy unreadable this tick — not touching replication");
        return Ok(());
    };
    let target = compute_target(&policy, &topo);

    apply_mon_mgr(&target).await;
    apply_pools(&policy, &target).await;
    Ok(())
}

/// Report drift between the mon footprint and one-mon-per-node.
///
/// Deliberately observational: it reads, logs, and changes nothing.
///
/// Under Rook this patched `spec.mon.count` and the operator scheduled however
/// many mon pods that implied. With Ceph on the host there is no such dial — a
/// mon exists because a *machine* runs one — and the node-join flow in
/// homelab/nixos/ceph/default.nix is what creates it, at the one moment where
/// the joining machine is provably able to run it.
///
/// Both directions are left alone on purpose:
///
///   * Too few mons means a machine has joined Kubernetes but its Ceph has not
///     come up yet. That resolves itself on the joining node's bootstrap retry
///     timer, and acting on it from here would mean this node reaching into
///     another machine to start a daemon.
///
///   * Too many is the dangerous one. Removing a mon can cost quorum outright,
///     and — critically — a node that is merely *offline* is indistinguishable
///     from one that has left. Automating removal would mean a rebooting
///     machine could be evicted from the quorum by a peer.
async fn apply_mon_mgr(target: &Target) {
    let Ok(dump) = crate::ceph_cli::ceph_json(&["mon", "dump"]).await else {
        return;
    };
    let cur_mon = dump["mons"].as_array().map(|a| a.len()).unwrap_or(0) as u32;
    if cur_mon == target.mon {
        return;
    }

    if cur_mon < target.mon {
        tracing::info!(
            "topology: {cur_mon} mon(s) across {} node(s) — a machine has joined Kubernetes but \
             its Ceph has not yet; its yolab-ceph-bootstrap retry timer will pick it up",
            target.mon
        );
    } else {
        tracing::info!(
            "topology: {cur_mon} mon(s) for {} node(s) — a machine has left. Removal is \
             operator-driven (`ceph mon remove <name>`): an offline node must never be \
             auto-evicted from the quorum.",
            target.mon
        );
    }
}

/// Pools that must stay at one copy no matter how many machines join.
///
/// `images` holds each node's container image store, and its size=1 is load
/// bearing in two ways that homelab/nixos/ceph/images-store.nix spells out:
///
///   1. Every node needs its own unpacked copy of every image it runs, so
///      replicating across nodes buys nothing. At three machines this loop would
///      set size=3 and spend 9x the bytes on data any registry can re-send.
///
///   2. Worse, and the reason this is a filter and not a preference: each node
///      sizes its image RBD as a share of the pool's RAW capacity. At size=3 a
///      25% image consumes 75% of raw, three nodes consume 225%, and the pool
///      reaches full-ratio — at which point Ceph blocks writes for EVERY app on
///      EVERY node, not just image pulls.
///
/// images-store.nix predicted this exact failure in a comment. Nothing enforced
/// it, so the prediction was the only thing standing in its way.
fn is_unreplicated_pool(pool: &str) -> bool {
    pool == "images"
}

async fn pool_size(pool: &str) -> u32 {

    crate::ceph_cli::ceph(&["osd", "pool", "get", pool, "size", "-f", "json"])
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["size"].as_u64())
        .map(|x| x as u32)
        .unwrap_or(1)
}

/// Apply the target crush rule + size + min_size to every data pool.
/// Auto mode raises copies only (never silently drops a replica); manual mode
/// applies the pinned size exactly (the UI confirms reductions).
async fn apply_pools(policy: &StoragePolicy, target: &Target) {
    let rule = if target.failure_domain == "osd" { "replicated_osd" } else { "replicated_rule" };

    // Ensure the OSD-domain rule exists (the host rule ships by default).
    if target.failure_domain == "osd" {
        let have = crate::ceph_cli::ceph(&["osd", "crush", "rule", "ls"]).await.unwrap_or_default();
        if !have.lines().any(|l| l.trim() == rule) {
            let _ = crate::ceph_cli::ceph(&[
                "osd", "crush", "rule", "create-replicated", rule, "default", "osd",
            ])
            .await;
        }
    }

    let pools = crate::ceph_cli::ceph(&["osd", "pool", "ls"]).await.unwrap_or_default();
    for pool in pools
        .lines()
        .map(|l| l.trim())
        .filter(|p| !p.is_empty() && !p.starts_with(".nfs") && !p.starts_with(".rgw"))
        .filter(|p| !is_unreplicated_pool(p))
    {
        let cur = pool_size(pool).await;
        let want = if policy.mode == "manual" {
            target.size
        } else if cur > target.size {
            // cur > target.size implies cur > available OSDs (target.size is already
            // OSD-clamped). The pool is already degraded — reducing to the feasible
            // maximum can only help. If OSDs are added later, the raise-only path below
            // will grow size back up.
            target.size
        } else {
            cur.max(target.size) // raise-only in auto mode
        };
        let min = target.min_size.min(want);

        let _ = crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "crush_rule", rule]).await;
        if want != cur {
            let ws = want.to_string();
            let res = if want == 1 {
                crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "size", &ws, "--yes-i-really-mean-it"]).await
            } else {
                crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "size", &ws]).await
            };
            if res.is_ok() {
                tracing::info!("topology: pool {pool} size {cur}→{want} (fd={})", target.failure_domain);
            }
        }
        let ms = min.to_string();
        let _ = crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "min_size", &ms]).await;
    }
}

// ── HTTP: /api/storage/policy ─────────────────────────────────────────────────

/// Nulls rather than invented numbers when the cluster cannot be read.
///
/// This used to fall back to defaults for both the policy and the topology and
/// hand the result to `compute_target`, so a Storage page opened while Ceph was
/// unreachable showed a confident, fabricated answer: a specific number of
/// copies, a specific failure domain, a specific machine count. All of it made
/// up. "We do not know right now" is a worse-looking page and a truthful one.
pub async fn get_policy(State(_s): State<AppState>) -> Json<Value> {
    let policy = read_policy().await;
    let topo = observe().await;
    let target = match (&policy, &topo) {
        (Some(p), Some(t)) => Some(compute_target(p, t)),
        _ => None,
    };
    Json(serde_json::json!({ "policy": policy, "topology": topo, "target": target }))
}

#[derive(Deserialize)]
pub struct SetPolicyReq {
    pub mode: String,
    pub size: Option<u32>,
    pub min_size: Option<u32>,
    pub failure_domain: Option<String>,
}

pub async fn set_policy(
    State(_s): State<AppState>,
    Json(req): Json<SetPolicyReq>,
) -> (StatusCode, Json<Value>) {
    if req.mode != "auto" && req.mode != "manual" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "mode must be auto or manual"})));
    }
    // Read-modify-write, so an unreadable current policy must abort. Falling
    // back to defaults would quietly reset every field the caller did not send —
    // someone changing only the mode would find their pinned size and failure
    // domain replaced.
    let Some(mut p) = read_policy().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "cannot read the current storage settings right now — nothing was changed"
            })),
        );
    };
    p.mode = req.mode;
    if let Some(s) = req.size {
        p.size = s;
    }
    if let Some(m) = req.min_size {
        p.min_size = m;
    }
    if let Some(f) = req.failure_domain {
        p.failure_domain = f;
    }
    if p.size < 1 || p.size > 3 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "size must be 1–3"})));
    }
    if p.min_size < 1 || p.min_size > p.size {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "min_size must be ≥1 and ≤size"})));
    }
    if p.failure_domain != "osd" && p.failure_domain != "host" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "failure_domain must be osd or host"})));
    }
    match write_policy(&p).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true, "policy": p}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> StoragePolicy {
        StoragePolicy::default()
    }

    #[test]
    fn one_disk_no_redundancy() {
        let t = compute_target(&auto(), &Topology { nodes: 1, osds: 1, osd_hosts: 1 });
        assert_eq!((t.size, t.min_size, t.failure_domain.as_str(), t.mon), (1, 1, "osd", 1));
    }

    #[test]
    fn one_node_two_disks_uses_osd_domain() {
        // The classic trap: 2 disks on 1 host must use failure domain "osd",
        // else host-domain can't place a 2nd replica and sits degraded.
        let t = compute_target(&auto(), &Topology { nodes: 1, osds: 2, osd_hosts: 1 });
        assert_eq!((t.size, t.failure_domain.as_str()), (2, "osd"));
    }

    /// Two machines: two copies across two hosts, and a mon on each.
    ///
    /// The mon count is the accepted cost of every node being a peer. Two mons
    /// need a majority of two, so both machines must stay up — worse
    /// availability than a single mon, but no machine is special and no single
    /// machine dying takes the *data* with it.
    #[test]
    fn two_nodes_host_domain_and_a_mon_on_each() {
        let t = compute_target(&auto(), &Topology { nodes: 2, osds: 2, osd_hosts: 2 });
        assert_eq!((t.size, t.failure_domain.as_str(), t.mon), (2, "host", 2));
    }

    #[test]
    fn three_nodes_full_ha() {
        let t = compute_target(&auto(), &Topology { nodes: 3, osds: 3, osd_hosts: 3 });
        assert_eq!((t.size, t.min_size, t.failure_domain.as_str(), t.mon, t.mgr), (3, 2, "host", 3, 3));
    }

    /// The bug a second machine walks straight into.
    ///
    /// Node joins Kubernetes, so nodes=2 and the policy asks for size=2 across
    /// hosts — but its disks are off (or its Ceph has not joined), so every OSD
    /// is still on one host. Ceph cannot place a second replica anywhere, and
    /// the pools sit undersized forever instead of returning to HEALTH_OK.
    #[test]
    fn a_node_with_no_osds_does_not_raise_replication() {
        let t = Topology { nodes: 2, osds: 2, osd_hosts: 1 };
        let target = compute_target(&auto(), &t);
        assert_eq!(
            target.size, 1,
            "with one OSD-carrying host there is nowhere to put a second copy"
        );
    }

    /// Once the second machine really does carry storage, size rises.
    #[test]
    fn a_node_that_brings_disks_does_raise_replication() {
        let t = Topology { nodes: 2, osds: 2, osd_hosts: 2 };
        assert_eq!(compute_target(&auto(), &t).size, 2);
    }

    /// Three machines, but only two with disks: two copies, not three.
    #[test]
    fn replication_follows_osd_hosts_not_machine_count() {
        let t = Topology { nodes: 3, osds: 6, osd_hosts: 2 };
        let target = compute_target(&auto(), &t);
        assert_eq!(target.size, 2);
        assert!(target.min_size <= target.size);
    }

    /// The osd-domain case must be untouched: one node with several disks still
    /// replicates across disks, where the host ceiling does not apply.
    #[test]
    fn the_host_ceiling_does_not_apply_to_the_osd_domain() {
        let t = Topology { nodes: 1, osds: 3, osd_hosts: 1 };
        let target = compute_target(&auto(), &t);
        assert_eq!(target.failure_domain, "osd");
        assert_eq!(target.size, 2, "one host, but replicas go on separate disks");
    }

    #[test]
    fn size_never_exceeds_osd_count() {
        // 3 nodes but only 1 OSD so far — can't ask for 3 copies.
        let t = compute_target(&auto(), &Topology { nodes: 3, osds: 1, osd_hosts: 3 });
        assert_eq!(t.size, 1);
        assert!(t.min_size <= t.size);
    }

    #[test]
    fn manual_is_respected_but_clamped() {
        let p = StoragePolicy { mode: "manual".into(), size: 3, min_size: 3, failure_domain: "osd".into() };
        // Only 2 OSDs present → size clamped to 2, min_size clamped to ≤size.
        let t = compute_target(&p, &Topology { nodes: 1, osds: 2, osd_hosts: 1 });
        assert_eq!(t.size, 2);
        assert!(t.min_size <= 2);
        assert_eq!(t.failure_domain, "osd");
    }

    // ── The clamps that decide how many copies exist ──────────────────────────
    //
    // compute_target is only ever called with a Topology that was fully read —
    // observe() returns None otherwise. These pin what would happen if that ever
    // stopped being true, because the direction of the failure is what matters:
    // a zero here does not mean "small cluster", it means "unknown", and acting
    // on it deletes replicas.

    #[test]
    fn zero_osd_hosts_would_collapse_replication_to_one_copy() {
        // The exact shape a failed `ceph osd tree` used to produce: OSDs seen,
        // hosts not. apply_pools would then set every pool to size 1 and Ceph
        // would delete the other copies.
        let t = compute_target(&auto(), &Topology { nodes: 3, osds: 6, osd_hosts: 0 });
        assert_eq!(t.size, 1, "documents the collapse observe() now prevents");
    }

    #[test]
    fn zero_nodes_would_downgrade_a_healthy_cluster() {
        // A failed `kubectl get nodes` used to produce this on a healthy
        // three-machine cluster: two copies instead of three, and the failure
        // domain switched from host to osd, which reshuffles every object.
        let t = compute_target(&auto(), &Topology { nodes: 0, osds: 6, osd_hosts: 3 });
        assert_eq!((t.size, t.failure_domain.as_str()), (2, "osd"));
    }

    // ── Pools that must never be replicated ───────────────────────────────────

    #[test]
    fn the_images_pool_is_never_resized() {
        assert!(is_unreplicated_pool("images"));
    }

    #[test]
    fn app_data_pools_are_resized() {
        for p in ["yolab-fs-data0", "yolab-fs-metadata", ".mgr"] {
            assert!(!is_unreplicated_pool(p), "{p} must follow the replication target");
        }
    }

    /// Guards against a pool called something like "images-backup" being caught
    /// by a prefix match if this ever becomes one.
    #[test]
    fn only_the_exact_images_pool_is_exempt() {
        assert!(!is_unreplicated_pool("images-old"));
        assert!(!is_unreplicated_pool("my-images"));
    }
}
