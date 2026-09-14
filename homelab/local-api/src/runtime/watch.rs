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
        let selector = format!("metadata.name={name}");
        // By field selector, never `get <kind> <name>`: watching a NAMED object
        // that does not exist fails at once with NotFound, and yolab-restores does
        // not exist until the first restore — so that watch never ran, only
        // retried. A filtered watch of the
        // namespace waits, and reports the object's creation too.
        [
            "get",
            kind,
            "-n",
            ns,
            "--field-selector",
            &selector,
            "--watch-only",
            "-o",
            "name",
        ]
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

    fn sh(script: &str, targets: &'static [&'static str], filter: fn(&str) -> bool) -> Watch {
        Watch {
            label: "test",
            bin: "/bin/sh",
            args: vec!["-c".to_string(), script.to_string()],
            targets,
            filter,
        }
    }

    fn is_event(line: &str) -> bool {
        line.starts_with("EVENT")
    }

    #[tokio::test]
    async fn a_matching_line_wakes_every_target() {
        let woken = super::super::waker("test-watch-target");
        let watch = sh("echo EVENT; echo noise", &["test-watch-target"], is_event);
        run_once(&watch).await.unwrap();
        // The permit from the wake is waiting for the controller's next wait.
        let notified = tokio::time::timeout(Duration::from_secs(1), woken.notified()).await;
        assert!(notified.is_ok(), "the target was woken");
    }

    #[tokio::test]
    async fn a_watch_that_dies_without_output_is_an_error_but_a_finished_one_is_not() {
        let failed = run_once(&sh("exit 3", &[], any_line)).await;
        assert!(failed.is_err());
        let finished = run_once(&sh("exit 0", &[], any_line)).await;
        assert!(finished.is_ok());
        // kubectl watches end on their own after printing events; that is normal.
        let ended = run_once(&sh("echo event; exit 1", &[], any_line)).await;
        assert!(ended.is_ok());
    }

    #[tokio::test]
    async fn a_binary_that_cannot_start_is_an_error() {
        let w = Watch {
            label: "test",
            bin: "/nonexistent/watcher",
            args: vec![],
            targets: &[],
            filter: any_line,
        };
        assert!(run_once(&w).await.is_err());
    }

    #[test]
    fn kube_watches_select_by_name_so_a_missing_object_can_still_be_watched() {
        for w in standard().into_iter().filter(|w| w.bin == "kubectl") {
            assert!(
                w.args.contains(&"--field-selector".to_string()),
                "{}",
                w.label
            );
            assert!(
                w.args.iter().any(|a| a.starts_with("metadata.name=")),
                "{}",
                w.label
            );
            // `get configmap <name>` would put the name straight after the kind.
            assert_eq!(w.args[2], "-n", "{}: {:?}", w.label, w.args);
        }
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
