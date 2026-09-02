//! Ensure the Ceph images pool and this node's RBD image exist.
//!
//! No dependency on any OSD unit: OSD instances are enabled dynamically by
//! local-api, so there is no single unit to order against. This waits for an
//! OSD to actually report up instead, which is the real condition — see
//! images-store.nix for why (a pool cannot hold anything until one exists).

use std::time::Duration;

use anyhow::{bail, Result};

use crate::host::Host;

use super::images_sizing::{self, SizingPolicy};

pub struct ImagesRbdPolicy {
    pub pool_name: String,
    pub share_of_pool: f64,
    pub min_size_gb: u64,
}

async fn wait_reachable<H: Host>(host: &H, attempts: u32) -> bool {
    for _ in 0..attempts {
        if host.reachable().await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn any_osd_up<H: Host>(host: &H) -> bool {
    host.ceph_json(&["osd", "stat"])
        .await
        .ok()
        .and_then(|v| v["num_up_osds"].as_u64())
        .unwrap_or(0)
        > 0
}

async fn wait_osd_up<H: Host>(host: &H, attempts: u32) -> bool {
    for _ in 0..attempts {
        if any_osd_up(host).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn run_ok<H: Host>(host: &H, bin: &str, args: &[&str]) -> Result<()> {
    let out = host.run_cmd(bin, args).await?;
    if !out.success {
        bail!("{bin} {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(())
}

pub async fn run<H: Host>(host: &H, node: &str, policy: &ImagesRbdPolicy) -> Result<()> {
    if !wait_reachable(host, 90).await {
        tracing::info!("images-rbd: ceph not reachable — nothing to provision yet");
        return Ok(());
    }
    if !wait_osd_up(host, 90).await {
        tracing::info!(
            "images-rbd: no OSD is up yet — the images pool will be created once a disk is switched on"
        );
        return Ok(());
    }

    let pools = host.ceph(&["osd", "pool", "ls"]).await?;
    if !pools.lines().any(|l| l.trim() == policy.pool_name) {
        run_ok(
            host,
            "ceph",
            &["osd", "pool", "create", &policy.pool_name, "32", "32"],
        )
        .await?;
        run_ok(
            host,
            "ceph",
            &[
                "osd",
                "pool",
                "set",
                &policy.pool_name,
                "size",
                "1",
                "--yes-i-really-mean-it",
            ],
        )
        .await?;
        run_ok(
            host,
            "ceph",
            &[
                "osd",
                "pool",
                "application",
                "enable",
                &policy.pool_name,
                "rbd",
            ],
        )
        .await?;
        run_ok(host, "rbd", &["pool", "init", &policy.pool_name]).await?;
    }

    // Size from capacity that actually exists — see images-store.nix's header,
    // point 2. `None` means the capacity read failed; nothing to size yet,
    // and a later tick (the timer re-runs this) picks it up.
    let sizing = SizingPolicy {
        pool_name: policy.pool_name.clone(),
        share_of_pool: policy.share_of_pool,
        min_size_gb: policy.min_size_gb,
    };
    let Some(want_mb) = images_sizing::compute(host, &sizing).await? else {
        tracing::info!("images-rbd: could not read pool capacity — not sizing anything");
        return Ok(());
    };

    let existing = host.run_cmd("rbd", &["ls", &policy.pool_name]).await?;
    if !existing.stdout.lines().any(|l| l.trim() == node) {
        // krbd cannot map object-map/fast-diff/deep-flatten, so create with
        // only the features the kernel client supports — getting this wrong
        // produces a map failure that reads like a permissions error.
        run_ok(
            host,
            "rbd",
            &[
                "create",
                &format!("{}/{node}", policy.pool_name),
                "--size",
                &want_mb.to_string(),
                "--image-feature",
                "layering,exclusive-lock",
            ],
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn policy() -> ImagesRbdPolicy {
        ImagesRbdPolicy {
            pool_name: "images".into(),
            share_of_pool: 0.25,
            min_size_gb: 40,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn does_nothing_while_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        run(&host, "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("pool create"));
    }

    #[tokio::test(start_paused = true)]
    async fn does_nothing_before_any_osd_is_up() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", r#"{"num_up_osds":0}"#);
        run(&host, "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("pool create"));
    }

    #[tokio::test]
    async fn creates_the_pool_only_when_it_does_not_exist_yet() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", r#"{"num_up_osds":1}"#)
            .ok("ceph osd pool ls", "images\n")
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":1048576000000}}"#)
            .ok("ceph osd pool get images size", r#"{"size":1}"#)
            .ok("rbd ls images", "yolab-n1\n");

        run(&host, "yolab-n1", &policy()).await.unwrap();

        assert!(
            !host.ran("pool create"),
            "the pool already exists, must not be recreated"
        );
        assert!(!host.ran("rbd create"), "this node's image already exists");
    }

    #[tokio::test]
    async fn creates_the_pool_and_image_from_scratch() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", r#"{"num_up_osds":1}"#)
            .ok("ceph osd pool ls", "") // pool absent
            .ok("ceph osd pool create images 32 32", "")
            .ok("ceph osd pool set images size 1 --yes-i-really-mean-it", "")
            .ok("ceph osd pool application enable images rbd", "")
            .ok("rbd pool init images", "")
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":1048576000000}}"#)
            .ok("ceph osd pool get images size", r#"{"size":1}"#)
            .ok("rbd ls images", "") // this node's image absent
            .ok("rbd create images/yolab-n1", "");

        run(&host, "yolab-n1", &policy()).await.unwrap();

        assert!(host.ran("ceph osd pool create images 32 32"));
        assert!(host.ran("rbd create images/yolab-n1 --size 250000"));
    }

    #[tokio::test]
    async fn sizes_nothing_when_capacity_cannot_be_read() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", r#"{"num_up_osds":1}"#)
            .ok("ceph osd pool ls", "images\n")
            .ok("ceph osd tree", r#"{"nodes":[]}"#)
            .fail("ceph df", "no osds");

        run(&host, "yolab-n1", &policy()).await.unwrap();

        assert!(!host.ran("rbd create"));
    }
}
