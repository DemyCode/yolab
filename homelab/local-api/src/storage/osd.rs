//! Boot enumeration for OSDs: start a yolab-ceph-osd@ instance for every OSD
//! ceph-volume reports as prepared on this host, except one the cluster no
//! longer knows (teardown purges from Ceph *before* it zaps the disk, so a
//! half-removed OSD is still "prepared" and must not be restarted).

use anyhow::Result;
use serde_json::Value;

use crate::host::Host;

/// OSD ids from `ceph-volume lvm list --format json`, i.e. the object's keys.
/// ceph-volume prints `-->` progress lines before the JSON, so anything before
/// the first `{` is discarded. Sorted so callers are deterministic.
fn lvm_osd_ids(raw: &str) -> Vec<i64> {
    let Some(start) = raw.find('{') else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, Value>>(&raw[start..]) else {
        return Vec::new();
    };
    let mut ids: Vec<i64> = map.keys().filter_map(|k| k.parse::<i64>().ok()).collect();
    ids.sort_unstable();
    ids
}

/// Whether a prepared OSD should be started. `known` is the cluster's OSD list:
/// `None` means it could not be read (the boot case), and an empty list means a
/// cluster with no OSDs — both start everything, matching the old shell.
fn should_start(id: i64, known: Option<&[i64]>) -> bool {
    match known {
        None => true,
        Some(known) => known.is_empty() || known.contains(&id),
    }
}

pub async fn run<H: Host>(host: &H) -> Result<()> {
    let Ok(raw) = host.ceph_volume(&["lvm", "list", "--format", "json"]).await else {
        return Ok(());
    };
    let prepared = lvm_osd_ids(&raw);
    if prepared.is_empty() {
        return Ok(());
    }

    let known = host.osd_ids().await.ok();
    for id in prepared {
        if !should_start(id, known.as_deref()) {
            tracing::info!(
                "osd.{id}: prepared on this disk but no longer part of the cluster — not starting it"
            );
            continue;
        }
        let unit = format!("yolab-ceph-osd@{id}.service");
        tracing::info!("starting {unit}");
        let _ = host.systemctl(&["start", "--no-block", &unit]).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lvm_osd_ids_reads_the_object_keys() {
        let raw = "--> progress\n{\"5\":[{}],\"7\":[{}]}";
        assert_eq!(lvm_osd_ids(raw), vec![5, 7]);
    }

    #[test]
    fn lvm_osd_ids_ignores_non_numeric_keys() {
        let raw = "{\"5\":[{}],\"not-a-number\":[{}]}";
        assert_eq!(lvm_osd_ids(raw), vec![5]);
    }

    #[test]
    fn lvm_osd_ids_returns_nothing_without_json() {
        assert_eq!(lvm_osd_ids("no json here"), Vec::<i64>::new());
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
}
