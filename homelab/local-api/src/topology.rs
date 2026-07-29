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
const CLUSTER: &str = "rook-ceph";
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
    // Real mon HA needs 3 (mons must be odd; 2 is worse than 1). Only 3+ nodes.
    let (mon, mgr) = if topo.nodes >= 3 { (3, 2) } else { (1, 1) };

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

    // Feasibility clamps: never ask for more copies than there are OSDs, and keep
    // min_size within [1, size]. Prevents permanently-degraded pools.
    size = size.clamp(1, 3).min(topo.osds.max(1));
    min_size = min_size.clamp(1, size);
    Target { size, min_size, failure_domain: fd, mon, mgr }
}

// ── Policy persistence (ConfigMap) ────────────────────────────────────────────

pub async fn read_policy() -> StoragePolicy {
    let map: std::collections::HashMap<String, String> = kubectl::get_json(&[
        "get", "configmap", POLICY_CM, "-n", NS, "-o", "jsonpath={.data}",
    ])
    .await
    .ok()
    .and_then(|v| serde_json::from_value(v).ok())
    .unwrap_or_default();

    let d = StoragePolicy::default();
    StoragePolicy {
        mode: map.get("mode").cloned().unwrap_or(d.mode),
        size: map.get("size").and_then(|s| s.parse().ok()).unwrap_or(d.size),
        min_size: map.get("min_size").and_then(|s| s.parse().ok()).unwrap_or(d.min_size),
        failure_domain: map.get("failure_domain").cloned().unwrap_or(d.failure_domain),
    }
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

async fn observe() -> Topology {
    let nodes = kubectl::get_nodes().await.map(|n| n.len() as u32).unwrap_or(0);
    let osds = kubectl::get_json(&[
        "get", "deploy", "-n", NS, "-l", "app=rook-ceph-osd", "-o", "json",
    ])
    .await
    .ok()
    .and_then(|v| v["items"].as_array().map(|a| a.len() as u32))
    .unwrap_or(0);
    Topology { nodes, osds }
}

async fn cluster_health() -> String {
    kubectl::run(&[
        "get", "cephcluster", "-n", NS, CLUSTER, "-o", "jsonpath={.status.ceph.health}",
    ])
    .await
    .unwrap_or_default()
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
    let topo = observe().await;
    if topo.osds == 0 {
        return Ok(()); // nothing provisioned yet
    }
    // Don't rebalance while the cluster is in a hard-error state.
    if cluster_health().await == "HEALTH_ERR" {
        return Ok(());
    }

    let policy = read_policy().await;
    let target = compute_target(&policy, &topo);

    apply_mon_mgr(&target).await;
    apply_pools(&policy, &target).await;
    Ok(())
}

/// Raise-only mon/mgr scaling. We never auto-tear-down mons (a node that's
/// temporarily offline should not collapse quorum), so this only ever grows.
async fn apply_mon_mgr(target: &Target) {
    let Ok(cr) = kubectl::get_json(&["get", "cephcluster", CLUSTER, "-n", NS, "-o", "json"]).await
    else {
        return;
    };
    let cur_mon = cr["spec"]["mon"]["count"].as_u64().unwrap_or(1) as u32;
    let cur_mgr = cr["spec"]["mgr"]["count"].as_u64().unwrap_or(1) as u32;
    let new_mon = cur_mon.max(target.mon);
    let new_mgr = cur_mgr.max(target.mgr);
    if new_mon != cur_mon || new_mgr != cur_mgr {
        let patch = serde_json::json!({"spec": {"mon": {"count": new_mon}, "mgr": {"count": new_mgr}}})
            .to_string();
        if kubectl::run(&["patch", "cephcluster", CLUSTER, "-n", NS, "--type", "merge", "-p", &patch])
            .await
            .is_ok()
        {
            tracing::info!("topology: mon {cur_mon}→{new_mon}, mgr {cur_mgr}→{new_mgr}");
        }
    }
}

async fn pool_size(pool: &str) -> u32 {
    kubectl::ceph_exec(&["osd", "pool", "get", pool, "size", "-f", "json"])
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
        let have = kubectl::ceph_exec(&["osd", "crush", "rule", "ls"]).await.unwrap_or_default();
        if !have.lines().any(|l| l.trim() == rule) {
            let _ = kubectl::ceph_exec(&[
                "osd", "crush", "rule", "create-replicated", rule, "default", "osd",
            ])
            .await;
        }
    }

    let pools = kubectl::ceph_exec(&["osd", "pool", "ls"]).await.unwrap_or_default();
    for pool in pools
        .lines()
        .map(|l| l.trim())
        .filter(|p| !p.is_empty() && !p.starts_with(".nfs") && !p.starts_with(".rgw"))
    {
        let cur = pool_size(pool).await;
        let want = if policy.mode == "manual" { target.size } else { cur.max(target.size) };
        let min = target.min_size.min(want);

        let _ = kubectl::ceph_exec(&["osd", "pool", "set", pool, "crush_rule", rule]).await;
        if want != cur {
            let ws = want.to_string();
            let res = if want == 1 {
                kubectl::ceph_exec(&["osd", "pool", "set", pool, "size", &ws, "--yes-i-really-mean-it"]).await
            } else {
                kubectl::ceph_exec(&["osd", "pool", "set", pool, "size", &ws]).await
            };
            if res.is_ok() {
                tracing::info!("topology: pool {pool} size {cur}→{want} (fd={})", target.failure_domain);
            }
        }
        let ms = min.to_string();
        let _ = kubectl::ceph_exec(&["osd", "pool", "set", pool, "min_size", &ms]).await;
    }
}

// ── HTTP: /api/storage/policy ─────────────────────────────────────────────────

pub async fn get_policy(State(_s): State<AppState>) -> Json<Value> {
    let policy = read_policy().await;
    let topo = observe().await;
    let target = compute_target(&policy, &topo);
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
    let mut p = read_policy().await;
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
        let t = compute_target(&auto(), &Topology { nodes: 1, osds: 1 });
        assert_eq!((t.size, t.min_size, t.failure_domain.as_str(), t.mon), (1, 1, "osd", 1));
    }

    #[test]
    fn one_node_two_disks_uses_osd_domain() {
        // The classic trap: 2 disks on 1 host must use failure domain "osd",
        // else host-domain can't place a 2nd replica and sits degraded.
        let t = compute_target(&auto(), &Topology { nodes: 1, osds: 2 });
        assert_eq!((t.size, t.failure_domain.as_str()), (2, "osd"));
    }

    #[test]
    fn two_nodes_host_domain_single_mon() {
        let t = compute_target(&auto(), &Topology { nodes: 2, osds: 2 });
        assert_eq!((t.size, t.failure_domain.as_str(), t.mon), (2, "host", 1));
    }

    #[test]
    fn three_nodes_full_ha() {
        let t = compute_target(&auto(), &Topology { nodes: 3, osds: 3 });
        assert_eq!((t.size, t.min_size, t.failure_domain.as_str(), t.mon, t.mgr), (3, 2, "host", 3, 2));
    }

    #[test]
    fn size_never_exceeds_osd_count() {
        // 3 nodes but only 1 OSD so far — can't ask for 3 copies.
        let t = compute_target(&auto(), &Topology { nodes: 3, osds: 1 });
        assert_eq!(t.size, 1);
        assert!(t.min_size <= t.size);
    }

    #[test]
    fn manual_is_respected_but_clamped() {
        let p = StoragePolicy { mode: "manual".into(), size: 3, min_size: 3, failure_domain: "osd".into() };
        // Only 2 OSDs present → size clamped to 2, min_size clamped to ≤size.
        let t = compute_target(&p, &Topology { nodes: 1, osds: 2 });
        assert_eq!(t.size, 2);
        assert!(t.min_size <= 2);
        assert_eq!(t.failure_domain, "osd");
    }
}
