//! CephFS bootstrap — the filesystem behind every app PVC.
//!
//! Replaces the `yolab-cephfs-init` systemd unit, which was a bash script on a
//! timer. Idempotent: creates the pools, the filesystem and the `csi`
//! subvolume group once a disk is available.
//!
//! Cluster-scoped (one writer), and paused while a restore or a storage recovery
//! runs: the recovery deletes and recreates this very filesystem, and creating one
//! here between its steps would race it.
//!
//! SIZE IS SET ONLY ON POOLS THIS TICK CREATED. It used to run `pool set size 1`
//! on both pools whenever the filesystem was missing — including pools that
//! already existed with the owner's chosen copy count, which deleted the extra
//! replicas until the topology controller put them back.

use std::time::Duration;

use anyhow::Result;

use crate::ceph::model::{FsEntry, OsdStat};
use crate::runtime::{Activity, Controller, Ctx, Requirement, Scope, Tick};

const FS_NAME: &str = "yolab-fs";
const META_POOL: &str = "yolab-fs-metadata";
const DATA_POOL: &str = "yolab-fs-data0";
const SUBVOLUME_GROUP: &str = "csi";

fn pool_listed(pool_ls: &str, name: &str) -> bool {
    pool_ls.lines().any(|l| l.trim() == name)
}

fn has_named(entries: &[FsEntry], name: &str) -> bool {
    entries.iter().any(|e| e.name == name)
}

pub struct CephFsController;

impl Controller for CephFsController {
    fn name(&self) -> &'static str {
        "cephfs"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph, Requirement::KubeApi]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[Activity::Restore, Activity::Heal]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        ensure().await
    }
}

pub(crate) async fn ensure() -> Result<Tick> {
    let stat: OsdStat = crate::ceph_cli::ceph_typed(&["osd", "stat"]).await?;
    if stat.num_up_osds == 0 {
        return Ok(Tick::Idle("no OSD is up yet".into()));
    }

    let fs_ls: Vec<FsEntry> = crate::ceph_cli::ceph_typed(&["fs", "ls"]).await?;
    if has_named(&fs_ls, FS_NAME) {
        ensure_subvolumegroup().await?;
        return Ok(Tick::Done);
    }

    let pool_ls = crate::ceph_cli::ceph(&["osd", "pool", "ls"]).await?;
    for (pool, pgs) in [(META_POOL, "16"), (DATA_POOL, "32")] {
        if pool_listed(&pool_ls, pool) {
            continue;
        }
        crate::ceph_cli::ceph(&["osd", "pool", "create", pool, pgs, pgs]).await?;
        // New and empty, so size 1 loses nothing; the topology controller raises
        // it to the owner's chosen count on its next tick.
        crate::ceph_cli::ceph(&[
            "osd",
            "pool",
            "set",
            pool,
            "size",
            "1",
            "--yes-i-really-mean-it",
        ])
        .await?;
    }
    crate::ceph_cli::ceph(&["fs", "new", FS_NAME, META_POOL, DATA_POOL, "--force"]).await?;
    tracing::info!("created CephFS {FS_NAME}");
    ensure_subvolumegroup().await?;
    Ok(Tick::Done)
}

async fn ensure_subvolumegroup() -> Result<()> {
    let ls: Vec<FsEntry> =
        crate::ceph_cli::ceph_typed(&["fs", "subvolumegroup", "ls", FS_NAME]).await?;
    if has_named(&ls, SUBVOLUME_GROUP) {
        return Ok(());
    }
    crate::ceph_cli::ceph(&["fs", "subvolumegroup", "create", FS_NAME, SUBVOLUME_GROUP]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(raw: &str) -> Vec<FsEntry> {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn has_named_finds_a_matching_entry() {
        let v = entries(r#"[{"name": "yolab-fs"}, {"name": "other"}]"#);
        assert!(has_named(&v, "yolab-fs"));
        assert!(!has_named(&v, "missing"));
    }

    #[test]
    fn a_listing_that_is_not_a_list_is_an_error_not_an_empty_one() {
        assert!(serde_json::from_str::<Vec<FsEntry>>("{}").is_err());
        assert!(serde_json::from_str::<Vec<FsEntry>>("\"nope\"").is_err());
    }

    #[test]
    fn pool_listed_matches_whole_lines_only() {
        let ls = "yolab-fs-metadata\nyolab-fs-data0\nimages\n";
        assert!(pool_listed(ls, "yolab-fs-metadata"));
        assert!(!pool_listed(ls, "metadata"));
        assert!(!pool_listed(ls, "yolab-fs-metadata-extra"));
    }
}
