//! Ensure the Ceph images pool and this node's RBD image exist.
//!
//! No dependency on any OSD unit: OSD instances are enabled dynamically by
//! local-api, so there is no single unit to order against. This waits for an
//! OSD to actually report up instead, which is the real condition — see
//! images-store.nix for why (a pool cannot hold anything until one exists).

use std::time::Duration;

use anyhow::{bail, Result};

use crate::host::Host;

use super::ceph_shared::POOL_PROBE_TIMEOUT;
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

    // BOUNDED, AND THE BOUND IS LOAD-BEARING — this unit gates k3s.
    //
    // `yolab-images-rbd` is ordered before `yolab-containerd-store`, which k3s is
    // ordered behind. So an unbounded probe here does not merely make THIS unit
    // slow, it holds the node out of the cluster: on node2 (2026-09-11, started
    // 10:08:50) this call sat in `start` while `yolab-containerd-store`, `k3s` and
    // `yolab-local-api` were all listed `waiting` behind it, with no failed units
    // anywhere. Bounding the probe in containerd_store alone fixed nothing on a
    // cold boot, because this one runs first.
    //
    // A failure here is the right outcome and needs no special handling: `?`
    // propagates, the unit fails, the timer re-runs it in two minutes, and the
    // units queued behind it are released to do the best they can without Ceph —
    // which, for the image store, is to stay on the root disk and re-pull. See
    // POOL_PROBE_TIMEOUT for why 30s is the whole of the useful budget.
    //
    // `.success` IS CHECKED SEPARATELY FROM `?`, and the distinction is not
    // pedantry. `?` only fires when the command could not be run or timed out; a
    // pool that answers with an ERROR comes back `Ok` with `success == false` and
    // an empty stdout — which reads as "this node's image is not in the list" and
    // sends us straight into `rbd create` against a pool that just said no. Same
    // conflation of "could not answer" with "absent" that cost 23 hours in
    // containerd_store.rs, one file over.
    let existing = host
        .run_cmd_bounded("rbd", &["ls", &policy.pool_name], POOL_PROBE_TIMEOUT)
        .await?;
    if !existing.success {
        bail!(
            "images-rbd: cannot list {}: {}",
            policy.pool_name,
            existing.stderr.trim()
        );
    }
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

    /// 2026-09-11: this unit gates k3s, so an unanswerable pool must FAIL here
    /// rather than be waited out — and must never be mistaken for "no image yet".
    ///
    /// On node2 this call blocked while `yolab-containerd-store`, `k3s` and
    /// `yolab-local-api` all sat `waiting` behind it in the job queue. Failing
    /// releases them; creating an image on a pool that cannot answer would just
    /// hang again on the write.
    #[tokio::test]
    async fn a_pool_that_cannot_answer_fails_instead_of_creating_an_image() {
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
            .fail("rbd ls images", "timed out");

        let result = run(&host, "yolab-n1", &policy()).await;

        assert!(
            result.is_err(),
            "a pool that cannot serve a read must fail the unit, not stall it"
        );
        assert!(
            !host.ran("rbd create"),
            "an unanswerable probe is not evidence the image is missing"
        );
    }

    #[test]
    fn the_pool_probe_gives_up_long_before_the_generic_command_bound() {
        assert!(
            POOL_PROBE_TIMEOUT.as_secs() < 600,
            "this unit is ordered ahead of k3s; waiting out the 600s run_cmd bound \
             holds the whole node out of the cluster"
        );
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
