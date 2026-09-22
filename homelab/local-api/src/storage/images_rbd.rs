
use anyhow::{bail, Result};

use crate::ceph::model::OsdStat;
use crate::host::Host;

use super::ceph_shared::POOL_PROBE_TIMEOUT;
use super::images_sizing::{self, SizingPolicy};
use super::wait::Attempt;

pub struct ImagesRbdPolicy {
    pub pool_name: String,
    pub share_of_pool: f64,
    pub min_size_gb: u64,
}

async fn run_ok<H: Host>(host: &H, bin: &str, args: &[&str]) -> Result<()> {
    let out = host.run_cmd(bin, args).await?;
    if !out.success {
        bail!("{bin} {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(())
}

pub async fn attempt<H: Host>(
    host: &H,
    node: &str,
    policy: &ImagesRbdPolicy,
) -> Result<Attempt<()>> {
    if !host.reachable().await {
        return Ok(Attempt::NotYet("Ceph is not answering".into()));
    }
    let stat: OsdStat = serde_json::from_value(host.ceph_json(&["osd", "stat"]).await?)?;
    if stat.num_up_osds == 0 {
        return Ok(Attempt::NotYet(
            "no OSD is up yet (this node's system disk becomes one at boot)".into(),
        ));
    }

    let pools = host.ceph(&["osd", "pool", "ls"]).await?;
    if !pools.lines().any(|l| l.trim() == policy.pool_name) {
        let pool = policy.pool_name.as_str();
        run_ok(host, "ceph", &["osd", "pool", "create", pool, "32", "32"]).await?;
        run_ok(
            host,
            "ceph",
            &[
                "osd",
                "pool",
                "set",
                pool,
                "size",
                "1",
                "--yes-i-really-mean-it",
            ],
        )
        .await?;
        run_ok(
            host,
            "ceph",
            &["osd", "pool", "application", "enable", pool, "rbd"],
        )
        .await?;
        run_ok(host, "rbd", &["pool", "init", pool]).await?;
    }

    let sizing = SizingPolicy {
        pool_name: policy.pool_name.clone(),
        share_of_pool: policy.share_of_pool,
        min_size_gb: policy.min_size_gb,
    };
    let Some(want_mb) = images_sizing::compute(host, &sizing).await? else {
        return Ok(Attempt::NotYet(format!(
            "the capacity of the {} pool cannot be read yet",
            policy.pool_name
        )));
    };

    let existing = host
        .run_cmd_bounded("rbd", &["ls", &policy.pool_name], POOL_PROBE_TIMEOUT)
        .await?;
    if !existing.success {
        return Ok(Attempt::NotYet(format!(
            "the {} pool cannot list its images: {}",
            policy.pool_name,
            existing.stderr.trim()
        )));
    }
    if !existing.stdout.lines().any(|l| l.trim() == node) {
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
        tracing::info!(
            "images-rbd: created {}/{node} ({want_mb} MB)",
            policy.pool_name
        );
    }
    Ok(Attempt::Ready(()))
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

    fn not_yet(a: Attempt<()>) -> String {
        match a {
            Attempt::NotYet(why) => why,
            Attempt::Ready(()) => panic!("expected NotYet"),
        }
    }

    #[tokio::test]
    async fn waits_while_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let why = not_yet(attempt(&host, "yolab-n1", &policy()).await.unwrap());
        assert!(why.contains("not answering"), "{why}");
        assert!(!host.ran("pool create"));
    }

    #[tokio::test]
    async fn waits_until_an_osd_is_up_instead_of_giving_up() {
        let host = FakeHost::new().ok("ceph -s", "").ok(
            "ceph osd stat",
            r#"{"num_osds":1,"num_up_osds":0,"num_in_osds":1}"#,
        );
        let why = not_yet(attempt(&host, "yolab-n1", &policy()).await.unwrap());
        assert!(why.contains("no OSD is up"), "{why}");
        assert!(!host.ran("pool create"));
    }

    #[tokio::test]
    async fn an_unreadable_osd_stat_is_an_error_not_zero_osds() {
        let host = FakeHost::new().ok("ceph -s", "").ok("ceph osd stat", "{}");
        assert!(attempt(&host, "yolab-n1", &policy()).await.is_err());
    }

    const ONE_UP: &str = r#"{"num_osds":1,"num_up_osds":1,"num_in_osds":1}"#;

    #[tokio::test]
    async fn creates_the_pool_only_when_it_does_not_exist_yet() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", ONE_UP)
            .ok("ceph osd pool ls", "images\n")
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":1048576000000}}"#)
            .ok("ceph osd pool get images size", r#"{"size":1}"#)
            .ok("rbd ls images", "yolab-n1\n");

        let ready = attempt(&host, "yolab-n1", &policy()).await.unwrap();

        assert_eq!(ready, Attempt::Ready(()));
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
            .ok("ceph osd stat", ONE_UP)
            .ok("ceph osd pool ls", "")
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
            .ok("rbd ls images", "")
            .ok("rbd create images/yolab-n1", "");

        let ready = attempt(&host, "yolab-n1", &policy()).await.unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(host.ran("ceph osd pool create images 32 32"));
        assert!(host.ran("rbd create images/yolab-n1 --size 250000"));
    }

    #[tokio::test]
    async fn a_failed_pool_create_is_an_error_and_nothing_else_runs() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", ONE_UP)
            .ok("ceph osd pool ls", "")
            .fail("ceph osd pool create", "Error ERANGE: pg_num exceeds max");
        assert!(attempt(&host, "yolab-n1", &policy()).await.is_err());
        assert!(!host.ran("rbd create"));
    }

    #[tokio::test]
    async fn a_pool_that_cannot_answer_is_waited_on_instead_of_creating_an_image() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", ONE_UP)
            .ok("ceph osd pool ls", "images\n")
            .ok(
                "ceph osd tree",
                r#"{"nodes":[{"type":"host","children":[1]}]}"#,
            )
            .ok("ceph df", r#"{"stats":{"total_bytes":1048576000000}}"#)
            .ok("ceph osd pool get images size", r#"{"size":1}"#)
            .fail(
                "rbd ls images",
                "rbd: error opening pool: (110) Connection timed out",
            );

        let result = attempt(&host, "yolab-n1", &policy()).await;

        assert!(
            !host.ran("rbd create"),
            "an unanswerable probe is not evidence the image is missing"
        );
        assert!(!matches!(result, Ok(Attempt::Ready(()))));
    }

    #[test]
    fn the_pool_probe_gives_up_long_before_the_generic_command_bound() {
        assert!(
            POOL_PROBE_TIMEOUT.as_secs() < 600,
            "each attempt must end quickly so the wait can report and retry"
        );
    }

    #[tokio::test]
    async fn waits_when_capacity_cannot_be_read() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd stat", ONE_UP)
            .ok("ceph osd pool ls", "images\n")
            .ok("ceph osd tree", r#"{"nodes":[]}"#)
            .fail("ceph df", "no osds");

        let result = attempt(&host, "yolab-n1", &policy()).await;

        assert!(!matches!(result, Ok(Attempt::Ready(()))));
        assert!(!host.ran("rbd create"));
    }
}
