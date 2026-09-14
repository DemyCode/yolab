//! Start a yolab-ceph-osd@ instance for every OSD ceph-volume reports as
//! prepared on this host, except one the cluster no longer knows (teardown
//! purges from Ceph *before* it zaps the disk, so a half-removed OSD is still
//! "prepared" and must not be restarted).
//!
//! Runs at boot as `yolab-ceph-osd-activate.service` (before k3s: the image
//! store on Ceph needs OSDs up first), and afterwards as the `osd-activate`
//! controller. Instances are started, never enabled — /etc/systemd/system is a
//! read-only store path on NixOS, so this enumeration IS the persistence.

use anyhow::{Context, Result};

use crate::ceph::model::parse_lvm_list;
use crate::error::Outcome;
use crate::host::Host;

/// OSD ids from `ceph-volume lvm list --format json`, sorted. A listing that
/// cannot be parsed is an error — the old version returned an empty list, which
/// started nothing and reported success.
fn lvm_osd_ids(raw: &str) -> Result<Vec<i64>> {
    Ok(parse_lvm_list(raw)?.into_keys().collect())
}

/// Whether a prepared OSD should be started. `known` is the cluster's OSD list:
/// `None` means it could not be read (the boot case, before the mon answers),
/// and an empty list means a cluster with no OSDs — both start everything.
/// Starting an OSD the cluster no longer knows is harmless (the mon refuses
/// its key); NOT starting a real one on a cold boot keeps the whole node's
/// storage down.
fn should_start(id: i64, known: Option<&[i64]>) -> bool {
    match known {
        None => true,
        Some(known) => known.is_empty() || known.contains(&id),
    }
}

pub async fn run<H: Host>(host: &H) -> Result<()> {
    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await
        .context("ceph-volume lvm list")?;
    let prepared = lvm_osd_ids(&raw)?;
    if prepared.is_empty() {
        return Ok(());
    }

    let known = host.osd_ids().await.ok_or_warn(
        "osd-activate: the cluster's OSD list is unreadable — starting every prepared OSD",
    );
    for id in prepared {
        if !should_start(id, known.as_deref()) {
            tracing::info!(
                "osd.{id}: prepared on this disk but no longer part of the cluster — not starting it"
            );
            continue;
        }
        let unit = format!("yolab-ceph-osd@{id}.service");
        match host.systemctl(&["start", "--no-block", &unit]).await {
            Ok(o) if o.success => tracing::info!("started {unit}"),
            Ok(o) => tracing::warn!("could not start {unit}: {}", o.stderr.trim()),
            Err(e) => tracing::warn!("could not start {unit}: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn lvm_osd_ids_reads_the_object_keys() {
        let raw = "--> progress\n{\"5\":[{}],\"7\":[{}]}";
        assert_eq!(lvm_osd_ids(raw).unwrap(), vec![5, 7]);
    }

    #[test]
    fn a_non_numeric_osd_key_is_an_error() {
        let raw = "{\"5\":[{}],\"not-a-number\":[{}]}";
        assert!(lvm_osd_ids(raw).is_err());
    }

    #[test]
    fn a_listing_without_json_is_an_error_not_an_empty_host() {
        assert!(lvm_osd_ids("no json here").is_err());
        assert_eq!(lvm_osd_ids("{}").unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn should_start_is_true_when_the_cluster_is_unreadable() {
        assert!(should_start(1, None));
    }

    #[test]
    fn should_start_is_true_on_an_empty_cluster() {
        assert!(should_start(1, Some(&[])));
    }

    #[test]
    fn should_start_filters_against_a_known_cluster() {
        assert!(should_start(1, Some(&[1, 2])));
        assert!(!should_start(3, Some(&[1, 2])));
    }

    #[tokio::test]
    async fn a_failed_listing_fails_the_run_instead_of_starting_nothing_quietly() {
        let host = FakeHost::new().fail("ceph-volume lvm list", "timed out");
        assert!(run(&host).await.is_err());
        assert!(!host.ran("systemctl start"));
    }

    #[tokio::test]
    async fn starts_known_osds_and_skips_purged_ones() {
        let host = FakeHost::new()
            .ok("ceph-volume lvm list", "{\"1\":[{}],\"4\":[{}]}")
            .ok("ceph osd ls", "[1,2]")
            .ok("systemctl start", "");
        run(&host).await.unwrap();
        assert!(host.ran("systemctl start --no-block yolab-ceph-osd@1.service"));
        assert!(!host.ran("yolab-ceph-osd@4.service"));
    }
}
