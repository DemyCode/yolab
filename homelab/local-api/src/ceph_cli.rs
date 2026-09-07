//! Ceph access via the local host binaries.
//!
//! A plain subprocess rather than `kubectl exec` into a Rook pod, for two
//! reasons found the hard way: routing storage questions through the k3s API
//! meant the Storage page went blind during a 20-hour kubelet crash-loop, and
//! running the `ceph` CLI inside the mgr's 512Mi cgroup OOM-killed the mgr.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;

/// One ceph-volume at a time *from this process*.
///
/// It cannot see yolab-ceph-osd-activate or the yolab-ceph-osd@N ExecStartPres,
/// which also run ceph-volume; LVM's own locking makes those block rather than
/// corrupt, and the `timeout` wrappers on those units stop the blocking
/// becoming permanent.
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
const TIMEOUT_SECS: u64 = 30;

async fn run_bin(bin: &str, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        // Without kill_on_drop a timeout only stops *waiting*; the child runs
        // on and the reconcile loop starts another next tick.
        Command::new(bin).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{bin} timed out after {TIMEOUT_SECS}s"))?
    .with_context(|| format!("spawn {bin}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Ceph writes cephx negotiation chatter to stderr even on success, so
        // the last line is the actual error.
        let msg = stderr.lines().last().unwrap_or("unknown error").to_string();
        bail!("{bin} {}: {msg}", args.join(" "))
    }
}

pub async fn ceph(args: &[&str]) -> Result<String> {
    run_bin("ceph", args).await
}

pub async fn ceph_json(args: &[&str]) -> Result<Value> {
    let mut a = args.to_vec();
    a.extend_from_slice(&["-f", "json"]);
    let raw = ceph(&a).await?;
    serde_json::from_str(&raw).with_context(|| format!("parse json from `ceph {}`", args.join(" ")))
}

/// Unused from Rust today — the images-store systemd units drive rbd directly,
/// because they run before k3s and therefore before local-api exists. Kept
/// because surfacing image-store usage on the Storage page is the obvious next
/// consumer.
#[allow(dead_code)]
pub async fn rbd(args: &[&str]) -> Result<String> {
    run_bin("rbd", args).await
}

/// Creating an OSD is the one operation whose runtime is unbounded in practice
/// — it wipes labels, creates LVs and mkfs's BlueStore — so it gets its own
/// generous limit rather than the shared 30s.
pub async fn ceph_volume(args: &[&str]) -> Result<String> {
    let _serialised = if is_read_only(args) {
        let Ok(guard) = CEPH_VOLUME_LOCK.try_lock() else {
            bail!(
                "ceph-volume is already running on this node — skipping `{}`",
                args.join(" ")
            );
        };
        guard
    } else {
        // Queue. Every call below is bounded at 600s, so the holder cannot block
        // this indefinitely — which is what makes waiting safe here, and is the
        // difference between "a disk you switched on eventually becomes an OSD"
        // and "it might, depending on timing".
        CEPH_VOLUME_LOCK.lock().await
    };
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        // Without kill_on_drop this leaked a process tree every ten minutes:
        // ceph-volume shells out to `lvs`, lvs blocked scanning a stalled RBD,
        // the timeout abandoned it, and the next tick started another — eight
        // deep before yolab-local-api became unstoppable and a nixos-rebuild
        // hung behind it.
        //
        // This does not fix that case on its own; uninterruptible sleep ignores
        // SIGKILL. Keeping the RBD out of LVM's scan (see
        // homelab/nixos/ceph/images-store.nix) is what stops lvs blocking. This
        // fixes every other timeout, where the child was killable and simply
        // abandoned.
        Command::new("ceph-volume")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ceph-volume timed out after 600s"))?
    .context("spawn ceph-volume")?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        bail!(
            "ceph-volume {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Callers compare this against the fsid in a disk's BlueStore superblock to
/// tell our disks from a stranger's, so an unreachable mon must yield None and
/// never a default — otherwise every foreign disk starts looking like ours.
pub async fn cluster_fsid() -> Option<String> {
    // Some releases return {"fsid": "..."}, others a bare UUID. Accept either:
    // returning None makes every labelled disk look foreign.
    if let Ok(v) = ceph_json(&["fsid"]).await {
        if let Some(f) = v["fsid"].as_str().filter(|s| !s.is_empty()) {
            return Some(f.to_string());
        }
    }
    ceph(&["fsid"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The only trusted signal for "destroying this OSD loses no data" — never
/// infer it from reweight or PG counts. Inferring it from `pg ls-by-osd` once
/// caused real data loss.
pub async fn osd_safe_to_destroy(osd_id: i64) -> bool {
    ceph_json(&["osd", "safe-to-destroy", &format!("osd.{osd_id}")])
        .await
        .ok()
        .and_then(|v| {
            v["safe_to_destroy"]
                .as_array()
                .map(|a| a.iter().any(|x| x.as_i64() == Some(osd_id)))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::is_read_only;

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
}
