//! Topology controller — applies the redundancy the user asked for.
//!
//! Runs only on the disk-reconciler lease holder, so the cluster has a single
//! writer.
//!
//! THE USER STATES INTENT, CEPH REPORTS REALITY, THE UI SHOWS THE GAP.
//!
//! There used to be an "auto" mode that derived the copy count from how many
//! machines and disks it could currently see. It is gone, and the reason is
//! worth keeping: `observe()` counted OSDs that were UP, so unplugging a disk
//! made the cluster look smaller, and `apply_pools` then reduced `size` to
//! match. One disconnected disk silently dropped three copies to two; a second
//! took it to one, at which point Ceph DELETES the surviving replicas. Plugging
//! the disks back in re-replicated everything from whatever was left.
//!
//! The mistake was treating "cannot place a third copy right now" as "should
//! not keep a third copy". A missing disk is a temporary fact about
//! availability; `size` is a durable statement about how many copies the owner
//! wants. Nothing here adjusts it any more — the number is theirs, it is applied
//! as given, and when reality falls short the Storage page says so in words.
//!
//! `min_size` is likewise no longer computed; see MIN_SIZE.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{kubectl, AppState};

const NS: &str = "rook-ceph";
const POLICY_CM: &str = "yolab-storage-policy";

/// What the owner asked for. Two numbers, both theirs, neither derived.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoragePolicy {
    /// Copies of everything to keep. Applied as given — not clamped to what
    /// currently fits. Asking for more copies than there are disks is a legal
    /// thing to want: it means "and make the rest when I add disks", which is
    /// exactly what Ceph does once they appear.
    pub size: u32,
    /// "osd" — copies on different disks, survives a dead disk.
    /// "host" — copies on different machines, survives a dead machine.
    pub failure_domain: String,
}

/// Copies that must be ONLINE before Ceph will serve a placement group. Always
/// one, and never derived from anything.
///
/// `size` and `min_size` answer different questions. `size` is how many copies
/// to KEEP — a durability goal. `min_size` is how many must be REACHABLE before
/// Ceph will answer at all — an availability gate. Below it a placement group
/// goes inactive and every read AND write to it blocks; clients hang rather
/// than fail.
///
/// This used to be `size - 1`, so three machines meant min_size=2 and losing
/// two disks stopped every app on every machine until one came back. For a box
/// somebody keeps their photos on, "nothing works and I cannot see why" is a
/// worse outcome than the risk below, and it is not a state its owner can
/// diagnose or fix.
///
/// THE COST, STATED PLAINLY. At min_size=1 a write is acknowledged once a
/// single copy has it, so if that disk dies before the copy is made, that write
/// is gone. Recovery can also leave a placement group `incomplete` — Ceph knows
/// a newer version existed on the dead disk and refuses to serve the older one
/// — which needs hands-on intervention to clear.
///
/// That is the trade taken here: availability now, against a narrow window in
/// which a second failure loses recent writes. `size` is what defends against
/// that window, and `size` is the number the owner controls.
pub const MIN_SIZE: u32 = 1;

/// A policy the owner has never set is different from one that cannot be read,
/// and both are different from a policy that says "one copy".
pub enum PolicyState {
    /// No choice has been recorded yet — a fresh cluster, or one upgrading from
    /// the era of auto mode. See `seed_policy_from_cluster`.
    NotChosen,
    Chosen(StoragePolicy),
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

    // NO CLAMPING. The old code reduced `size` to whatever could be placed
    // right now, and "right now" counted OSDs that were UP — so a disconnected
    // disk shrank the target and `apply_pools` shrank the pool to match,
    // deleting replicas to satisfy a number derived from a cable.
    //
    // Wanting more copies than currently fit is a coherent thing to want. It
    // means "and make the rest when I add disks", and that is precisely what
    // Ceph does the moment they appear. Until then the placement groups sit
    // undersized, which is honest, harmless at min_size=1, and reported to the
    // owner in words by the Storage page rather than silently corrected here.
    Target {
        size: policy.size,
        min_size: MIN_SIZE,
        failure_domain: policy.failure_domain.clone(),
        mon,
        mgr,
    }
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
pub async fn read_policy() -> Option<PolicyState> {
    let v = match kubectl::get_json(&["get", "configmap", POLICY_CM, "-n", NS, "-o", "json"]).await
    {
        Ok(v) => v,
        Err(e) => {
            // A missing ConfigMap is the genuine first-run state and must mean
            // NotChosen, not "unreadable": only the former lets the controller
            // seed a policy, and nothing else creates this map. An unreachable
            // API server is the other, dangerous case and must stay None — the
            // `kind` check below is what told those two apart until a fresh
            // install's missing map was swallowed into None by `.ok()?`.
            if kubectl::is_not_found(&e) {
                return Some(PolicyState::NotChosen);
            }
            return None;
        }
    };
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

    // A cluster from the auto-mode era has `mode` and a `size` that was a
    // DEFAULT, not a decision — auto never wrote back what it applied. Reading
    // that stored 2 on a cluster running 3 copies and applying it would delete
    // a replica on the first tick after upgrading, which is the exact failure
    // this whole change exists to remove. So the presence of `mode` means the
    // policy has not really been chosen, and the pools themselves are asked
    // instead.
    let migrating = map.contains_key("mode");
    let (Some(size), false) = (
        map.get("size").and_then(|s| s.parse::<u32>().ok()),
        migrating,
    ) else {
        return Some(PolicyState::NotChosen);
    };

    Some(PolicyState::Chosen(StoragePolicy {
        size,
        failure_domain: map
            .get("failure_domain")
            .cloned()
            .unwrap_or_else(|| "host".into()),
    }))
}

/// Adopt whatever the cluster is already doing as the owner's choice.
///
/// Run once, when no policy has been recorded. Copies the largest size across
/// the data pools and the failure domain of the rule they are using, so
/// upgrading changes nothing about the data and only writes down what was
/// already true. Taking a default here instead would shrink a pool the moment
/// this controller first ran.
async fn seed_policy_from_cluster() -> Option<StoragePolicy> {
    let pools = crate::ceph_cli::ceph(&["osd", "pool", "ls"]).await.ok()?;
    let mut size = 0u32;
    for pool in pools
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.starts_with('.'))
    {
        size = size.max(pool_size(pool).await);
    }
    // No data pools yet: a brand new cluster. One copy is the only honest
    // starting point — it is what a single disk can hold.
    let size = if size == 0 { 1 } else { size };

    // "host" only when the cluster can actually place that way today; otherwise
    // the first apply would leave everything undersized for a reason the owner
    // never chose.
    // Unknown topology falls back to "osd", the domain that can always be
    // placed. Guessing "host" could leave every group undersized on a cluster
    // whose shape we could not read.
    let osd_hosts = observe().await.map(|t| t.osd_hosts).unwrap_or(0);
    let failure_domain = if osd_hosts >= size { "host" } else { "osd" };

    let p = StoragePolicy {
        size,
        failure_domain: failure_domain.into(),
    };
    match write_policy(&p).await {
        Ok(()) => {
            tracing::info!(
                "storage policy adopted from the running cluster: {size} copies, {failure_domain} domain"
            );
            Some(p)
        }
        Err(e) => {
            tracing::warn!("could not record the storage policy: {e}");
            None
        }
    }
}

async fn write_policy(p: &StoragePolicy) -> anyhow::Result<()> {
    // `mode` and `min_size` are removed, not just left unwritten: a merge patch
    // keeps keys it does not mention, and a lingering `mode` would make
    // read_policy treat every later read as an unmigrated cluster.
    let patch = serde_json::json!({"data": {
        "mode": serde_json::Value::Null,
        "min_size": serde_json::Value::Null,
        "size": p.size.to_string(),
        "failure_domain": p.failure_domain,
    }})
    .to_string();
    if kubectl::run(&[
        "patch",
        "configmap",
        POLICY_CM,
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
        let _ = kubectl::run(&["create", "configmap", POLICY_CM, "-n", NS]).await;
        kubectl::run(&[
            "patch",
            "configmap",
            POLICY_CM,
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
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
pub(crate) async fn observe() -> Option<Topology> {
    let nodes = kubectl::get_nodes().await.ok()?.len() as u32;
    // OSD count from Ceph itself. This used to count Rook OSD Deployments with a
    // ready replica; there are no such Deployments now, and the count was always
    // a proxy — it measured pods Rook had scheduled, not OSDs Ceph had up.
    // `num_up_osds` is the quantity the replication targets actually depend on.
    let osds =
        crate::ceph_cli::ceph_json(&["osd", "stat"]).await.ok()?["num_up_osds"].as_u64()? as u32;

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

    Some(Topology {
        nodes,
        osds,
        osd_hosts,
    })
}

/// Health straight from the mon rather than from a Rook CR status field.
///
/// Returns "" when Ceph is unreachable, which every caller must treat as "do
/// not act" — the same discipline as an unknown fsid. Reading silence as
/// HEALTH_OK would let a reduction proceed against a cluster that cannot answer.
async fn cluster_health() -> Option<String> {
    crate::ceph_cli::ceph_json(&["health"]).await.ok()?["status"]
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
    // A restore's RebuildingStorage phase purges OSDs and destroys/recreates CephFS
    // pools directly — reapplying pool size/failure-domain here at the same time
    // would race that teardown (e.g. setting a crush rule on a pool this tick just
    // watched get deleted out from under it).
    if crate::routers::restore_run::is_active().await {
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

    let policy = match read_policy().await {
        None => {
            tracing::debug!("topology: storage policy unreadable this tick — changing nothing");
            return Ok(());
        }
        Some(PolicyState::NotChosen) => {
            // First run, or the first run after auto mode was removed. Write
            // down what the cluster is already doing and act on it next tick;
            // nothing is applied from a policy that was never chosen.
            let _ = seed_policy_from_cluster().await;
            return Ok(());
        }
        Some(PolicyState::Chosen(p)) => p,
    };
    let target = compute_target(&policy, &topo);

    apply_mon_mgr(&target).await;
    apply_pools(&target).await;
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

/// Whether `apply_pools` owns this pool's replica count.
///
/// Ceph's own rgw and nfs pools manage themselves. `images` is deliberately
/// included: pinning it at one copy is what made losing a single disk take the
/// whole container store with it.
fn apply_pools_selects(pool: &str) -> bool {
    !pool.is_empty() && !pool.starts_with(".nfs") && !pool.starts_with(".rgw")
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
/// There is no "auto" mode any more (see this module's header) and no
/// raise-only rule to go with it: `target.size` is the owner's own number,
/// and it is applied exactly, up or down.
async fn apply_pools(target: &Target) {
    let rule = if target.failure_domain == "osd" {
        "replicated_osd"
    } else {
        "replicated_rule"
    };

    // Ensure the OSD-domain rule exists (the host rule ships by default).
    if target.failure_domain == "osd" {
        let have = crate::ceph_cli::ceph(&["osd", "crush", "rule", "ls"])
            .await
            .unwrap_or_default();
        if !have.lines().any(|l| l.trim() == rule) {
            let _ = crate::ceph_cli::ceph(&[
                "osd",
                "crush",
                "rule",
                "create-replicated",
                rule,
                "default",
                "osd",
            ])
            .await;
        }
    }

    let pools = crate::ceph_cli::ceph(&["osd", "pool", "ls"])
        .await
        .unwrap_or_default();
    for pool in pools
        .lines()
        .map(|l| l.trim())
        .filter(|p| apply_pools_selects(p))
    {
        let cur = pool_size(pool).await;
        // The owner's number, applied as given — up or down. There is no
        // raise-only rule any more because there is nothing left that could
        // lower it behind their back: `size` comes from the policy and nowhere
        // else, so the only way it drops is somebody asking for that.
        let want = target.size;
        let min = MIN_SIZE;

        let _ = crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "crush_rule", rule]).await;
        if want != cur {
            let ws = want.to_string();
            let res = if want == 1 {
                crate::ceph_cli::ceph(&[
                    "osd",
                    "pool",
                    "set",
                    pool,
                    "size",
                    &ws,
                    "--yes-i-really-mean-it",
                ])
                .await
            } else {
                crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "size", &ws]).await
            };
            if res.is_ok() {
                tracing::info!(
                    "topology: pool {pool} size {cur}→{want} (fd={})",
                    target.failure_domain
                );
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
    let chosen = match read_policy().await {
        Some(PolicyState::Chosen(p)) => Some(p),
        // Not chosen yet, or unreadable. Null rather than an invented default:
        // the page would otherwise show a copy count nobody selected, next to a
        // cluster doing something else.
        _ => None,
    };
    let topo = observe().await;
    let target = match (&chosen, &topo) {
        (Some(p), Some(t)) => Some(compute_target(p, t)),
        _ => None,
    };
    Json(serde_json::json!({
        "policy": chosen,
        "topology": topo,
        "target": target,
        // So the page can render "each copy lives on a different disk/machine"
        // without hardcoding a number that is decided here.
        "min_size": MIN_SIZE,
    }))
}

#[derive(Deserialize)]
pub struct SetPolicyReq {
    pub size: Option<u32>,
    /// Accepted and ignored: older clients still send it, and the threshold is
    /// decided here (`MIN_SIZE`), not by the caller. Optional, so a client that
    /// has stopped sending it still parses — the point of tolerating a
    /// field is that its absence has to be fine too.
    #[allow(dead_code)]
    pub min_size: Option<u32>,
    pub failure_domain: Option<String>,
}

// `mode` is deliberately absent rather than accepted-and-ignored. It used to be
// declared here as a bare `String` to tolerate older clients, which had exactly
// the opposite effect: serde treats a non-Option field as REQUIRED, so the new
// page — which correctly no longer sends a mode — got
//
//   missing field `mode` at line 1 column 33
//
// (column 33 being the end of `{"size":2,"failure_domain":"osd"}`) and every
// attempt to change the copy count failed with a deserialization error instead
// of changing anything. Serde ignores unknown fields by default, so REMOVING
// the field is what actually accepts both the old shape and the new one.

pub async fn set_policy(
    State(_s): State<AppState>,
    Json(req): Json<SetPolicyReq>,
) -> (StatusCode, Json<Value>) {
    // Both fields are required now. There is no mode to change on its own and
    // no derived value to preserve, so a partial update has nothing to merge
    // into — which also removes the read-modify-write that made this fail when
    // the current policy could not be read.
    let (Some(size), Some(failure_domain)) = (req.size, req.failure_domain.clone()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "size and failure_domain are both required"})),
        );
    };

    // No upper bound on purpose. Asking for more copies than there are disks
    // means "and make the rest when I add some", which is what Ceph does when
    // they appear. Zero is the one number that is not a wish but a mistake:
    // Ceph rejects it, and it would read as "keep no copies".
    if size < 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "keep at least one copy"})),
        );
    }
    if failure_domain != "osd" && failure_domain != "host" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "failure_domain must be osd or host"})),
        );
    }
    let p = StoragePolicy {
        size,
        failure_domain,
    };
    match write_policy(&p).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "policy": p})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(size: u32, fd: &str) -> StoragePolicy {
        StoragePolicy {
            size,
            failure_domain: fd.into(),
        }
    }

    fn topo(nodes: u32, osds: u32, osd_hosts: u32) -> Topology {
        Topology {
            nodes,
            osds,
            osd_hosts,
        }
    }

    // ── The policy is applied as given ────────────────────────────────────────

    #[test]
    fn the_chosen_size_is_what_comes_out() {
        for size in [1u32, 2, 3, 5, 9] {
            let t = compute_target(&policy(size, "host"), &topo(3, 6, 3));
            assert_eq!(t.size, size);
        }
    }

    #[test]
    fn the_chosen_failure_domain_is_what_comes_out() {
        for fd in ["osd", "host"] {
            let t = compute_target(&policy(2, fd), &topo(2, 4, 2));
            assert_eq!(t.failure_domain, fd);
        }
    }

    /// Asking for more copies than there are disks is allowed, and means "make
    /// the rest when I add some". The old code clamped it down to what fitted;
    /// this asserts it no longer does.
    #[test]
    fn more_copies_than_disks_is_kept_not_clamped() {
        let t = compute_target(&policy(3, "osd"), &topo(1, 1, 1));
        assert_eq!(t.size, 3, "the owner's number survives a small cluster");
    }

    /// Same, for the host domain: three copies across one machine cannot be
    /// placed today and will be placed the day a third machine joins.
    #[test]
    fn more_copies_than_machines_is_kept_not_clamped() {
        let t = compute_target(&policy(3, "host"), &topo(1, 4, 1));
        assert_eq!(t.size, 3);
    }

    // ── The regression that motivated all of this ─────────────────────────────

    /// THE POINT OF THIS MODULE.
    ///
    /// Unplugging a disk used to shrink the target — `observe()` counted OSDs
    /// that were UP — and `apply_pools` then shrank the pool to match, deleting
    /// replicas. A second unplug took it to one copy, and reconnecting both
    /// re-replicated everything from whatever survived.
    ///
    /// The topology is now irrelevant to `size`, so no cable can change how
    /// many copies the cluster keeps.
    #[test]
    fn losing_disks_cannot_change_how_many_copies_are_kept() {
        let p = policy(3, "host");
        let healthy = compute_target(&p, &topo(3, 3, 3));
        let one_gone = compute_target(&p, &topo(3, 2, 2));
        let two_gone = compute_target(&p, &topo(3, 1, 1));
        let all_gone = compute_target(&p, &topo(0, 0, 0));

        assert_eq!(healthy.size, 3);
        assert_eq!(
            one_gone.size, 3,
            "a disconnected disk is not a smaller cluster"
        );
        assert_eq!(two_gone.size, 3);
        assert_eq!(
            all_gone.size, 3,
            "even an unreadable cluster keeps the promise"
        );
    }

    // ── min_size ──────────────────────────────────────────────────────────────

    /// It used to be `size - 1`, so three machines produced min_size=2: lose two
    /// disks and every app on every machine stopped until one came back — a
    /// state its owner can neither diagnose nor fix. One reachable copy is now
    /// always enough to keep serving.
    #[test]
    fn min_size_is_one_whatever_is_asked_for() {
        for size in [1u32, 2, 3, 7] {
            for fd in ["osd", "host"] {
                let t = compute_target(&policy(size, fd), &topo(3, 6, 3));
                assert_eq!(t.min_size, 1, "size={size} fd={fd}");
            }
        }
    }

    #[test]
    fn min_size_never_exceeds_size() {
        let t = compute_target(&policy(1, "osd"), &topo(1, 1, 1));
        assert!(t.min_size <= t.size);
    }

    // ── mon / mgr ─────────────────────────────────────────────────────────────
    //
    // Still derived from the node count, because a mon exists by virtue of a
    // machine running one. These are reported so the UI can show drift, not
    // driven toward.

    #[test]
    fn one_mon_and_mgr_per_machine() {
        assert_eq!(compute_target(&policy(2, "host"), &topo(3, 3, 3)).mon, 3);
        assert_eq!(compute_target(&policy(2, "host"), &topo(3, 3, 3)).mgr, 3);
    }

    /// An empty topology must not report zero mons — the machine reading this
    /// is itself one.
    #[test]
    fn an_unreadable_cluster_still_reports_at_least_one_mon() {
        let t = compute_target(&policy(1, "osd"), &topo(0, 0, 0));
        assert_eq!((t.mon, t.mgr), (1, 1));
    }

    // ── Pool selection ────────────────────────────────────────────────────────

    /// `images` used to be pinned at one copy, which made a single disk loss
    /// take the container store with it. It now follows the chosen size like
    /// every other pool; the raw cost of that is paid for in images-store.nix,
    /// which divides the RBD by the replica count.
    #[test]
    fn every_data_pool_including_images_follows_the_chosen_size() {
        for p in ["images", "yolab-fs-data0", "yolab-fs-metadata"] {
            assert!(apply_pools_selects(p), "{p} must follow the chosen size");
        }
    }

    /// Ceph's own pools are still left alone: `.mgr` sizes itself, and the
    /// rgw/nfs ones are not ours to touch.
    #[test]
    fn cephs_own_pools_are_left_alone() {
        for p in [".rgw.root", ".nfs"] {
            assert!(!apply_pools_selects(p), "{p} must not be resized");
        }
    }

    // ── The request body the page actually sends ────────────────────────────────

    /// The exact body StoragePage sends. This failed with "missing field
    /// `mode` at line 1 column 33" for as long as `mode` was a bare `String`:
    /// nothing about the endpoint needed a mode, but serde still demanded one,
    /// so every copy-count change was rejected before `set_policy` ran.
    #[test]
    fn the_current_page_body_parses() {
        let req: SetPolicyReq =
            serde_json::from_str(r#"{"size":2,"failure_domain":"osd"}"#).unwrap();
        assert_eq!(req.size, Some(2));
        assert_eq!(req.failure_domain.as_deref(), Some("osd"));
    }

    /// The reason the field was there in the first place. An older client still
    /// sends `mode` and `min_size`; both are ignored, and neither may make the
    /// body unparseable.
    #[test]
    fn an_older_client_body_still_parses() {
        let req: SetPolicyReq = serde_json::from_str(
            r#"{"mode":"manual","size":3,"min_size":2,"failure_domain":"host"}"#,
        )
        .unwrap();
        assert_eq!(req.size, Some(3));
        assert_eq!(req.failure_domain.as_deref(), Some("host"));
    }
}
