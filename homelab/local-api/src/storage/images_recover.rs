//! Rebuild the images pool's unrecoverable placement groups, empty.
//!
//! THE ONE POOL WHOSE CONTENTS ARE NOT THE OWNER'S DATA.
//!
//! Everything in `images` is a container layer a registry will send again. That
//! makes it the only pool on this platform where "we cannot read this any more"
//! has a correct automatic answer: discard it and re-pull. The owner's actual
//! data lives in `yolab-fs-metadata` and `yolab-fs-data0`, and those are never
//! touched here — losing them is what the backup system exists for, and getting
//! it back is a conscious decision a person makes, not one this file makes for
//! them.
//!
//! WHY THIS EXISTS AT ALL. The self-healing above it rebuilds the *filesystem*
//! on the RBD (`mkfs` when the store will not mount or read) and the *contents*
//! of the data-root (`discard_and_swap`). It stopped one layer short: nothing
//! ever rebuilt the RBD's own storage. So a pool whose PGs had no surviving copy
//! left the cluster waiting forever for a disk that was never coming back —
//! `images_rbd` only ever created the pool when it was ABSENT, and a pool that
//! exists but cannot serve fell straight through every check. Observed
//! 2026-09-11: 26 of the images pool's 32 PGs down after a disk was pulled at
//! size 1, every node parked on its root disk indefinitely, and the Ceph
//! dashboard's pool page hanging because enumerating RBD images never returned.
//!
//! WHY A GRACE PERIOD, AND WHY IT IS NOT ABOUT OSDs. `down` does not mean gone,
//! it means no copy is available *right now*. A `nixos-rebuild` restarts the OSD
//! activation chain and takes an LV down for ~90s; a node reboot is a few
//! minutes. Both produce down PGs that recover on their own. Firing immediately
//! would throw away every node's image cache on every routine deploy and force a
//! simultaneous re-pull across the cluster's uplink — a worse outage than the one
//! being repaired. The grace period is the whole guard: not "is this OSD coming
//! back", just "has this lasted long enough that it plainly is not a blip".
//!
//! `force-create-pg` rather than deleting and recreating the pool, deliberately.
//! Pool deletion needs `mon_allow_pool_delete`, which is `false` on these
//! clusters and should stay that way: turning it on to serve this feature would
//! put `yolab-fs-*` within reach of any bug in it. `force-create-pg` cannot
//! delete a pool, takes one PG at a time, and leaves the pool's size, rule and
//! application exactly as they were. Verified present on Ceph 20.2.3 (tentacle).
//!
//! After this runs the RBD's objects are empty, so the image reads as corrupt —
//! which `containerd_store::filesystem_is_usable` already recognises, and already
//! answers with `mkfs`. Nothing new is needed downstream; this only removes the
//! blockage that kept that machinery from ever getting a chance to run.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::host::Host;

pub struct RecoverPolicy {
    pub pool_name: String,
    pub grace: Duration,
}

fn marker_path(root: &Path) -> PathBuf {
    root.join("var/lib/ceph/.yolab-images-unservable-since")
}

/// PG states that mean no copy of the data is available to serve.
///
/// DELIBERATELY NARROW, and `unknown` is deliberately NOT here. A PG reports
/// `unknown` when the mgr has not yet received stats for it — which is the normal
/// state for a minute after a mgr restart or failover, and was the state of 43
/// PGs on the live cluster moments after `mgr` moved to node3. Treating it as
/// lost would destroy an image store every time the manager restarts.
///
/// `stale` alone is likewise not here: it means the mon has not heard from the
/// PG's primary recently, which a restarting OSD produces routinely. It only
/// counts when it appears alongside `down`, e.g. `stale+down`, and then it is the
/// `down` that decides.
fn is_lost_state(state: &str) -> bool {
    state.split('+').any(|s| s == "down" || s == "incomplete")
}

/// The pool's numeric id, looked up by NAME so the caller's configured pool is
/// the only thing that can be acted on. Never hardcode the id: it is assigned at
/// creation time and differs between clusters.
async fn pool_id<H: Host>(host: &H, pool: &str) -> Option<i64> {
    let v = host.ceph_json(&["osd", "lspools"]).await.ok()?;
    v.as_array()?
        .iter()
        .find_map(|p| (p["poolname"].as_str()? == pool).then(|| p["poolnum"].as_i64())?)
}

/// Inactive PGs belonging to `pool` whose state says the data is gone.
///
/// Every pgid is re-checked against the pool id prefix rather than trusted from
/// the query, so a change to the Ceph command's filtering can never widen this
/// onto another pool's placement groups.
async fn lost_pgs<H: Host>(host: &H, pool: i64) -> Vec<String> {
    let Ok(v) = host.ceph_json(&["pg", "dump_stuck", "inactive"]).await else {
        return Vec::new();
    };
    let prefix = format!("{pool}.");
    v["stuck_pg_stats"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|pg| {
                    let id = pg["pgid"].as_str()?;
                    let state = pg["state"].as_str()?;
                    (id.starts_with(&prefix) && is_lost_state(state)).then(|| id.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How long the pool has been unservable, starting the clock if this is the
/// first time we have seen it. Returns `None` when the clock has just started.
fn elapsed_unservable(root: &Path, now: u64) -> Option<Duration> {
    let marker = marker_path(root);
    match std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(since) => Some(Duration::from_secs(now.saturating_sub(since))),
        None => {
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, now.to_string());
            None
        }
    }
}

fn clear_marker(root: &Path) {
    let _ = std::fs::remove_file(marker_path(root));
}

pub async fn run<H: Host>(host: &H, root: &Path, policy: &RecoverPolicy) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("images-recover: ceph not reachable — nothing to judge yet");
        return Ok(());
    }
    let Some(pool) = pool_id(host, &policy.pool_name).await else {
        // No pool at all is images_rbd's job, not this one's.
        clear_marker(root);
        return Ok(());
    };

    let lost = lost_pgs(host, pool).await;
    if lost.is_empty() {
        // Healthy, or degraded in a way that still serves reads. Either way the
        // clock must be reset, or an unrelated blip weeks later would inherit a
        // stale start time and trip the grace period instantly.
        clear_marker(root);
        return Ok(());
    }

    let Some(elapsed) = elapsed_unservable(root, now_secs()) else {
        tracing::warn!(
            "images-recover: {} has {} placement group(s) with no surviving copy; \
             starting the {}s clock before rebuilding them",
            policy.pool_name,
            lost.len(),
            policy.grace.as_secs()
        );
        return Ok(());
    };
    if elapsed < policy.grace {
        tracing::info!(
            "images-recover: {} still unservable ({} pg(s)) after {}s — waiting until {}s \
             in case this is an OSD restart rather than a lost disk",
            policy.pool_name,
            lost.len(),
            elapsed.as_secs(),
            policy.grace.as_secs()
        );
        return Ok(());
    }

    tracing::warn!(
        "images-recover: {} has been unable to serve for {}s. Rebuilding {} placement \
         group(s) empty — every object in this pool is a container layer a registry \
         will send again. Nodes will re-pull their images.",
        policy.pool_name,
        elapsed.as_secs(),
        lost.len()
    );
    let prefix = format!("{pool}.");
    for pg in &lost {
        // Belt and braces: `lost_pgs` already filtered on this, and it is cheap to
        // refuse again right before the only destructive call in this file.
        if !pg.starts_with(&prefix) {
            tracing::error!("images-recover: refusing to touch {pg} — not in pool {pool}");
            continue;
        }
        match host
            .ceph(&["osd", "force-create-pg", pg, "--yes-i-really-mean-it"])
            .await
        {
            Ok(_) => tracing::info!("images-recover: rebuilt {pg}"),
            Err(e) => tracing::warn!("images-recover: could not rebuild {pg}: {e}"),
        }
    }
    clear_marker(root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn policy() -> RecoverPolicy {
        RecoverPolicy {
            pool_name: "images".into(),
            grace: Duration::from_secs(900),
        }
    }

    const LSPOOLS: &str = r#"[{"poolnum":2,"poolname":"yolab-fs-metadata"},
                              {"poolnum":3,"poolname":"yolab-fs-data0"},
                              {"poolnum":4,"poolname":"images"}]"#;

    fn stuck(entries: &[(&str, &str)]) -> String {
        let items: Vec<String> = entries
            .iter()
            .map(|(id, state)| format!(r#"{{"pgid":"{id}","state":"{state}"}}"#))
            .collect();
        format!(
            r#"{{"pg_ready":true,"stuck_pg_stats":[{}]}}"#,
            items.join(",")
        )
    }

    // ── the state classifier ────────────────────────────────────────────────

    #[test]
    fn down_and_incomplete_mean_the_data_is_gone() {
        assert!(is_lost_state("down"));
        assert!(is_lost_state("stale+down"));
        assert!(is_lost_state("incomplete"));
    }

    /// The one that would have destroyed a healthy cluster's image store: 43 PGs
    /// reported `unknown` on the live cluster right after the mgr failed over.
    #[test]
    fn unknown_and_stale_alone_are_not_loss() {
        assert!(!is_lost_state("unknown"));
        assert!(!is_lost_state("stale"));
        assert!(!is_lost_state("active+clean"));
        assert!(!is_lost_state("active+undersized+degraded"));
    }

    // ── scoping ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn never_touches_another_pools_placement_groups() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd lspools", LSPOOLS)
            .ok(
                "ceph pg dump_stuck inactive",
                // app data is down too — it must be ignored entirely
                &stuck(&[("3.1a", "down"), ("2.7", "incomplete"), ("4.d", "down")]),
            )
            .ok("ceph osd force-create-pg", "");
        let dir = tempfile::tempdir().unwrap();
        // Pre-age the clock so the grace period is already satisfied.
        std::fs::create_dir_all(dir.path().join("var/lib/ceph")).unwrap();
        std::fs::write(marker_path(dir.path()), (now_secs() - 5000).to_string()).unwrap();

        run(&host, dir.path(), &policy()).await.unwrap();

        let calls = host.calls();
        assert!(
            calls.iter().any(|c| c.contains("force-create-pg 4.d")),
            "the images pool's lost pg must be rebuilt, calls: {calls:?}"
        );
        for forbidden in ["3.1a", "2.7"] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "{forbidden} is the owner's data and must never be rebuilt, calls: {calls:?}"
            );
        }
    }

    // ── the grace period ────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_first_sighting_only_starts_the_clock() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd lspools", LSPOOLS)
            .ok("ceph pg dump_stuck inactive", &stuck(&[("4.d", "down")]));
        let dir = tempfile::tempdir().unwrap();

        run(&host, dir.path(), &policy()).await.unwrap();

        assert!(
            !host.ran("force-create-pg"),
            "a pool seen unservable for the first time may just be an OSD restarting"
        );
        assert!(
            marker_path(dir.path()).exists(),
            "the clock must be started"
        );
    }

    #[tokio::test]
    async fn waits_out_the_grace_period_before_destroying_anything() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd lspools", LSPOOLS)
            .ok("ceph pg dump_stuck inactive", &stuck(&[("4.d", "down")]));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/ceph")).unwrap();
        // 60s in: a nixos-rebuild's OSD restart is ~90s, so this must not fire.
        std::fs::write(marker_path(dir.path()), (now_secs() - 60).to_string()).unwrap();

        run(&host, dir.path(), &policy()).await.unwrap();

        assert!(
            !host.ran("force-create-pg"),
            "a deploy restarting an OSD must not cost the cluster its image cache"
        );
    }

    /// Recovery resets the clock, or a blip weeks later inherits a stale start
    /// time and trips the grace period on its first tick.
    #[tokio::test]
    async fn a_pool_that_recovers_clears_the_clock() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd lspools", LSPOOLS)
            .ok("ceph pg dump_stuck inactive", &stuck(&[("4.d", "unknown")]));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/ceph")).unwrap();
        std::fs::write(marker_path(dir.path()), (now_secs() - 5000).to_string()).unwrap();

        run(&host, dir.path(), &policy()).await.unwrap();

        assert!(!host.ran("force-create-pg"));
        assert!(
            !marker_path(dir.path()).exists(),
            "the clock must be reset once the pool serves again"
        );
    }

    #[tokio::test]
    async fn does_nothing_while_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        run(&host, dir.path(), &policy()).await.unwrap();
        assert!(!host.ran("force-create-pg"));
    }
}
