//! The image RBD's size arithmetic — shared by `images_rbd::run` (create) and
//! `images_grow::run` (grow), because computing it twice is how a disagreement
//! between the two once resized the image in both directions forever.
//!
//! It decides how much of the cluster one node's container store may claim.
//! Getting it wrong walks every machine into full-ratio, which blocks writes
//! for every app on every node — not something to discover on hardware, hence
//! the test cases below pin the exact arithmetic from the shell fragment this
//! replaces (nix/checks.nix's old `images-sizing` check drove the same three
//! cases against the shell version; these are that check, moved with the code
//! it was testing).

use serde_json::Value;

use crate::host::Host;

pub struct SizingPolicy {
    pub pool_name: String,
    pub share_of_pool: f64,
    pub min_size_gb: u64,
}

/// Number of CRUSH hosts with at least one OSD. Falls back to 1 — never 0 —
/// so a cluster the tree can't be read from still gets a sane ceiling instead
/// of one that divides by zero.
fn host_count(osd_tree: &Value) -> u64 {
    let n = osd_tree["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    n["type"] == "host" && n["children"].as_array().is_some_and(|c| !c.is_empty())
                })
                .count()
        })
        .unwrap_or(0);
    if n == 0 {
        1
    } else {
        n as u64
    }
}

/// `None` means "could not read pool capacity" — the caller's cue to size
/// nothing this tick rather than substitute a default that could shrink a
/// live image.
fn total_mb(df: &Value) -> Option<u64> {
    df["stats"]["total_bytes"].as_u64().map(|b| b / 1_048_576)
}

/// Replica count for `pool_name`. Falls back to 1 — never 0 — for the same
/// reason as `host_count`: dividing usable capacity by zero must never happen.
fn replica_count(pool_size: &Value) -> u64 {
    match pool_size["size"].as_u64() {
        Some(n) if n > 0 => n,
        _ => 1,
    }
}

/// The pure arithmetic: usable capacity (raw / replicas), the owner's share of
/// it, floored at `min_size_gb` and capped so no single node's image can eat
/// more than half of what `hosts` machines share. The ceiling applies LAST —
/// exceeding it is the full-ratio failure this module exists to prevent, so it
/// beats the floor when the two conflict.
fn want_mb(total_mb: u64, replicas: u64, hosts: u64, share_of_pool: f64, min_size_gb: u64) -> u64 {
    let usable_mb = total_mb / replicas;
    let want = (usable_mb as f64 * share_of_pool) as u64;
    let min_mb = min_size_gb * 1024;
    let want = want.max(min_mb);
    let cap_mb = usable_mb / (hosts * 2);
    want.min(cap_mb)
}

/// Runs the three `ceph` reads and applies `want_mb`. `Ok(None)` is the
/// "nothing to size yet" case (unreachable cluster or unreadable capacity),
/// which callers must treat as "do nothing this tick", not as an error.
pub async fn compute<H: Host>(host: &H, policy: &SizingPolicy) -> anyhow::Result<Option<u64>> {
    let hosts = host
        .ceph_json(&["osd", "tree"])
        .await
        .map(|v| host_count(&v))
        .unwrap_or(1);

    let Some(t_mb) = host
        .ceph_json(&["df"])
        .await
        .ok()
        .and_then(|v| total_mb(&v))
    else {
        return Ok(None);
    };

    let replicas = host
        .ceph_json(&["osd", "pool", "get", &policy.pool_name, "size"])
        .await
        .map(|v| replica_count(&v))
        .unwrap_or(1);

    Ok(Some(want_mb(
        t_mb,
        replicas,
        hosts,
        policy.share_of_pool,
        policy.min_size_gb,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(hosts: u64) -> Value {
        let children: Vec<Value> = (0..hosts)
            .map(|_| serde_json::json!({"type": "host", "children": [1]}))
            .collect();
        serde_json::json!({"nodes": children})
    }

    #[test]
    fn host_count_falls_back_to_one_when_unreadable() {
        assert_eq!(host_count(&Value::Null), 1);
        assert_eq!(host_count(&serde_json::json!({"nodes": []})), 1);
    }

    #[test]
    fn host_count_ignores_hosts_with_no_osds() {
        let t = serde_json::json!({"nodes": [
            {"type": "host", "children": []},
            {"type": "host", "children": [1]},
        ]});
        assert_eq!(host_count(&t), 1);
    }

    #[test]
    fn total_mb_is_none_when_capacity_cannot_be_read() {
        assert_eq!(total_mb(&Value::Null), None);
    }

    #[test]
    fn replica_count_falls_back_to_one() {
        assert_eq!(replica_count(&Value::Null), 1);
        assert_eq!(replica_count(&serde_json::json!({"size": 0})), 1);
        assert_eq!(replica_count(&serde_json::json!({"size": 3})), 3);
    }

    // ── want_mb: the exact cases the old shell-driven nix check pinned ────────

    #[test]
    fn one_copy_gets_a_quarter_of_the_pool() {
        assert_eq!(want_mb(1_000_000, 1, 1, 0.25, 40), 250_000);
    }

    #[test]
    fn two_copies_of_half_the_image_cost_the_same_raw_bytes() {
        let one = want_mb(1_000_000, 1, 1, 0.25, 40);
        let two = want_mb(1_000_000, 2, 1, 0.25, 40);
        assert_eq!(two, 125_000);
        assert_eq!(one, two * 2);
    }

    #[test]
    fn the_ceiling_beats_the_floor_on_a_small_pool() {
        // 4 hosts sharing a tiny pool: the ceiling (usable / (hosts*2)) must
        // win even though it lands below the 40G floor.
        let tiny = want_mb(1000, 2, 4, 0.25, 40);
        assert!(tiny <= 1000 / 2 / (4 * 2));
    }

    #[test]
    fn the_floor_wins_when_the_pool_is_merely_small_not_tiny() {
        // Plenty of ceiling room, but the share alone would undercut the
        // floor — the floor must raise it.
        let got = want_mb(100_000, 1, 1, 0.01, 40);
        assert_eq!(got, 40 * 1024);
    }

    #[tokio::test]
    async fn compute_returns_none_when_capacity_is_unreadable() {
        let host = crate::host::fake::FakeHost::new()
            .ok("ceph osd tree", r#"{"nodes":[]}"#)
            .fail("ceph df", "no osds up");
        let policy = SizingPolicy {
            pool_name: "images".into(),
            share_of_pool: 0.25,
            min_size_gb: 40,
        };
        assert_eq!(compute(&host, &policy).await.unwrap(), None);
    }

    #[tokio::test]
    async fn compute_wires_the_three_reads_into_want_mb() {
        let host = crate::host::fake::FakeHost::new()
            .ok("ceph osd tree", &tree(1).to_string())
            .ok("ceph df", r#"{"stats":{"total_bytes":1048576000000}}"#)
            .ok("ceph osd pool get images size", r#"{"size":1}"#);
        let policy = SizingPolicy {
            pool_name: "images".into(),
            share_of_pool: 0.25,
            min_size_gb: 40,
        };
        assert_eq!(compute(&host, &policy).await.unwrap(), Some(250_000));
    }
}
