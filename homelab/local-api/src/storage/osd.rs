use std::collections::BTreeSet;

use anyhow::{Context, Result};

use crate::error::Outcome;
use crate::host::Host;

fn should_start(id: i64, known: Option<&[i64]>) -> bool {
    match known {
        None => true,
        Some(known) => known.is_empty() || known.contains(&id),
    }
}

pub async fn run<H: Host>(host: &H) -> Result<()> {
    let prepared: BTreeSet<i64> = crate::disks_reconciler::local_osds(host)
        .await
        .context("list this machine's OSDs")?
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    if prepared.is_empty() {
        return Ok(());
    }

    let known = host.osd_ids().await.ok_or_warn(
        "osd-activate: the cluster's OSD list is unreadable — starting every prepared OSD",
    );
    for id in prepared {
        if should_start(id, known.as_deref()) {
            crate::disks_reconciler::ensure_osd_unit_running(host, id).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const OURS: &str = r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#;

    fn listing() -> String {
        serde_json::json!({
            "1": [{"devices": ["/dev/sdb"], "tags": {"ceph.cluster_fsid": "11111111-2222-3333-4444-555555555555"}}],
            "2": [{"devices": ["/dev/sdc"], "tags": {"ceph.cluster_fsid": "99999999-8888-7777-6666-555555555555"}}],
            "4": [{"devices": ["/dev/sdd"], "tags": {"ceph.cluster_fsid": "11111111-2222-3333-4444-555555555555"}}],
        })
        .to_string()
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
    async fn starts_only_this_clusters_known_osds_that_are_not_running() {
        let host = FakeHost::new()
            .ok("ceph-volume lvm list", &listing())
            .ok("ceph fsid", OURS)
            .ok("ceph osd ls", "[1,2]")
            .ok("systemctl show -p ActiveState --value yolab-ceph-osd@1.service", "inactive")
            .ok("systemctl start", "");
        run(&host).await.unwrap();
        assert!(host.ran("systemctl start yolab-ceph-osd@1.service"));
        assert!(!host.ran("yolab-ceph-osd@2.service"));
        assert!(!host.ran("yolab-ceph-osd@4.service"));
    }

    #[tokio::test]
    async fn a_running_osd_is_left_alone() {
        let host = FakeHost::new()
            .ok("ceph-volume lvm list", &listing())
            .ok("ceph fsid", OURS)
            .ok("ceph osd ls", "[1,4]")
            .ok("systemctl show -p ActiveState --value", "active");
        run(&host).await.unwrap();
        assert!(!host.ran("systemctl start"));
    }
}
