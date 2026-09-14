//! Event sources that wake controllers early.
//!
//! EVENTS ARE HINTS, STATE IS THE TRUTH. A line from `udevadm monitor` or a
//! `kubectl get --watch` only means "something you care about may have changed —
//! look now". The controller then reads the whole current state, exactly as it
//! would on its periodic resync. A missed event therefore costs latency, never
//! correctness, and a duplicate costs one cheap tick (the runtime collapses
//! bursts). This is the level-triggered model Kubernetes controllers use, and it
//! is why this file needs no offsets, no replay and no ordering.
//!
//! Each watcher is a long-running subprocess, restarted with a backoff if it
//! exits — kubectl watches end on their own every few minutes by design.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// A command whose every output line wakes `targets`.
pub struct Watch {
    pub label: &'static str,
    pub bin: &'static str,
    pub args: Vec<String>,
    pub targets: &'static [&'static str],
    /// Lines for which this returns false are ignored (e.g. udev chatter).
    pub filter: fn(&str) -> bool,
}

pub fn any_line(_: &str) -> bool {
    true
}

/// A block device appeared, disappeared or changed.
pub fn udev_block_event(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("UDEV")
        && (l.contains(" add ") || l.contains(" remove ") || l.contains(" change "))
}

pub fn spawn(w: Watch) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(5);
        loop {
            match run_once(&w).await {
                Ok(()) => backoff = Duration::from_secs(5),
                Err(e) => {
                    tracing::debug!("watch {}: {e}", w.label);
                    backoff = (backoff * 2).min(Duration::from_secs(120));
                }
            }
            tokio::time::sleep(backoff).await;
        }
    });
}

async fn run_once(w: &Watch) -> std::io::Result<()> {
    let mut child = Command::new(w.bin)
        .args(&w.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        return Ok(());
    };
    let mut lines = BufReader::new(stdout).lines();
    let mut seen = false;
    while let Some(line) = lines.next_line().await? {
        if !(w.filter)(&line) {
            continue;
        }
        seen = true;
        for t in w.targets {
            super::wake(t);
        }
    }
    let status = child.wait().await?;
    if !status.success() && !seen {
        return Err(std::io::Error::other(format!("exited with {status}")));
    }
    Ok(())
}

/// The watches local-api runs. Kept in one list so what can wake what is
/// readable in one place.
pub fn standard() -> Vec<Watch> {
    let kube_watch = |kind: &str, name: &str, ns: &str| -> Vec<String> {
        ["get", kind, name, "-n", ns, "--watch-only", "-o", "name"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    vec![
        Watch {
            label: "udev-block",
            bin: "udevadm",
            args: ["monitor", "--udev", "--subsystem-match=block"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            targets: &["disks"],
            filter: udev_block_event,
        },
        Watch {
            label: "disk-config",
            bin: "kubectl",
            args: kube_watch("configmap", "yolab-disk-config", "rook-ceph"),
            targets: &["disks"],
            filter: any_line,
        },
        Watch {
            label: "storage-policy",
            bin: "kubectl",
            args: kube_watch("configmap", "yolab-storage-policy", "rook-ceph"),
            targets: &["topology", "disks"],
            filter: any_line,
        },
        Watch {
            label: "storage-heal",
            bin: "kubectl",
            args: kube_watch("configmap", "yolab-storage-heal", "kube-system"),
            targets: &["storage-heal", "cephfs", "disks", "topology"],
            filter: any_line,
        },
        Watch {
            label: "restores",
            bin: "kubectl",
            args: kube_watch("configmap", "yolab-restores", "kube-system"),
            targets: &[
                "restore-watchdog",
                "disks",
                "topology",
                "cephfs",
                "storage-heal",
            ],
            filter: any_line,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_udev_block_events_wake_the_disk_controller() {
        assert!(udev_block_event(
            "UDEV  [1234.5678] add      /devices/pci0000:00/block/sdb (block)"
        ));
        assert!(udev_block_event(
            "UDEV  [1234.5678] remove   /devices/pci0000:00/block/sdb (block)"
        ));
        assert!(!udev_block_event(
            "monitor will print the received events for:"
        ));
        assert!(!udev_block_event(
            "KERNEL[1234.5678] add      /devices/pci0000:00/block/sdb (block)"
        ));
        assert!(!udev_block_event(""));
    }

    #[test]
    fn every_watch_target_is_a_controller_that_exists() {
        let known = crate::controllers::NAMES;
        for w in standard() {
            for t in w.targets {
                assert!(
                    known.contains(t),
                    "watch {} wakes unknown controller {t}",
                    w.label
                );
            }
        }
    }
}
