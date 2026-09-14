//! Ceph access via the local host binaries.
//!
//! A plain subprocess rather than `kubectl exec` into a Rook pod, for two
//! reasons found the hard way: routing storage questions through the k3s API
//! meant the Storage page went blind during a 20-hour kubelet crash-loop, and
//! running the `ceph` CLI inside the mgr's 512Mi cgroup OOM-killed the mgr.
//!
//! Every call returns `exec::CmdError`, never a defaulted value: see exec.rs for
//! why "could not answer" must stay distinguishable from "answered nothing".
//!
//! Commands that destroy data are refused here unless they arrive through
//! `crate::ceph::destructive`, which is the only code able to open that door and
//! only does so with proof that the destruction is safe or was asked for.
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::ceph::destructive::{self, Door};
use crate::exec::{self, CmdError};

/// One ceph-volume at a time *from this process*.
///
/// It cannot see the yolab-ceph-osd@N ExecStartPres, which also run
/// ceph-volume; LVM's own locking makes those block rather than corrupt, and
/// the `timeout` wrappers on those units stop the blocking becoming permanent.
///
/// This exists for failure, not throughput. Every caller is on a reconcile
/// loop, so a wedged call means another starts next tick, and another, until
/// the unit cannot be stopped.
///
/// READERS SKIP, WRITERS QUEUE — see `is_read_only` below. It used to be
/// `try_lock` for everything, on the reasoning that queueing behind a wedged
/// call "just moves the pile-up from processes into tasks". That is true of a
/// wedged call and exactly wrong for healthy ones: `lvm list` runs on several
/// reconcile loops and is cheap, `lvm create` runs once when someone switches a
/// disk on, and with try_lock the one operation that does work loses to the ones
/// that merely look.
///
/// Not hypothetical. In the two-node VM test on 2026-09-07, `lvm create` was
/// refused on 10 consecutive attempts across 15 minutes on both nodes — always
/// "already running", never once a 600s timeout, so no call was ever wedged; it
/// was simply starved by the listings. No OSD was created, so no pool, no RBD,
/// and nothing downstream could happen at all.
static CEPH_VOLUME_LOCK: Mutex<()> = Mutex::const_new(());

/// True for the `ceph-volume` invocations that only inspect state.
///
/// Skipping one of these costs nothing — the caller is a reconcile loop and will
/// ask again next tick — so they keep the non-blocking behaviour that stops a
/// wedged call piling up tasks. Anything else changes the disk and was asked for
/// by a person, so it waits its turn instead of being dropped.
fn is_read_only(args: &[&str]) -> bool {
    matches!(args, ["lvm", "list", ..] | ["inventory", ..])
}

/// A wedged mon can hang a command forever. Bounding every call makes a storage
/// hiccup degrade the UI instead of blocking the whole task pool.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Creating an OSD is the one operation whose runtime is unbounded in practice
/// — it wipes labels, creates LVs and mkfs's BlueStore — so it gets its own
/// generous limit rather than the shared 30s.
const CEPH_VOLUME_TIMEOUT: Duration = Duration::from_secs(600);

async fn run_bin(bin: &str, args: &[&str]) -> Result<String, CmdError> {
    let out = exec::output(bin, args, TIMEOUT).await?;
    if out.success {
        return Ok(out.stdout);
    }
    // Ceph writes cephx negotiation chatter to stderr even on success, so the
    // last line is the actual error — but classification reads all of it.
    let stderr = out.stderr.trim();
    let last = stderr.lines().last().unwrap_or("unknown error").to_string();
    Err(CmdError::Failed {
        cmd: exec::render(bin, args),
        kind: exec::classify(bin, stderr),
        stderr: last,
    })
}

fn refuse_destructive(bin: &str, args: &[&str]) -> Result<(), CmdError> {
    if destructive::is_destructive(bin, args) {
        tracing::error!(
            "refusing `{}` — destructive commands must go through ceph::destructive",
            exec::render(bin, args)
        );
        return Err(CmdError::Forbidden {
            cmd: exec::render(bin, args),
        });
    }
    Ok(())
}

pub async fn ceph(args: &[&str]) -> Result<String, CmdError> {
    refuse_destructive("ceph", args)?;
    run_bin("ceph", args).await
}

/// The door-holding variant. `Door` can only be constructed inside
/// `ceph::destructive`, so no other module can reach this with a purge. The
/// returned future does not borrow the door — holding it is the check.
pub fn ceph_destructive<'a>(
    _door: &Door,
    args: &'a [&str],
) -> impl std::future::Future<Output = Result<String, CmdError>> + Send + 'a {
    async move { run_bin("ceph", args).await }
}

fn with_json_format<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut a = args.to_vec();
    a.extend_from_slice(&["-f", "json"]);
    a
}

pub async fn ceph_json(args: &[&str]) -> Result<Value, CmdError> {
    let a = with_json_format(args);
    let raw = ceph(&a).await?;
    exec::parse_json(&exec::render("ceph", &a), &raw)
}

/// `ceph <args> -f json`, deserialized into a model from `crate::ceph::model`.
/// A shape that does not match is a `Parse` error, never a default.
pub async fn ceph_typed<T: DeserializeOwned>(args: &[&str]) -> Result<T, CmdError> {
    let a = with_json_format(args);
    let raw = ceph(&a).await?;
    exec::parse_json(&exec::render("ceph", &a), &raw)
}

pub async fn ceph_volume(args: &[&str]) -> Result<String, CmdError> {
    refuse_destructive("ceph-volume", args)?;
    ceph_volume_inner(args).await
}

pub fn ceph_volume_destructive<'a>(
    _door: &Door,
    args: &'a [&str],
) -> impl std::future::Future<Output = Result<String, CmdError>> + Send + 'a {
    async move { ceph_volume_inner(args).await }
}

async fn ceph_volume_inner(args: &[&str]) -> Result<String, CmdError> {
    let cmd = exec::render("ceph-volume", args);
    let _serialised = if is_read_only(args) {
        let Ok(guard) = CEPH_VOLUME_LOCK.try_lock() else {
            return Err(CmdError::Busy { cmd });
        };
        guard
    } else {
        // Queue. Every call below is bounded at 600s, so the holder cannot block
        // this indefinitely — which is what makes waiting safe here, and is the
        // difference between "a disk you switched on eventually becomes an OSD"
        // and "it might, depending on timing".
        CEPH_VOLUME_LOCK.lock().await
    };
    // `kill_on_drop` (inside exec::output) matters here: without it this leaked
    // a process tree every ten minutes — ceph-volume shells out to `lvs`, lvs
    // blocked scanning a stalled RBD, the timeout abandoned it, and the next
    // tick started another, eight deep, before yolab-local-api became
    // unstoppable and a nixos-rebuild hung behind it. Uninterruptible sleep
    // ignores SIGKILL; keeping the RBD out of LVM's scan (images-store.nix) is
    // what stops lvs blocking in the first place.
    let out = exec::output("ceph-volume", args, CEPH_VOLUME_TIMEOUT).await?;
    exec::into_checked("ceph-volume", args, out)
}

/// Callers compare this against the fsid in a disk's BlueStore superblock to
/// tell our disks from a stranger's, so an unreachable mon is an error and
/// never a default — otherwise every foreign disk starts looking like ours.
pub async fn cluster_fsid() -> Result<String, CmdError> {
    // Some releases return {"fsid": "..."}, others a bare UUID. Accept either.
    let json_err = match ceph_json(&["fsid"]).await {
        Ok(v) => match v["fsid"].as_str().filter(|s| !s.is_empty()) {
            Some(f) => return Ok(f.to_string()),
            None => CmdError::parse("ceph fsid -f json", "no fsid field"),
        },
        Err(e) => e,
    };
    if json_err.is_unanswered() {
        return Err(json_err);
    }
    let plain = ceph(&["fsid"]).await?;
    let f = plain.trim();
    if f.is_empty() {
        return Err(CmdError::parse("ceph fsid", "empty output"));
    }
    Ok(f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The split that decides whether a call may be dropped under contention.
    /// Getting a mutating call classified as read-only would reintroduce the
    /// starvation this exists to fix — `lvm create` refused 10/10 times while
    /// listings held the lock — so the mapping is pinned rather than assumed.
    #[test]
    fn only_inspection_may_be_skipped() {
        assert!(is_read_only(&["lvm", "list", "--format", "json"]));
        assert!(is_read_only(&["inventory", "--format", "json"]));

        for mutating in [
            vec!["lvm", "create", "--bluestore", "--data", "/dev/vdb"],
            vec!["lvm", "zap", "--destroy", "/dev/vdb"],
            vec!["lvm", "prepare", "--data", "/dev/vdb"],
            vec!["lvm", "activate", "--all"],
        ] {
            assert!(
                !is_read_only(&mutating),
                "{mutating:?} changes the disk and must queue, never be dropped"
            );
        }
    }

    /// An unrecognised subcommand must be treated as mutating. A future
    /// ceph-volume verb that this list has not learned about should wait its
    /// turn rather than be silently discarded under load.
    #[test]
    fn anything_unrecognised_is_treated_as_mutating() {
        assert!(!is_read_only(&["something-new"]));
        assert!(!is_read_only(&[]));
    }

    #[tokio::test]
    async fn a_purge_through_the_plain_entry_point_is_refused_before_it_runs() {
        let err = ceph(&["osd", "purge", "osd.3", "--yes-i-really-mean-it"])
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Forbidden { .. }));
    }

    #[tokio::test]
    async fn a_zap_through_the_plain_entry_point_is_refused_before_it_runs() {
        let err = ceph_volume(&["lvm", "zap", "--destroy", "/dev/vdb"])
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Forbidden { .. }));
    }
}
