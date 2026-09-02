//! Grow the images RBD as the Ceph pool grows.
//!
//! Without this the feature is inert: add a disk, the pool grows, and the
//! image store stays exactly the same size forever. Only ever grows —
//! shrinking a mounted filesystem under a running containerd would corrupt
//! it, and a pool that shrank (a disk was removed) is exactly when you least
//! want to be truncating the image store.

use anyhow::Result;
use serde_json::Value;

use crate::host::Host;

use super::containerd_store::{containerd_root, Filesystem};
use super::images_sizing::{self, SizingPolicy};

pub struct GrowPolicy {
    pub pool_name: String,
    pub share_of_pool: f64,
    pub min_size_gb: u64,
    pub filesystem: Filesystem,
}

fn current_size_mb(rbd_info: &Value) -> Option<u64> {
    rbd_info["size"].as_u64().map(|b| b / 1_048_576)
}

pub async fn run<H: Host>(
    host: &H,
    root: &std::path::Path,
    node: &str,
    policy: &GrowPolicy,
) -> Result<()> {
    // The timer fires on a schedule and can land during bootstrap before the
    // admin keyring exists, or on a boot where the store never mounted. Both
    // are normal states, not failures.
    if !host.reachable().await {
        tracing::info!("images-grow: ceph not reachable yet — nothing to grow");
        return Ok(());
    }
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    if !host
        .run_cmd("mountpoint", &["-q", &croot_s])
        .await
        .is_ok_and(|o| o.success)
    {
        tracing::info!("images-grow: {croot_s} is not RBD-backed on this boot — nothing to grow");
        return Ok(());
    }

    let image = format!("{}/{node}", policy.pool_name);
    let info_out = host
        .run_cmd("rbd", &["info", &image, "--format", "json"])
        .await?;
    let info: Value = serde_json::from_str(&info_out.stdout)?;
    let Some(cur_mb) = current_size_mb(&info) else {
        tracing::warn!("images-grow: could not read {image}'s current size");
        return Ok(());
    };

    let sizing = SizingPolicy {
        pool_name: policy.pool_name.clone(),
        share_of_pool: policy.share_of_pool,
        min_size_gb: policy.min_size_gb,
    };
    let Some(want_mb) = images_sizing::compute(host, &sizing).await? else {
        tracing::info!("images-grow: could not read pool capacity — nothing to grow");
        return Ok(());
    };

    if want_mb > cur_mb {
        tracing::info!("images-grow: growing images RBD: {cur_mb}MB -> {want_mb}MB");
        host.run_cmd("rbd", &["resize", &image, "--size", &want_mb.to_string()])
            .await?;
        match policy.filesystem {
            Filesystem::Xfs => {
                host.run_cmd("xfs_growfs", &[&croot_s]).await?;
            }
            Filesystem::Ext4 => {
                let dev = host
                    .run_cmd("findmnt", &["-no", "SOURCE", &croot_s])
                    .await?
                    .stdout
                    .trim()
                    .to_string();
                host.run_cmd("resize2fs", &[&dev]).await?;
            }
        }
    } else {
        tracing::info!(
            "images-grow: images RBD already at {cur_mb}MB (target {want_mb}MB), nothing to do"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn policy() -> GrowPolicy {
        GrowPolicy {
            pool_name: "images".into(),
            share_of_pool: 0.25,
            min_size_gb: 40,
            filesystem: Filesystem::Xfs,
        }
    }

    #[test]
    fn current_size_mb_converts_bytes_to_mb() {
        let v = serde_json::json!({"size": 41_943_040_000u64}); // 40000 MB
        assert_eq!(current_size_mb(&v), Some(40_000));
    }

    #[test]
    fn current_size_mb_is_none_when_unreadable() {
        assert_eq!(current_size_mb(&Value::Null), None);
    }

    #[tokio::test]
    async fn does_nothing_while_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("rbd resize"));
    }

    #[tokio::test]
    async fn does_nothing_when_not_rbd_backed() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .fail("mountpoint -q", "not mounted");
        let dir = tempfile::tempdir().unwrap();
        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("rbd resize"));
    }

    #[tokio::test]
    async fn grows_when_the_pool_has_room() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("mountpoint -q", "")
            .ok("rbd info images/yolab-n1", r#"{"size":41943040000}"#) // 40000MB
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":419430400000}}"#) // -> want 100000MB
            .ok("ceph osd pool get images size", r#"{"size":1}"#)
            .ok("rbd resize", "")
            .ok("xfs_growfs", "");

        run(
            &host,
            tempfile::tempdir().unwrap().path(),
            "yolab-n1",
            &policy(),
        )
        .await
        .unwrap();

        assert!(host.ran("rbd resize images/yolab-n1 --size 100000"));
        assert!(host.ran("xfs_growfs"));
    }

    #[tokio::test]
    async fn never_shrinks_when_the_pool_has_less_room_than_before() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("mountpoint -q", "")
            .ok("rbd info images/yolab-n1", r#"{"size":419430400000}"#) // 400000MB, already large
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":419430400000}}"#) // want only 100000MB
            .ok("ceph osd pool get images size", r#"{"size":1}"#);

        run(
            &host,
            tempfile::tempdir().unwrap().path(),
            "yolab-n1",
            &policy(),
        )
        .await
        .unwrap();

        assert!(
            !host.ran("rbd resize"),
            "shrinking a mounted filesystem must never happen"
        );
    }
}
