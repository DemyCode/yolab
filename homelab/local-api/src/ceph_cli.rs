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
/// the unit cannot be stopped. `try_lock` rather than `lock` because queueing
/// behind a wedged call just moves the pile-up from processes into tasks.
static CEPH_VOLUME_LOCK: Mutex<()> = Mutex::const_new(());

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
    let Ok(_serialised) = CEPH_VOLUME_LOCK.try_lock() else {
        bail!(
            "ceph-volume is already running on this node — skipping `{}`",
            args.join(" ")
        );
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

/// Gates anything that would otherwise read "Ceph unreachable" as "Ceph says
/// no".
pub async fn reachable() -> bool {
    ceph(&["-s"]).await.is_ok()
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

/// Device path -> OSD id for this host. Reads the LVM tags ceph-volume wrote,
/// so it needs no mon and works with the cluster down — unlike `ceph osd
/// metadata`.
///
/// Errors propagate. A failure must NOT be read as "no OSDs": the reconciler
/// creates an OSD for any ON disk missing from this map, and `ceph-volume lvm
/// create` wipes the device. See `refuse_osd_creation`.
pub async fn local_osds() -> Result<Vec<(String, i64)>> {
    let raw = ceph_volume(&["lvm", "list", "--format", "json"]).await?;
    let our_fsid = cluster_fsid().await.unwrap_or_default();
    crate::disks_reconciler::parse_lvm_list(&raw, &our_fsid)
}

/// Used to catch the id `ceph-volume lvm create` allocates. Creation takes an
/// id from the mon before the slow work, so a create that dies partway leaves
/// an id in the osdmap with no CRUSH location and nothing on disk; diffing this
/// before and after names it, so the next attempt reuses it.
///
/// Errors propagate. An empty list must never be inferred from a failure — the
/// caller diffs two snapshots, and a silent empty one makes every existing OSD
/// look newly created.
pub async fn osd_ids() -> Result<Vec<i64>> {
    let v = ceph_json(&["osd", "ls"]).await?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default())
}

/// Drops an OSD id from the osdmap without touching any disk.
///
/// Only ever called on an id this process allocated and failed to finish
/// creating, and only when `ceph osd safe-to-destroy` agrees — an id with no
/// CRUSH location has never held a PG, so that passes trivially for a genuine
/// phantom and fails loudly for anything else.
pub async fn osd_purge(osd_id: i64) -> Result<String> {
    ceph(&[
        "osd",
        "purge",
        &format!("osd.{osd_id}"),
        "--yes-i-really-mean-it",
    ])
    .await
}
