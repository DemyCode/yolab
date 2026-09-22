use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::host::Host;

fn marker_path(root: &Path) -> PathBuf {
    root.join("var/lib/ceph/.yolab-set-noout")
}

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

pub async fn clear<H: Host>(host: &H, root: &Path) -> Result<()> {
    if !wait_reachable(host, 60).await {
        tracing::info!("noout-clear: ceph unreachable — leaving flags alone");
        return Ok(());
    }
    let marker = marker_path(root);
    if !marker.exists() {
        return Ok(());
    }
    host.ceph(&["osd", "unset", "noout"])
        .await
        .context("noout-clear: ceph osd unset noout")?;
    std::fs::remove_file(&marker)
        .with_context(|| format!("noout-clear: remove {}", marker.display()))?;
    tracing::info!("noout-clear: cleared noout");
    Ok(())
}

pub async fn set<H: Host>(host: &H, root: &Path) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("noout-set: ceph unreachable — leaving flags alone");
        return Ok(());
    }
    match host.ceph(&["osd", "dump"]).await {
        Ok(dump) if already_set(&dump) => {
            tracing::info!("noout-set: noout already set by someone else — leaving it");
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("noout-set: could not read the flags ({e}) — leaving them alone");
            return Ok(());
        }
    }
    host.ceph(&["osd", "set", "noout"])
        .await
        .context("noout-set: ceph osd set noout")?;
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
        assert!(!already_set(
            "epoch 12\nsomething about noout here\nflags sortbitwise"
        ));
    }

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
    async fn a_failed_unset_keeps_the_marker_so_the_next_boot_retries() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .fail("ceph osd unset noout", "Error EACCES: access denied");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/ceph")).unwrap();
        std::fs::write(marker_path(dir.path()), "").unwrap();

        assert!(clear(&host, dir.path()).await.is_err());
        assert!(marker_path(dir.path()).exists());
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
    async fn a_failed_set_claims_nothing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd dump", "flags sortbitwise")
            .fail("ceph osd set noout", "Error EACCES: access denied");
        let dir = tempfile::tempdir().unwrap();

        assert!(set(&host, dir.path()).await.is_err());
        assert!(!marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn unreadable_flags_claim_nothing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .fail("ceph osd dump", "timed out");
        let dir = tempfile::tempdir().unwrap();

        set(&host, dir.path()).await.unwrap();
        assert!(!host.ran("osd set noout"));
        assert!(!marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn set_never_blocks_on_an_unreachable_cluster() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        set(&host, dir.path()).await.unwrap();
        assert!(!host.ran("osd set noout"));
    }
}
