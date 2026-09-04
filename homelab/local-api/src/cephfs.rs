//! CephFS bootstrap — the filesystem behind every app PVC.
//!
//! Replaces the `yolab-cephfs-init` systemd unit, which was a bash script on a
//! timer. It is idempotent and runs periodically from local-api, creating the
//! pools, filesystem and the `csi` subvolume group once a disk is available.

use anyhow::Result;
use serde_json::Value;

const FS_NAME: &str = "yolab-fs";
const META_POOL: &str = "yolab-fs-metadata";
const DATA_POOL: &str = "yolab-fs-data0";
const SUBVOLUME_GROUP: &str = "csi";

fn has_named(v: &Value, name: &str) -> bool {
    v.as_array()
        .map(|a| a.iter().any(|e| e["name"].as_str() == Some(name)))
        .unwrap_or(false)
}

fn pool_listed(pool_ls: &str, name: &str) -> bool {
    pool_ls.lines().any(|l| l.trim() == name)
}

pub async fn run() {
    tokio::time::sleep(std::time::Duration::from_secs(90)).await;
    loop {
        // Gated here, not inside `ensure()` itself: restore_run.rs calls `ensure()`
        // directly as part of its own RebuildingStorage recovery — after purging OSDs
        // and tearing the filesystem down, it needs `ensure()` to recreate it, and a
        // check inside `ensure()` would refuse that exact call (a restore is active by
        // definition while it's running) and deadlock the rebuild it's trying to finish.
        // This loop's own periodic background calls are what would race a rebuild in
        // progress, so only they are skipped.
        if crate::routers::restore_run::is_active().await {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            continue;
        }
        if let Err(e) = ensure().await {
            tracing::debug!("cephfs: {e}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    }
}

pub(crate) async fn ensure() -> Result<()> {
    if crate::ceph_cli::ceph(&["-s"]).await.is_err() {
        return Ok(());
    }
    let up = crate::ceph_cli::ceph_json(&["osd", "stat"])
        .await
        .ok()
        .and_then(|v| v["num_up_osds"].as_u64())
        .unwrap_or(0);
    if up == 0 {
        return Ok(());
    }

    let fs_ls = crate::ceph_cli::ceph_json(&["fs", "ls"])
        .await
        .unwrap_or_else(|_| serde_json::json!([]));
    if has_named(&fs_ls, FS_NAME) {
        ensure_subvolumegroup().await?;
        return Ok(());
    }

    let pool_ls = crate::ceph_cli::ceph(&["osd", "pool", "ls"]).await?;
    if !pool_listed(&pool_ls, META_POOL) {
        crate::ceph_cli::ceph(&["osd", "pool", "create", META_POOL, "16", "16"]).await?;
    }
    if !pool_listed(&pool_ls, DATA_POOL) {
        crate::ceph_cli::ceph(&["osd", "pool", "create", DATA_POOL, "32", "32"]).await?;
    }
    crate::ceph_cli::ceph(&[
        "osd",
        "pool",
        "set",
        META_POOL,
        "size",
        "1",
        "--yes-i-really-mean-it",
    ])
    .await?;
    crate::ceph_cli::ceph(&[
        "osd",
        "pool",
        "set",
        DATA_POOL,
        "size",
        "1",
        "--yes-i-really-mean-it",
    ])
    .await?;
    crate::ceph_cli::ceph(&["fs", "new", FS_NAME, META_POOL, DATA_POOL, "--force"]).await?;
    tracing::info!("created CephFS {FS_NAME}");
    ensure_subvolumegroup().await?;
    Ok(())
}

async fn ensure_subvolumegroup() -> Result<()> {
    let ls = match crate::ceph_cli::ceph_json(&["fs", "subvolumegroup", "ls", FS_NAME]).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if has_named(&ls, SUBVOLUME_GROUP) {
        return Ok(());
    }
    crate::ceph_cli::ceph(&["fs", "subvolumegroup", "create", FS_NAME, SUBVOLUME_GROUP]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_named_finds_a_matching_entry() {
        let v = serde_json::json!([{"name": "yolab-fs"}, {"name": "other"}]);
        assert!(has_named(&v, "yolab-fs"));
        assert!(!has_named(&v, "missing"));
    }

    #[test]
    fn has_named_treats_a_non_array_as_absent() {
        assert!(!has_named(&serde_json::json!({}), "yolab-fs"));
        assert!(!has_named(&serde_json::json!("nope"), "yolab-fs"));
    }

    #[test]
    fn pool_listed_matches_whole_lines_only() {
        let ls = "yolab-fs-metadata\nyolab-fs-data0\nimages\n";
        assert!(pool_listed(ls, "yolab-fs-metadata"));
        assert!(!pool_listed(ls, "metadata"));
        assert!(!pool_listed(ls, "yolab-fs-metadata-extra"));
    }
}
