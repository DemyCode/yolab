//! Ensure this node's mon is in the monmap.
//!
//! Having a mon store and a running daemon is not the same as being in the
//! monmap: a starting mon absent from the map asks the leader to add it
//! (MMonJoin), which is normally all that is needed. This is the fallback for
//! when that has not happened.
//!
//! THE ORDERING RULE — see homelab/nixos/ceph/default.nix's header — never
//! touch the monmap while this node's own mon is down. Adding a mon raises
//! the quorum requirement immediately, so the new mon must already be running
//! and able to sync within seconds. This is why `run` starts the daemon and
//! returns, rather than adding the node to the map, whenever the local mon
//! isn't active yet.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::host::Host;

use super::ceph_shared::{addrvec, mon_dir};

pub struct MonMemberArgs {
    pub mon_addr: String,
}

fn in_monmap(dump: &Value, node: &str) -> bool {
    dump["mons"]
        .as_array()
        .is_some_and(|mons| mons.iter().any(|m| m["name"] == node))
}

async fn is_active<H: Host>(host: &H, unit: &str) -> bool {
    host.systemctl(&["is-active", "--quiet", unit])
        .await
        .map(|o| o.success)
        .unwrap_or(false)
}

/// `--connect-timeout 10` on every call in this module, tighter than the 30s
/// `client_mount_timeout` in ceph.conf: this runs on a timer, so returning
/// quickly and trying again beats waiting out a full timeout while a peer
/// reboots.
async fn reachable_fast<H: Host>(host: &H) -> bool {
    host.ceph(&["--connect-timeout", "10", "-s"]).await.is_ok()
}

async fn mon_dump_fast<H: Host>(host: &H) -> Option<Value> {
    host.ceph_json(&["--connect-timeout", "10", "mon", "dump"])
        .await
        .ok()
}

pub async fn run<H: Host>(host: &H, root: &Path, node: &str, args: &MonMemberArgs) -> Result<()> {
    if !mon_dir(root, node).join("keyring").exists() {
        tracing::info!("{node}: has not joined yet — bootstrap runs first");
        return Ok(());
    }

    let unit = format!("ceph-mon-{node}.service");
    if !is_active(host, &unit).await {
        let _ = host.systemctl(&["start", "--no-block", &unit]).await;
        tracing::info!("started the local mon; membership is checked on the next run");
        return Ok(());
    }

    let mut reachable = false;
    for _ in 0..30 {
        if reachable_fast(host).await {
            reachable = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if !reachable {
        tracing::info!("cluster is not answering — not touching the monmap");
        return Ok(());
    }

    if mon_dump_fast(host)
        .await
        .is_some_and(|d| in_monmap(&d, node))
    {
        tracing::info!("{node} is already in the monmap");
        return Ok(());
    }

    tracing::info!("adding {node} to the monmap");
    let _ = host
        .ceph(&[
            "--connect-timeout",
            "10",
            "mon",
            "add",
            node,
            &addrvec(&args.mon_addr),
        ])
        .await;

    for _ in 0..60 {
        if mon_dump_fast(host)
            .await
            .is_some_and(|d| in_monmap(&d, node))
        {
            tracing::info!("{node} joined the quorum");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    tracing::warn!(
        "{node} is still not in the monmap — see homelab/nixos/ceph/default.nix's header for recovery"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn dump_with(names: &[&str]) -> String {
        serde_json::json!({"mons": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>()}).to_string()
    }

    #[test]
    fn in_monmap_matches_by_name() {
        let d: Value = serde_json::from_str(&dump_with(&["yolab-n1", "yolab-n2"])).unwrap();
        assert!(in_monmap(&d, "yolab-n1"));
        assert!(!in_monmap(&d, "yolab-n3"));
    }

    #[test]
    fn in_monmap_is_false_on_an_unreadable_dump() {
        assert!(!in_monmap(&Value::Null, "yolab-n1"));
    }

    fn joined(dir: &tempfile::TempDir, node: &str) {
        let d = mon_dir(dir.path(), node);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("keyring"), "x").unwrap();
    }

    #[tokio::test]
    async fn does_nothing_before_this_node_has_joined() {
        let host = FakeHost::new(); // no calls scripted — none should happen
        let dir = tempfile::tempdir().unwrap();
        run(
            &host,
            dir.path(),
            "yolab-n1",
            &MonMemberArgs {
                mon_addr: "fd00:cafe::1".into(),
            },
        )
        .await
        .unwrap();
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn starts_the_mon_and_stops_rather_than_touch_the_monmap_while_it_is_down() {
        let host = FakeHost::new()
            .fail(
                "systemctl is-active --quiet ceph-mon-yolab-n1.service",
                "inactive",
            )
            .ok("systemctl start --no-block ceph-mon-yolab-n1.service", "");
        let dir = tempfile::tempdir().unwrap();
        joined(&dir, "yolab-n1");

        run(
            &host,
            dir.path(),
            "yolab-n1",
            &MonMemberArgs {
                mon_addr: "fd00:cafe::1".into(),
            },
        )
        .await
        .unwrap();

        assert!(host.ran("systemctl start --no-block ceph-mon-yolab-n1.service"));
        assert!(
            !host.ran("mon dump") && !host.ran("mon add"),
            "the monmap must never be touched while the local mon is not active"
        );
    }

    #[tokio::test]
    async fn does_nothing_when_already_in_the_monmap() {
        let host = FakeHost::new()
            .ok("systemctl is-active --quiet ceph-mon-yolab-n1.service", "")
            .ok("ceph --connect-timeout 10 -s", "")
            .ok(
                "ceph --connect-timeout 10 mon dump",
                &dump_with(&["yolab-n1"]),
            );
        let dir = tempfile::tempdir().unwrap();
        joined(&dir, "yolab-n1");

        run(
            &host,
            dir.path(),
            "yolab-n1",
            &MonMemberArgs {
                mon_addr: "fd00:cafe::1".into(),
            },
        )
        .await
        .unwrap();

        assert!(!host.ran("mon add"));
    }

    #[tokio::test]
    async fn adds_a_missing_node_and_confirms_it_joins() {
        let host = FakeHost::new()
            .ok("systemctl is-active --quiet ceph-mon-yolab-n2.service", "")
            .ok("ceph --connect-timeout 10 -s", "")
            .ok(
                "ceph --connect-timeout 10 mon dump",
                &dump_with(&["yolab-n1"]),
            ) // not in it yet
            .ok("ceph --connect-timeout 10 mon add", "")
            .ok(
                "ceph --connect-timeout 10 mon dump",
                &dump_with(&["yolab-n1", "yolab-n2"]),
            ); // now it is
        let dir = tempfile::tempdir().unwrap();
        joined(&dir, "yolab-n2");

        run(
            &host,
            dir.path(),
            "yolab-n2",
            &MonMemberArgs {
                mon_addr: "fd00:cafe::2".into(),
            },
        )
        .await
        .unwrap();

        assert!(host.ran("mon add yolab-n2 [v2:[fd00:cafe::2]:3300,v1:[fd00:cafe::2]:6789]"));
    }

    // 30x2s reachability wait, or 60x2s monmap-confirmation wait — paused time
    // so both resolve instantly instead of taking a minute or two.
    #[tokio::test(start_paused = true)]
    async fn gives_up_quietly_when_the_cluster_never_answers() {
        let host = FakeHost::new()
            .ok("systemctl is-active --quiet ceph-mon-yolab-n1.service", "")
            .fail("ceph --connect-timeout 10 -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        joined(&dir, "yolab-n1");

        run(
            &host,
            dir.path(),
            "yolab-n1",
            &MonMemberArgs {
                mon_addr: "fd00:cafe::1".into(),
            },
        )
        .await
        .unwrap();

        assert!(!host.ran("mon add"));
    }
}
