//! Hold Ceph's `noout` flag across a reboot.
//!
//! An OSD down for `mon_osd_down_out_interval` (600s) is marked `out`, and
//! Ceph starts copying its data onto the remaining disks. Right for a dead
//! disk, wrong for a reboot that comes back in two minutes — with
//! `osd_max_backfills=4` the pointless rebalance is aggressive, and on a
//! multi-node cluster it saturates the WireGuard links copying data that was
//! never lost. `set` runs as the unit's ExecStop (ceph-noout-set, at
//! shutdown), `clear` as its ExecStart (ceph-noout-clear, at the next boot).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::host::Host;

fn marker_path(root: &Path) -> PathBuf {
    root.join("var/lib/ceph/.yolab-set-noout")
}

/// Reads `ceph osd dump`'s plain-text `flags` line. Not the JSON form: the
/// shell this replaces used `ceph osd dump | grep -q '^flags.*noout'`, and the
/// flags line has no `-f json` equivalent worth parsing over grepping for the
/// substring.
fn already_set(osd_dump: &str) -> bool {
    osd_dump
        .lines()
        .any(|l| l.starts_with("flags") && l.contains("noout"))
}

async fn wait_reachable<H: Host>(host: &H, attempts: u32) -> bool {
    for _ in 0..attempts {
        if host.reachable().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// ExecStart: clear noout, but only if a previous `set` (this same marker) is
/// what turned it on — an operator's own `ceph osd set noout` for a
/// maintenance window that outlives this reboot must not be stomped.
pub async fn clear<H: Host>(host: &H, root: &Path) -> Result<()> {
    if !wait_reachable(host, 60).await {
        tracing::info!("noout-clear: ceph unreachable — leaving flags alone");
        return Ok(());
    }
    let marker = marker_path(root);
    if marker.exists() {
        let _ = host.ceph(&["osd", "unset", "noout"]).await;
        let _ = std::fs::remove_file(&marker);
        tracing::info!("noout-clear: cleared noout");
    }
    Ok(())
}

/// ExecStop: set noout before the daemons on this node go down for the
/// reboot. Never blocks waiting for reachability — a shutdown that is already
/// underway must not hang on a cluster that happens to be unreachable right
/// now (see maintenance.nix's TimeoutStopSec note on why ExecStop must stay
/// bounded).
pub async fn set<H: Host>(host: &H, root: &Path) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("noout-set: ceph unreachable — leaving flags alone");
        return Ok(());
    }
    if let Ok(dump) = host.ceph(&["osd", "dump"]).await {
        if already_set(&dump) {
            tracing::info!("noout-set: noout already set by someone else — leaving it");
            return Ok(());
        }
    }
    let _ = host.ceph(&["osd", "set", "noout"]).await;
    std::fs::create_dir_all(root.join("var/lib/ceph"))?;
    std::fs::write(marker_path(root), "")?;
    tracing::info!("noout-set: set noout for shutdown");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn already_set_reads_the_flags_line() {
        assert!(already_set("flags noout,sortbitwise"));
        assert!(!already_set("flags sortbitwise"));
    }

    #[test]
    fn already_set_ignores_noout_mentioned_elsewhere() {
        // Only the line that actually starts with "flags" counts — a stray
        // mention of "noout" in some other line (e.g. a log excerpt Ceph
        // echoes back) must not be read as the flag being live.
        assert!(!already_set(
            "epoch 12\nsomething about noout here\nflags sortbitwise"
        ));
    }

    // `clear` retries reachability up to 60 times with a 1s sleep between —
    // paused time so this resolves instantly instead of taking a minute.
    #[tokio::test(start_paused = true)]
    async fn clear_does_nothing_when_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        clear(&host, dir.path()).await.unwrap();
        assert!(!host.ran("osd unset noout"));
    }

    #[tokio::test]
    async fn clear_does_nothing_without_a_marker_file() {
        let host = FakeHost::new().ok("ceph -s", "");
        let dir = tempfile::tempdir().unwrap();
        clear(&host, dir.path()).await.unwrap();
        assert!(!host.ran("osd unset noout"));
    }

    #[tokio::test]
    async fn clear_unsets_and_removes_the_marker_it_owns() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd unset noout", "");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/ceph")).unwrap();
        std::fs::write(marker_path(dir.path()), "").unwrap();

        clear(&host, dir.path()).await.unwrap();

        assert!(host.ran("osd unset noout"));
        assert!(!marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn set_leaves_an_operators_own_noout_alone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd dump", "flags noout,sortbitwise");
        let dir = tempfile::tempdir().unwrap();

        set(&host, dir.path()).await.unwrap();

        assert!(!host.ran("osd set noout"));
        assert!(!marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn set_sets_the_flag_and_drops_a_marker() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd dump", "flags sortbitwise")
            .ok("ceph osd set noout", "");
        let dir = tempfile::tempdir().unwrap();

        set(&host, dir.path()).await.unwrap();

        assert!(host.ran("osd set noout"));
        assert!(marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn set_never_blocks_on_an_unreachable_cluster() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        set(&host, dir.path()).await.unwrap();
        assert!(!host.ran("osd set noout"));
    }
}
