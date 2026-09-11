//! Map the images RBD and mount it as containerd's data-root.
//!
//! `after` only, never `requires`, on both sides in the Nix unit — this must
//! not fail when Ceph has no OSDs yet, and k3s must not fail when this does.
//! Failing open costs one boot cycle before images move off root; failing
//! closed costs the whole node, with no UI left to diagnose it from.
//!
//! "MOUNTED" WAS NEVER THE QUESTION. "WORKS" IS.
//!
//! This used to check `mountpoint` alone and call that healthy, and that is
//! how both machines in a cluster once sat dead for seventeen hours: the pool
//! was size 1, a disk was lost, the RBD came back with holes where its
//! objects had been, XFS mounted, hit metadata that was now zeros, shut
//! itself down, and every read returned EIO — while `mountpoint` cheerfully
//! reported success. `is_readable_dir` below is the fix: it is a partial
//! read, not just a stat, precisely because opendir() can succeed against a
//! mount that returns EIO on the first readdir(). This has nothing to do with
//! replica count — a partially-readable RBD is exactly as fatal at size 3 as
//! it was at size 1 — so the check stays regardless of what topology.rs sets.
//!
//! That check then spent months unable to fire, because the `mountpoint -q`
//! guarding it stat()s the path, and stat() on a shut-down filesystem returns
//! EIO — so the guard answered "not mounted" for the exact state the check
//! was written to catch. It reads the mount table now; see `is_mountpoint`.
//!
//! Nothing under containerd's data-root is the owner's data: every byte is a
//! container layer a registry will send again. So the right response to any
//! doubt here is to rebuild, never to try to preserve it.
//!
//! That principle was written here long before the code obeyed it. This module
//! spent its whole life copying the entire store onto the RBD before swapping,
//! and that copy was the one operation forcing k3s to stay stopped — ~17 minutes
//! per attempt, which on a two-node cluster is ~17 minutes with no etcd quorum
//! and no API anywhere. It has been deleted; see `discard_and_swap`. Preserving
//! disposable data was never worth an outage, and it was the source of nearly
//! every bug this file has had.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::host::Host;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filesystem {
    Xfs,
    Ext4,
}

impl Filesystem {
    pub fn parse(s: &str) -> Self {
        // Anything unrecognised defaults to xfs, matching the Nix option's
        // own default — never silently ext4, which most k8s distros do not.
        if s.eq_ignore_ascii_case("ext4") {
            Filesystem::Ext4
        } else {
            Filesystem::Xfs
        }
    }
}

/// Budget for `mkfs`, whose runtime scales with the SIZE OF THE IMAGE rather
/// than with how quickly Ceph answers.
///
/// The generic `host::RUN_CMD_TIMEOUT` of 600s is the right question to ask of an
/// `rbd`, `mount` or `systemctl` call — past ten minutes those are hung, not
/// slow. It is the wrong question here, and getting that distinction wrong is the
/// bug this file keeps having: the copy that used to live in this module was
/// bounded at 600s, needed ~1000s, and was SIGKILLed on every attempt it ever
/// made.
///
/// Grounded in measurement, the standing lesson in this file: a full scan of
/// node2's 163 GiB image took 134s. The image is sized as a share of the pool
/// (see images_sizing.rs), so it grows as disks are added — a cluster several
/// times larger would push that toward, and past, 600s. 1800s leaves room for
/// that while still sitting well under the unit's own 3600s `TimeoutStartSec`,
/// so a genuine wedge is reported here with a reason rather than by systemd
/// killing the wrapper silently.
const FS_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

pub struct ContainerdStorePolicy {
    pub pool_name: String,
    pub filesystem: Filesystem,
}

pub fn containerd_root(root: &Path) -> PathBuf {
    root.join("var/lib/rancher/k3s/agent/containerd")
}

fn stage_dir(root: &Path) -> PathBuf {
    let uniq: u64 = rand::random();
    root.join(format!("tmp/yolab-containerd-migrate-{uniq:016x}"))
}

use super::ceph_shared::POOL_PROBE_TIMEOUT;

/// Whether this node's image is there — and, KEPT SEPARATE, whether the pool was
/// able to answer the question at all.
///
/// This used to be a `bool` that collapsed the third case into "absent" with an
/// `.unwrap_or(false)`, and that conflation is what made the outage silent: a
/// pool that cannot serve a read is not a pool with no image in it. The image
/// was right there, mapped at /dev/rbd0 and mounted, while this reported it
/// missing and `run()` announced it was "leaving containerd on the root disk".
///
/// The two answers also want opposite handling, which a bool cannot express:
/// `Absent` is a fresh node awaiting provisioning, and doing nothing is correct.
/// `Unavailable` is a node that must get OFF Ceph and stay off it until the
/// cluster can serve again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageState {
    Present,
    Absent,
    Unavailable,
}

async fn image_state<H: Host>(host: &H, pool: &str, name: &str) -> ImageState {
    match host
        .run_cmd_bounded("rbd", &["ls", pool], POOL_PROBE_TIMEOUT)
        .await
    {
        Ok(o) if o.success => {
            if o.stdout.lines().any(|l| l.trim() == name) {
                ImageState::Present
            } else {
                ImageState::Absent
            }
        }
        // Covers the pool not existing yet, which `rbd ls` reports as a plain
        // failure. Calling that `Unavailable` rather than `Absent` costs one
        // timer tick on a fresh cluster — images_rbd creates the pool, the next
        // run finds it — and never risks acting on an answer the pool did not
        // actually give.
        _ => ImageState::Unavailable,
    }
}

/// Answers from the mount table, never by touching the mount.
///
/// This shelled out to `mountpoint -q` for most of its life, and `mountpoint`
/// stat()s the path. stat() on a filesystem XFS has shut down returns EIO, so
/// `mountpoint` exits non-zero and this reported "not mounted" about precisely
/// the state the caller exists to recognise — silently disabling the
/// mounted-but-unreadable branch in `run()` that this module's header calls THE
/// fix. On a live node (2026-09-06) `mountpoint -q` answered "not a mountpoint"
/// for a containerd data-root on which every read, `ls` included, returned EIO.
///
/// `findmnt` reads /proc/self/mountinfo, which the kernel keeps regardless of
/// whether the filesystem behind the mount can still serve anything, so it goes
/// on saying "mounted" about a mount that has stopped working. That is the
/// answer this function is asked for.
async fn is_mountpoint<H: Host>(host: &H, path: &str) -> bool {
    host.run_cmd("findmnt", &["-rno", "TARGET", "--mountpoint", path])
        .await
        .map(|o| o.success)
        .unwrap_or(false)
}

/// A real, partial read — not just `Path::exists` — because opendir() can
/// succeed against a mount that then returns EIO on the first readdir(). See
/// this module's header for the incident that made the distinction matter.
fn is_readable_dir(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(entries) => entries.into_iter().all(|e| e.is_ok()),
        Err(_) => false,
    }
}

/// Whether containerd can actually USE this store, as opposed to merely read it.
///
/// NOTHING TO DO WITH RESTIC SNAPSHOTS. containerd calls an image's layer
/// filesystems "snapshots" too, and this is entirely about those: the layers a
/// container image is assembled from. The owner's backups are a different thing
/// with the same word.
///
/// READABLE IS THE WRONG QUESTION, and asking only that cost a day. containerd
/// keeps two things here: a snapshotter metadata.db listing layers, and the
/// directories holding them. An XFS shutdown mid-write can leave the list intact
/// and the directories gone. The filesystem is then perfectly healthy, every
/// read succeeds, `is_readable_dir` is satisfied — and containerd cannot start a
/// single pod, because it looks up a layer's parent and finds nothing:
///
///   failed to create snapshot: missing parent "k8s.io/2/sha256:1021ef…"
///   bucket: not found
///
/// Observed on node1 (2026-09-08): metadata.db dated 12:50 beside an EMPTY
/// snapshots directory, and it survived a reboot — because rebooting restores
/// ACCESS to a filesystem, never CONSISTENCY of what is inside it. Every pod on
/// the node failed to start, the CephFS CSI driver among them, so no volume
/// could mount and a backup sat in SyncingVolumes indefinitely. The repair loop
/// logged "already mounted and readable" every five minutes throughout.
///
/// Deliberately narrow: a metadata.db WITH CONTENT beside an EMPTY snapshots
/// directory. Both halves matter. A fresh store has neither and is fine — that
/// is a new node, not a broken one. A working store has both. Only the mismatch
/// is corruption, and only the mismatch is worth discarding a node's image cache
/// over.
/// Takes the STORE directory itself, not a root to derive it from — because the
/// two callers meet the store in different shapes. `run()` looks at the mounted
/// data-root; `filesystem_is_usable` looks at a probe mount, where the store IS
/// the mount point. Deriving the path internally worked for one and silently
/// examined a non-existent directory for the other, which reads as coherent.
fn snapshotter_is_coherent(store: &Path) -> bool {
    let overlay = store.join("io.containerd.snapshotter.v1.overlayfs");
    let db = overlay.join("metadata.db");

    // No db yet: nothing has claimed a layer exists, so nothing can disagree.
    let db_has_content = std::fs::metadata(&db).map(|m| m.len() > 0).unwrap_or(false);
    if !db_has_content {
        return true;
    }
    // The db claims layers. If the directory that should hold them is missing or
    // empty, the two contradict each other and containerd will refuse to create
    // sandboxes against this store.
    dir_has_any_entries(&overlay.join("snapshots"))
}

/// The mounts other than the store itself that still reference the store's tree.
///
/// Every container's rootfs is an overlay whose `lowerdir`/`upperdir`/`workdir`
/// live under containerd's data-root, and each keeps a reference to that
/// filesystem's superblock. `umount -l` on the data-root detaches the *path* at
/// once but cannot free the superblock while those references remain, so the
/// block device stays busy and `mkfs` — which needs it exclusively — fails with
/// EBUSY.
///
/// THIS USED TO RETURN A COUNT, AND THE COUNT WAS ONLY EVER USED TO GIVE UP.
/// The reasoning was that releasing these needs the container runtime, the
/// runtime is down, therefore only a reboot can help. That is wrong twice over:
/// the containers behind these mounts are already dead (their filesystem is
/// unreadable — that is the branch we are in), and the repair does not need
/// `mkfs` at all, only a remount. So the targets are what matters now, because
/// unmounting them is what makes the store recoverable in place. On a live node
/// (2026-09-08) 41 of these held node2 down for over an hour while the repair
/// ran every five minutes and correctly concluded, by that old logic, that it
/// could do nothing.
///
/// Deepest first: a container's rootfs and the shm mount inside its sandbox can
/// nest, and unmounting a parent before its child leaves the child pinned by a
/// path that no longer resolves.
///
/// Reads `/proc/self/mounts`: the store's own line is excluded by target, and the
/// overlays are matched on their options, which is where the store path appears.
fn overlay_targets_pinning(mounts: &str, croot: &str) -> Vec<String> {
    let mut targets: Vec<String> = mounts
        .lines()
        .filter_map(|line| {
            let target = line.split_whitespace().nth(1)?;
            // The store's own mount is not a reference TO the store.
            (target != croot && line.contains(croot)).then(|| target.to_string())
        })
        .collect();
    targets.sort_by_key(|t| std::cmp::Reverse(t.matches('/').count()));
    targets
}

/// Unmounts everything still referencing a dead image store.
///
/// Safe precisely because the caller has already established the store is
/// unreadable: every one of these is the rootfs of a container whose filesystem
/// has vanished underneath it, so there is nothing running left to disturb.
/// Calling this against a HEALTHY store would tear down live containers, which
/// is why it lives behind that check and not on its own.
///
/// Lazy fallback per mount, and failures are not fatal — a mount that refuses
/// both is left for the umount of the store itself to deal with lazily, and the
/// next timer tick reassesses from scratch either way.
async fn release_pinning_overlays<H: Host>(host: &H, mounts: &str, croot: &str) -> usize {
    let targets = overlay_targets_pinning(mounts, croot);
    if targets.is_empty() {
        return 0;
    }
    tracing::info!(
        "releasing {} stale mounts still referencing the dead image store",
        targets.len()
    );
    let mut released = 0;
    for t in &targets {
        let ok = host.run_cmd("umount", &[t]).await.is_ok_and(|o| o.success)
            || host
                .run_cmd("umount", &["-l", t])
                .await
                .is_ok_and(|o| o.success);
        if ok {
            released += 1;
        }
    }
    if released < targets.len() {
        tracing::warn!(
            "released {released} of {} stale mounts; the rest are left to the lazy \
             unmount of the store itself",
            targets.len()
        );
    }
    released
}

fn dir_has_any_entries(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut e| e.next().is_some())
        .unwrap_or(false)
}

/// `.[] | select(.pool==pool and .name==name) | .device` from `rbd showmapped
/// --format json` — every match, not just the first, since the whole reason
/// `all_mapped_devices` exists is that there can legitimately be more than one. Pure
/// and separate from the fetch so the shape of that JSON (an array, `pool`/`name`/
/// `device` keys) is testable without a real rbd.
fn find_mapped_devices(showmapped: &Value, pool: &str, name: &str) -> Vec<String> {
    showmapped
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|e| e["pool"] == pool && e["name"] == name)
                .filter_map(|e| e["device"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every device currently mapped to `pool/name` — plural, because (see this function's
/// history) that is not always just one. `rbd map`, on kernel RBD, does NOT dedupe
/// against an already-mapped image the way this module used to assume: each call
/// creates ANOTHER `/dev/rbdN` and registers ANOTHER watch on the image, on top of
/// whatever is already there. A process killed mid-migration (systemd's
/// `TimeoutStartSec`, or a `nixos-rebuild switch` restarting the unit) never reaches
/// its own `rbd unmap`, so its mapping and watch outlive it — and the next run's
/// `rbd map` piles a new one on rather than finding and reusing it.
async fn all_mapped_devices<H: Host>(host: &H, pool: &str, name: &str) -> Vec<String> {
    let Ok(out) = host
        .run_cmd("rbd", &["showmapped", "--format", "json"])
        .await
    else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&out.stdout) else {
        return Vec::new();
    };
    find_mapped_devices(&v, pool, name)
}

/// Clears every existing mapping of `pool/name` before this run creates its own.
///
/// By the time this is called, `run()` has already confirmed the real containerd
/// mountpoint is NOT a healthy, in-use mount — its early return above would have taken
/// effect otherwise — so nothing here depends on any mapping this finds: every one of
/// them is leftover from an earlier, interrupted attempt. Unmounting first covers a
/// mapping still bind-mounted at an abandoned staging directory (see
/// `migrate_existing_store`'s `stage_dir`) — `rbd unmap` refuses a device that is
/// still busy.
///
/// Best-effort, deliberately: a mapping stuck in the uninterruptible-sleep state this
/// module's header describes cannot be cleared from here, or by anything short of the
/// kernel resolving the I/O or a reboot. A failure here is logged and skipped rather
/// than treated as fatal — that mapping stays leaked exactly as it would have anyway,
/// but it no longer blocks this run from getting its own clean mapping and proceeding.
async fn clear_stale_mappings<H: Host>(host: &H, pool: &str, name: &str) {
    for dev in all_mapped_devices(host, pool, name).await {
        let target = host
            .run_cmd("findmnt", &["-no", "TARGET", "--source", &dev])
            .await
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        if !target.is_empty() {
            tracing::warn!("unmounting stale {dev} at {target}, left behind by a previous attempt");
            if !host
                .run_cmd("umount", &[target.as_str()])
                .await
                .is_ok_and(|o| o.success)
            {
                let _ = host.run_cmd("umount", &["-l", target.as_str()]).await;
            }
        }
        if !host
            .run_cmd("rbd", &["unmap", &dev])
            .await
            .is_ok_and(|o| o.success)
        {
            tracing::warn!(
                "could not unmap stale {dev} — a previous attempt may still be wedged; leaving it mapped"
            );
        }
    }
}

/// Hand the device back to the kernel once we have decided not to use this store.
///
/// UNMOUNTING IS NOT ENOUGH, and believing it was cost a reboot of all three
/// machines on 2026-09-11.
///
/// Releasing the mount gets containerd running again, so the node goes Ready and
/// the repair looks complete. But the RBD stays MAPPED, and a mapped device with
/// requests it can never complete poisons everything that enumerates block
/// devices — which on this platform is the OSD start path:
///
///   ceph-volume runs `lvs` -> `lvs` opens /dev/rbd0 -> parks in uninterruptible
///   sleep -> systemd SIGKILLs it and is ignored ("Processes still around after
///   SIGKILL") -> `yolab-ceph-osd@N` stops at `deactivating/stop-sigterm` forever
///   -> `switch-to-configuration` waits on that job -> the deploy never finishes
///   -> the node that still needs this fix never receives it.
///
/// That is circular: the OSD is what would let the pool serve again, and the pool
/// not serving is what wedges the `lvs` the OSD start needs. Observed on node1 and
/// node3 simultaneously, load average 38 and climbing as each retry leaked another
/// unkillable `lvs`. Only a reboot cleared it.
///
/// `-o force` because a plain `rbd unmap` refuses while requests are outstanding,
/// and outstanding-forever requests are precisely the case this exists for. Bounded
/// for the same reason every other call here is: the unmap itself must not become
/// the new thing that hangs.
///
/// The LVM `global_filter` in images-store.nix is supposed to make this
/// unnecessary by keeping LVM away from `/dev/rbd*`. On the live nodes it was
/// present in /etc/lvm/lvm.conf, with no devices-file overriding it, and `lvs`
/// wedged on rbd0 anyway. Until that is understood, do not rely on it: the only
/// dependable way to keep something from tripping over this device is for the
/// device not to be there.
async fn abandon_mapping<H: Host>(host: &H, pool: &str, name: &str) {
    for dev in all_mapped_devices(host, pool, name).await {
        tracing::info!(
            "unmapping {dev} — nothing should be able to scan a device this pool cannot serve"
        );
        if !host
            .run_cmd_bounded("rbd", &["unmap", "-o", "force", &dev], POOL_PROBE_TIMEOUT)
            .await
            .is_ok_and(|o| o.success)
        {
            tracing::warn!(
                "could not unmap {dev}; it may still wedge anything that enumerates block devices"
            );
        }
    }
}

/// `-o osd_request_timeout` is THE setting behind the worst failure this storage
/// stack has had: krbd defaults to waiting forever, so when the pool cannot serve a
/// read, anything touching the device parks in uninterruptible sleep — a state SIGKILL
/// cannot end. With a timeout the same situation produces a recoverable I/O error
/// instead.
///
/// THE VALUE IS A TRADE-OFF BETWEEN TWO REAL OUTAGES, and 30s was the wrong side
/// of it. Too long and a genuinely dead cluster wedges the node unkillably; too
/// short and an OSD that is merely RESTARTING takes the filesystem down with it.
///
/// 30s lost that second bet on every deploy. `nixos-rebuild` restarts the OSD
/// activation chain, which runs `lvchange -an` and takes the LV down —
/// 15:42:39 to 15:44:12 on node1 (2026-09-08), 93 seconds. Ceph also has to
/// notice the OSD is gone (osd_heartbeat_grace, ~20s) and re-peer its PGs before
/// anything can be served again, so requests to those PGs simply block for the
/// whole window. At 30s they instead failed, XFS took `log I/O error -110`,
/// shut the containerd data-root down, and k3s could not start. Every rebuild
/// destabilised the storage the rebuild depends on.
///
/// 300s clears an OSD restart, a mon election and a peer node's reboot with room
/// to spare, and is still finite: a cluster that is actually gone produces an
/// error in five minutes rather than an unkillable process forever. The point
/// was never a short timeout — it was HAVING one.
const OSD_REQUEST_TIMEOUT_SECS: u32 = 300;

/// Maps the image, or returns the mapping that already exists.
///
/// Checks for an existing mapping first — real idempotency, not just the hope of
/// it — since `run()` already called `clear_stale_mappings` for this same
/// pool/name; finding one here would mean this ran concurrently with another
/// attempt, not that this ought to add yet another mapping on top.
async fn mapped_device<H: Host>(host: &H, pool: &str, name: &str) -> Option<String> {
    if let Some(dev) = all_mapped_devices(host, pool, name)
        .await
        .into_iter()
        .next()
    {
        return Some(dev);
    }
    let out = host
        .run_cmd(
            "rbd",
            &[
                "map",
                &format!("{pool}/{name}"),
                "-o",
                &format!("osd_request_timeout={OSD_REQUEST_TIMEOUT_SECS}"),
            ],
        )
        .await
        .ok()?;
    let dev = out.stdout.trim();
    if out.success && !dev.is_empty() {
        return Some(dev.to_string());
    }
    all_mapped_devices(host, pool, name)
        .await
        .into_iter()
        .next()
}

async fn has_filesystem<H: Host>(host: &H, dev: &str) -> bool {
    host.run_cmd("blkid", &[dev])
        .await
        .map(|o| o.success)
        .unwrap_or(false)
}

/// "Can containerd use this?" — answered by mounting it, which is how containerd
/// will find out.
///
/// This used to run `xfs_repair -n` (or `fsck.ext4 -n`) and read any non-zero
/// exit as damage. That condemns a perfectly good filesystem after an ordinary
/// unclean shutdown: XFS with an unreplayed log makes `xfs_repair` refuse
/// outright, and a dirty log is not damage — a plain mount replays it in
/// milliseconds. `xfs_repair -n` is also stricter than the question being asked;
/// it exits non-zero for anything it *would* change, which is a much lower bar
/// than "unusable".
///
/// Measured, on both nodes of a live cluster after a reboot on 2026-09-07:
/// node1 refused in 12s, node2 after a full 109s scan, both were declared
/// "damaged", both image stores were reformatted, and every image on both
/// machines had to be pulled again — for a reboot, with nothing wrong.
///
/// A mount still catches what the old check was written for. The incident in
/// this module's header was an RBD that came back with holes where its objects
/// had been; that filesystem MOUNTED and then returned EIO on read, which is
/// exactly what `is_readable_dir` detects here.
async fn filesystem_is_usable<H: Host>(host: &H, root: &Path, dev: &str) -> bool {
    let probe = stage_dir(root);
    if std::fs::create_dir_all(&probe).is_err() {
        return false;
    }
    let probe_s = probe.to_string_lossy().into_owned();

    let mounted = host
        .run_cmd("mount", &[dev, &probe_s])
        .await
        .is_ok_and(|o| o.success);
    // A partial read, not a stat — see `is_readable_dir`. An empty store is
    // usable; one whose first readdir() fails is not.
    //
    // AND COHERENT, which is a separate question and the one this got wrong.
    // `run()` only reaches its coherence branch for a store that is ALREADY
    // mounted; this is the path that mounts one fresh, and it was still asking
    // nothing but "does it read". So on node1 (2026-09-08) the incoherent RBD
    // was rejected while mounted, fell back to the root disk, and then five
    // minutes later this function probe-mounted the very same RBD, read it
    // happily, called it usable and mounted it straight back — restoring the
    // exact corruption that had just been diagnosed. The store must be judged
    // by the same standard wherever it is met.
    let usable = mounted && is_readable_dir(&probe) && snapshotter_is_coherent(&probe);

    if mounted {
        let _ = host.run_cmd("umount", &[probe_s.as_str()]).await;
    }
    let _ = std::fs::remove_dir(&probe);
    usable
}

async fn mkfs<H: Host>(host: &H, dev: &str, fs: Filesystem) -> Result<()> {
    let ok = match fs {
        Filesystem::Xfs => {
            host.run_cmd_bounded("mkfs.xfs", &["-f", "-m", "crc=1", dev], FS_OP_TIMEOUT)
                .await?
                .success
        }
        Filesystem::Ext4 => {
            host.run_cmd_bounded("mkfs.ext4", &["-q", "-m0", dev], FS_OP_TIMEOUT)
                .await?
                .success
        }
    };
    if !ok {
        bail!("mkfs failed");
    }
    Ok(())
}

/// Discards whatever image store is on the root disk and hands the empty RBD to
/// containerd, which re-pulls what it needs.
///
/// THIS USED TO COPY, AND COPYING WAS THE MISTAKE.
///
/// It moved the whole data-root onto the RBD before swapping — and that copy was
/// the single most expensive thing this module did, for no benefit this module's
/// own header does not already dismiss: "every byte is a container layer a
/// registry will send again."
///
/// What it cost, measured on a live two-node cluster on 2026-09-07:
///
///   - The copy is the ONLY reason k3s has to stay stopped. Everything else here
///     takes seconds. Copying 9.2G at the ~9MB/s an RBD accepts over this link
///     pinned the whole cluster's API down for ~17 minutes per attempt, because
///     two-node etcd loses quorum the moment one server leaves.
///   - It moved far more than it needed to. Of that 9.2G, 2.4G is the content
///     store; the rest is snapshots and metadata that containerd rebuilds from
///     the blobs. So it pushed ~7G of derived data across the network to avoid
///     re-fetching 2.4G that registries generally serve faster than Ceph accepts.
///   - It was the source of nearly every bug this module has had: a 600s command
///     bound that silently killed it, partial copies, leaked staging directories
///     and the orphaned RBD mappings behind them, and a failure message that
///     guessed "is the RBD large enough?" when size was never the issue.
///
/// And the control plane never needed it: k3s runs apiserver, etcd, scheduler and
/// controller-manager in-process rather than as containers, so an empty image
/// store does not stop the cluster coming back. Only workload pods re-pull, and
/// they do it *after* quorum is restored instead of while it is gone.
///
/// The one thing this genuinely gives up is working without a registry. That is
/// already true of the platform as a whole — it registers tunnels with an external
/// API and syncs charts from remote repos, and there is no local registry — so it
/// is not a new dependency, and no image here is locally built.
async fn discard_and_swap<H: Host>(host: &H, root: &Path, croot: &Path, dev: &str) -> Result<()> {
    // Prove the device mounts BEFORE destroying anything. Wiping first and then
    // failing to mount would leave the node with no image store at all — the one
    // outcome worse than an empty one, since it is not even a state containerd
    // can start from.
    let probe = stage_dir(root);
    std::fs::create_dir_all(&probe)?;
    let probe_s = probe.to_string_lossy().into_owned();
    let mountable = host
        .run_cmd("mount", &[dev, &probe_s])
        .await
        .is_ok_and(|o| o.success);
    if mountable {
        let _ = host.run_cmd("umount", &[probe_s.as_str()]).await;
    }
    let _ = std::fs::remove_dir(&probe);
    if !mountable {
        bail!("{dev} would not mount — staying on the root disk");
    }

    // Remove and recreate rather than clearing the contents in place: a glob
    // misses dotfiles, which would leave stale state under the new mount for
    // containerd to trip over. Doing it while unmounted is also what reclaims
    // the space — mounting over a populated directory only hides it.
    if dir_has_any_entries(croot) {
        tracing::info!(
            "discarding the root-disk image store; containerd will re-pull what it needs"
        );
    }
    std::fs::remove_dir_all(croot)?;
    std::fs::create_dir_all(croot)?;
    Ok(())
}

async fn mount_the_store<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
    mut needs_rebuild: bool,
) -> Result<()> {
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    let image = format!("{}/{node}", policy.pool_name);

    // The caller (`run()`) only reaches here once it has confirmed `croot` is not
    // already a healthy mount, so nothing depends on whatever `rbd showmapped` finds
    // for this image below — every mapping there is leftover from an earlier,
    // interrupted attempt. Clear them before minting a fresh one.
    clear_stale_mappings(host, &policy.pool_name, node).await;

    let Some(dev) = mapped_device(host, &policy.pool_name, node).await else {
        tracing::warn!("could not map {image} — leaving containerd on the root disk");
        return Ok(());
    };
    tracing::info!("images RBD mapped at {dev}");

    // Blank is not the only reason to format: a filesystem full of holes
    // (see this module's header) looks exactly like a healthy one to blkid,
    // which only reads the superblock — one object out of tens of thousands.
    if !needs_rebuild
        && has_filesystem(host, &dev).await
        && !filesystem_is_usable(host, root, &dev).await
    {
        tracing::warn!("the image store on {dev} will not mount and read — rebuilding it");
        needs_rebuild = true;
    }

    if needs_rebuild || !has_filesystem(host, &dev).await {
        tracing::info!("no filesystem on {dev}, creating {:?}", policy.filesystem);
        if mkfs(host, &dev, policy.filesystem).await.is_err() {
            tracing::warn!("mkfs failed — leaving containerd on the root disk");
            return Ok(());
        }
    }

    std::fs::create_dir_all(&croot)?;

    // Hand over to the RBD. Safe here because this unit runs Before=k3s
    // (enforced by run()'s k3s-stop bracket below), so nothing holds these
    // files open. Seconds, not the ~17 minutes the copy this replaced took —
    // and that duration is the cluster's whole API outage on a two-node
    // install, so it is the number that matters.
    if let Err(e) = discard_and_swap(host, root, &croot, &dev).await {
        tracing::warn!("{e}");
        return Ok(());
    }

    if !host
        .run_cmd("mount", &[&dev, &croot_s])
        .await
        .is_ok_and(|o| o.success)
    {
        tracing::warn!("mount failed — leaving containerd on the root disk");
        return Ok(());
    }
    let source = host
        .run_cmd("findmnt", &["-no", "SOURCE", &croot_s])
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
    tracing::info!("containerd data-root now on {source}");
    Ok(())
}

/// Put k3s back if this run stopped it, and report the run as successful.
///
/// `--no-block`, and not optional: k3s.service is `After=` this unit, so a
/// blocking start would deadlock against systemd's own ordering.
///
/// The result is deliberately `Ok(())` — none of this function's callers are
/// failures. "Ceph cannot serve the image store" is a state this unit is
/// designed to survive by staying on the root disk, and reporting it as a unit
/// failure would only add a red service to a node that is, by then, working.
async fn resume_k3s<H: Host>(host: &H, was_active: bool) -> Result<()> {
    if was_active {
        tracing::info!("starting k3s again");
        let _ = host
            .systemctl(&["start", "--no-block", "k3s.service"])
            .await;
    }
    Ok(())
}

pub async fn run<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
) -> Result<()> {
    // THE MOUNT IN FRONT OF US IS TRIAGED FIRST, BEFORE ANY QUESTION ABOUT CEPH.
    //
    // The two preflight guards that used to stand here — "is Ceph reachable" and
    // "does this node's image exist" — both returned `Ok(())` claiming to be
    // "leaving containerd on the root disk". That sentence is true exactly once:
    // on a fresh boot, where nothing is mounted yet. On a node whose data-root is
    // ALREADY a dead RBD mount it is precisely backwards — it leaves containerd
    // on a poisoned mount and calls that the safe fallback.
    //
    // Which means the recovery below, the one this module's header calls THE fix,
    // sat behind the two checks guaranteed to fail in the exact situation it
    // exists to repair. A pool that cannot serve reads is a pool `rbd ls` cannot
    // answer for, so the image reads as missing, so `run()` returned before ever
    // looking at the mount. The branch never executed once.
    //
    // Cost, on 2026-09-10: node3 was lost with the pools at size 1, XFS shut the
    // containerd data-root down on node1 and node2, and all three nodes sat
    // NotReady for 23 hours with kubelet repeating `Container runtime sanity
    // check failed ... /io.containerd.grpc.v1.introspection/uuid: input/output
    // error` while this unit reported success every five minutes.
    //
    // RELEASING A DEAD MOUNT NEEDS NOTHING FROM CEPH. It is a `umount` of a
    // filesystem that has already stopped answering, and it is what lets
    // containerd fall back to the root disk and the node come back Ready. So it
    // must not be gated on the health of the cluster that broke it — that
    // ordering makes the repair unreachable in exactly the cases that need it.
    // Ceph is asked about further down, once the node is safe either way.
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    let mut needs_rebuild = false;

    // Captured HERE, before anything below can stop k3s, because the recovery
    // path further down stops it too. Reading it after that point would see the
    // service already down, conclude it was never running, and skip the restart
    // — leaving a node that repaired its image store perfectly and then never
    // came back.
    //
    // The STATE TEXT, not the exit status. `is-active --quiet` exits non-zero
    // for "activating", and activating is exactly the state a k3s sitting on a
    // dead image store is in — it never finishes starting, because the
    // snapshotter it is retrying cannot open its data-root. Keying off the exit
    // status would therefore read the one node that needs this repair as "k3s
    // was not running", tear its mounts out from under a live process, and
    // never start it again.
    let was_active = host
        .systemctl(&["is-active", "k3s.service"])
        .await
        .map(|o| {
            let s = o.stdout.trim();
            s == "active" || s == "activating"
        })
        .unwrap_or(false);

    if is_mountpoint(host, &croot_s).await {
        // READABLE AND INCOHERENT is a third state, and it is the one that took
        // node1 out. The filesystem is fine; its CONTENTS contradict themselves,
        // so a remount returns the identical broken store — which is exactly why
        // rebooting the machine did not fix it either. The only repair is to
        // discard it and let containerd pull the layers again, and this is the
        // one directory in the system where that is unambiguously safe: every
        // byte of it is a container layer a registry will send back.
        if is_readable_dir(&croot) && !snapshotter_is_coherent(&croot) {
            tracing::warn!(
                "{croot_s} is readable, but its snapshotter metadata references layers \
                 that are not on disk — containerd cannot start any pod against it. \
                 Rebuilding the image store."
            );
            if was_active {
                tracing::info!("stopping k3s to rebuild the incoherent image store");
                let _ = host.systemctl(&["stop", "k3s.service"]).await;
            }
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            release_pinning_overlays(host, &mounts, &croot_s).await;
            if !host
                .run_cmd("umount", &[&croot_s])
                .await
                .is_ok_and(|o| o.success)
            {
                let _ = host.run_cmd("umount", &["-l", &croot_s]).await;
            }
            // The one place that forces a rebuild rather than letting
            // mount_the_store decide: the filesystem WILL mount and read
            // perfectly, so every check below it would pass this store as
            // healthy and hand it back exactly as broken.
            needs_rebuild = true;
        } else if is_readable_dir(&croot) {
            tracing::info!("{croot_s} is already mounted and readable");
            return Ok(());
        } else {
            // MOUNTED BUT UNREADABLE MEANS XFS LATCHED A SHUTDOWN, NOT THAT THE DATA
            // IS GONE.
            //
            // Ceph blips (a node reboot, an OSD restart, a network hiccup), the RBD's
            // writes hit osd_request_timeout, XFS takes `log I/O error -110` and shuts
            // the filesystem down to protect itself. That shutdown is LATCHED: the
            // block device recovers when Ceph does, but the mount stays poisoned
            // until something unmounts and mounts it again. Everything above it —
            // containerd, then k3s, then this node's half of etcd quorum — stays
            // wedged behind an `Input/output error` that never clears on its own.
            //
            // This used to declare the node unrecoverable whenever containers still
            // referenced the store, on the reasoning that repair meant `mkfs` and
            // `mkfs` needs the device exclusively. Both halves of that were wrong:
            //
            //   - Repair does NOT need mkfs. A shutdown filesystem mounts cleanly
            //     once remounted, replaying its log; the images survive. Rebuilding
            //     threw away the entire image cache to fix something a remount fixes,
            //     and `mount_the_store` already escalates to a rebuild by itself if
            //     the filesystem genuinely will not mount and read.
            //   - Those references are NOT load-bearing. They are the rootfs mounts
            //     of containers whose runtime is already dead — which is guaranteed
            //     here, because the store they were built on is unreadable. Nothing
            //     is running to break.
            //
            // Observed 2026-09-08: 41 overlay mounts pinned node2's store, this
            // branch logged "THIS NODE NEEDS A REBOOT" every 5 minutes for over an
            // hour, node2's etcd never started, and node1 looped elections at term 28
            // getting Connection refused on :2380 because nothing was listening.
            // A reboot was never actually required — releasing stale mounts is.
            tracing::warn!(
                "{croot_s} is mounted but cannot be read — XFS has shut down, recovering"
            );

            // k3s FIRST, and before touching any mount: it is the thing that would
            // otherwise be recreating the mounts being torn down. The restart is the
            // shared one at the bottom, driven by `was_active` captured above.
            if was_active {
                tracing::info!("stopping k3s to release the dead image store");
                let _ = host.systemctl(&["stop", "k3s.service"]).await;
            }

            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            release_pinning_overlays(host, &mounts, &croot_s).await;

            // Lazy as a fallback: containerd may already hold descriptors on a
            // filesystem that has shut down, and a plain umount would refuse.
            if !host
                .run_cmd("umount", &[&croot_s])
                .await
                .is_ok_and(|o| o.success)
            {
                let _ = host.run_cmd("umount", &["-l", &croot_s]).await;
            }

            // Deliberately NOT `needs_rebuild = true`. For a LATCHED SHUTDOWN a
            // plain remount is the right, non-destructive repair, and
            // mount_the_store still falls back to a rebuild on its own if the
            // filesystem turns out to be unusable. The incoherent-store branch
            // above is the case that genuinely must rebuild, and it says so
            // there rather than forcing it on every path through here.
            needs_rebuild = false;
        }
    }

    // ── Only now, with the node safe either way, ask about Ceph ─────────────
    //
    // Both of these return without putting containerd on the RBD, and BOTH MUST
    // RESTART k3s, because the triage above may have stopped it to release a
    // dead store. Returning a bare `Ok(())` here — which is what the old
    // placement of these checks did — would strand a node that had just been
    // repaired perfectly, with its runtime never started again.
    if !host.reachable().await {
        tracing::info!("ceph not reachable — containerd stays on the root disk");
        return resume_k3s(host, was_active).await;
    }
    match image_state(host, &policy.pool_name, node).await {
        ImageState::Present => {}
        ImageState::Absent => {
            tracing::info!(
                "no {}/{node} image yet — containerd stays on the root disk",
                policy.pool_name
            );
            return resume_k3s(host, was_active).await;
        }
        // The case that used to masquerade as `Absent`. Staying on the root disk
        // is not a degraded outcome here, it is the whole recovery: every byte
        // under containerd's data-root is a layer a registry will send again, and
        // k3s runs etcd and the apiserver in-process rather than as containers,
        // so the control plane comes back with an empty image store. The cluster
        // is reachable again while the pool is still down, and workload pods
        // re-pull once it is fixed.
        ImageState::Unavailable => {
            tracing::warn!(
                "the {} pool did not answer within {}s — it cannot serve reads right now. \
                 Leaving containerd on the root disk so this node can run without it; \
                 the image store will move back onto Ceph once the pool recovers.",
                policy.pool_name,
                POOL_PROBE_TIMEOUT.as_secs()
            );
            // Staying on the root disk is only half of getting off this pool. The
            // mapping has to go too, or it wedges the OSD start path and with it
            // every deploy on this node — see `abandon_mapping`.
            abandon_mapping(host, &policy.pool_name, node).await;
            return resume_k3s(host, was_active).await;
        }
    }

    // From here on this may stop k3s, and every exit path has to put it back
    // — the flag is captured at the top of this function (not with a `trap`) so
    // the restart runs whether `mount_the_store` returns Ok or Err, and whether
    // or not the recovery branch above already stopped it.
    //
    // Idempotent by design: `stop` on an already-stopped unit is a no-op, so the
    // recovery branch having stopped it costs nothing here.
    if was_active {
        tracing::info!("stopping k3s to move its image store onto Ceph");
        let _ = host.systemctl(&["stop", "k3s.service"]).await;
    }

    let result = mount_the_store(host, root, node, policy, needs_rebuild).await;

    // Not `?` on either: the restart has to happen even when mounting failed —
    // that is the case where the node most needs its runtime back — so the
    // start is issued first and `mount_the_store`'s result reported after.
    let _ = resume_k3s(host, was_active).await;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{fake::FakeHost, CommandOutput};
    use std::future::Future;

    fn policy() -> ContainerdStorePolicy {
        ContainerdStorePolicy {
            pool_name: "images".into(),
            filesystem: Filesystem::Xfs,
        }
    }

    // ── pure helpers ───────────────────────────────────────────────────────

    #[test]
    fn filesystem_parse_defaults_to_xfs() {
        assert_eq!(Filesystem::parse("xfs"), Filesystem::Xfs);
        assert_eq!(Filesystem::parse("ext4"), Filesystem::Ext4);
        assert_eq!(Filesystem::parse("EXT4"), Filesystem::Ext4);
        assert_eq!(Filesystem::parse("nonsense"), Filesystem::Xfs);
    }

    #[test]
    fn find_mapped_devices_matches_pool_and_name() {
        let v = serde_json::json!([
            {"pool": "images", "name": "yolab-n1", "device": "/dev/rbd0"},
            {"pool": "images", "name": "yolab-n2", "device": "/dev/rbd1"},
        ]);
        assert_eq!(find_mapped_devices(&v, "images", "yolab-n1"), ["/dev/rbd0"]);
        assert!(find_mapped_devices(&v, "images", "yolab-n9").is_empty());
    }

    /// The whole reason this returns a `Vec`: a process killed mid-migration leaves its
    /// mapping behind, and the next run's `rbd map` piles a new one on top rather than
    /// reusing it — so the real `rbd showmapped` output this guards against legitimately
    /// has more than one entry for the same pool/name.
    #[test]
    fn find_mapped_devices_returns_every_duplicate() {
        let v = serde_json::json!([
            {"pool": "images", "name": "node2", "device": "/dev/rbd0"},
            {"pool": "images", "name": "node2", "device": "/dev/rbd1"},
            {"pool": "images", "name": "node1", "device": "/dev/rbd2"},
        ]);
        assert_eq!(
            find_mapped_devices(&v, "images", "node2"),
            ["/dev/rbd0", "/dev/rbd1"]
        );
    }

    // ── clear_stale_mappings ──────────────────────────────────────────────────
    //
    // The actual incident this guards against: a node killed mid-migration enough
    // times leaves several `/dev/rbdN` mappings of the same image, at least one still
    // bind-mounted at an abandoned staging directory from `migrate_existing_store`.

    #[tokio::test]
    async fn clears_every_stale_mapping_unmounting_the_ones_still_bind_mounted() {
        let host = FakeHost::new()
            .ok(
                "rbd showmapped --format json",
                r#"[
                    {"pool": "images", "name": "node2", "device": "/dev/rbd0"},
                    {"pool": "images", "name": "node2", "device": "/dev/rbd1"},
                    {"pool": "images", "name": "node1", "device": "/dev/rbd2"}
                ]"#,
            )
            .ok(
                "findmnt -no TARGET --source /dev/rbd0",
                "/tmp/yolab-containerd-migrate-abc",
            )
            .ok("findmnt -no TARGET --source /dev/rbd1", "")
            .ok("umount", "")
            .ok("rbd unmap", "");

        clear_stale_mappings(&host, "images", "node2").await;

        assert!(host.ran("umount /tmp/yolab-containerd-migrate-abc"));
        assert!(host.ran("rbd unmap /dev/rbd0"));
        assert!(host.ran("rbd unmap /dev/rbd1"));
        // A different image's mapping is none of this call's business.
        assert!(!host.ran("rbd unmap /dev/rbd2"));
        // Nothing was mounted at rbd1 — unmounting it would be a bug in its own right.
        assert!(!host.ran("umount /dev/rbd1"));
    }

    // ── mapped_device ─────────────────────────────────────────────────────────

    /// The bug this whole fix is for: `rbd map` on kernel RBD does not dedupe against
    /// an already-mapped image, so calling it when a mapping already exists just piles
    /// another one on. `mapped_device` must never do that — it must find and reuse.
    #[tokio::test]
    async fn reuses_an_existing_mapping_instead_of_mapping_again() {
        let host = FakeHost::new().ok(
            "rbd showmapped --format json",
            r#"[{"pool": "images", "name": "node2", "device": "/dev/rbd0"}]"#,
        );

        let dev = mapped_device(&host, "images", "node2").await;

        assert_eq!(dev.as_deref(), Some("/dev/rbd0"));
        assert!(!host.ran("rbd map"), "must reuse, never map again");
    }

    #[tokio::test]
    async fn maps_fresh_when_nothing_is_currently_mapped() {
        let host = FakeHost::new().ok("rbd showmapped --format json", "[]").ok(
            "rbd map images/node2 -o osd_request_timeout=300",
            "/dev/rbd0",
        );

        let dev = mapped_device(&host, "images", "node2").await;

        assert_eq!(dev.as_deref(), Some("/dev/rbd0"));
        assert!(host.ran("rbd map images/node2"));
    }

    /// A mapping wedged in the uninterruptible-sleep state this module's header
    /// describes cannot be unmapped from here — `rbd unmap` on it just fails, same as
    /// the real command would. That must not stop the loop from clearing every OTHER
    /// mapping it can.
    #[tokio::test]
    async fn a_mapping_that_refuses_to_unmap_does_not_block_the_others() {
        let host = FakeHost::new()
            .ok(
                "rbd showmapped --format json",
                r#"[
                    {"pool": "images", "name": "node2", "device": "/dev/rbd0"},
                    {"pool": "images", "name": "node2", "device": "/dev/rbd1"}
                ]"#,
            )
            .ok("findmnt", "")
            .fail("rbd unmap /dev/rbd0", "rbd: sysfs write failed")
            .ok("rbd unmap /dev/rbd1", "");

        clear_stale_mappings(&host, "images", "node2").await;

        assert!(host.ran("rbd unmap /dev/rbd0"));
        assert!(host.ran("rbd unmap /dev/rbd1"));
    }

    #[test]
    fn is_readable_dir_is_true_for_an_ordinary_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_readable_dir(dir.path()));
    }

    #[test]
    fn is_readable_dir_is_false_for_a_missing_path() {
        assert!(!is_readable_dir(Path::new("/nonexistent/path/at/all")));
    }

    // ── snapshotter_is_coherent ───────────────────────────────────────────────
    //
    // The state this exists for reads perfectly and cannot run a pod, so every
    // one of these is about telling "broken" apart from "empty" — getting that
    // wrong either wipes a healthy node's image cache or leaves a dead one dead.

    /// Lays out the overlayfs snapshotter under a temp root.
    /// `db_bytes = 0` means no metadata.db at all.
    fn snapshotter_at(dir: &Path, db_bytes: usize, snapshot_dirs: usize) {
        let overlay = containerd_root(dir).join("io.containerd.snapshotter.v1.overlayfs");
        let snaps = overlay.join("snapshots");
        std::fs::create_dir_all(&snaps).unwrap();
        if db_bytes > 0 {
            std::fs::write(overlay.join("metadata.db"), vec![0u8; db_bytes]).unwrap();
        }
        for i in 0..snapshot_dirs {
            std::fs::create_dir_all(snaps.join(i.to_string())).unwrap();
        }
    }

    /// The node1 state: the db lists layers, the directory holding them is empty.
    #[test]
    fn a_db_with_layers_and_no_snapshot_dirs_is_incoherent() {
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 262_144, 0);
        assert!(!snapshotter_is_coherent(&containerd_root(dir.path())));
    }

    /// A NEW node has neither, and must never be mistaken for a broken one —
    /// wiping here would discard nothing but would stop k3s to do it.
    #[test]
    fn a_fresh_store_with_no_db_is_coherent() {
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 0, 0);
        assert!(snapshotter_is_coherent(&containerd_root(dir.path())));
    }

    #[test]
    fn a_store_with_both_is_coherent() {
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 262_144, 3);
        assert!(snapshotter_is_coherent(&containerd_root(dir.path())));
    }

    /// An empty db file is not a claim that layers exist, so it is not a
    /// contradiction with an empty directory.
    #[test]
    fn an_empty_db_file_is_coherent() {
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 0, 0);
        let overlay = containerd_root(dir.path()).join("io.containerd.snapshotter.v1.overlayfs");
        std::fs::write(overlay.join("metadata.db"), b"").unwrap();
        assert!(snapshotter_is_coherent(&containerd_root(dir.path())));
    }

    /// Nothing laid out at all — a store that has never been used. Absent is not
    /// broken.
    #[test]
    fn a_store_that_does_not_exist_yet_is_coherent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshotter_is_coherent(&containerd_root(dir.path())));
    }

    /// THE GAP THAT LET THE CORRUPTION COME BACK. The check takes the STORE
    /// directory, and its two callers meet the store in different shapes:
    /// `run()` has the mounted data-root, `filesystem_is_usable` has a probe
    /// mount where the store IS the mount point. Deriving `var/lib/rancher/...`
    /// internally answered correctly for the first and examined a directory
    /// that does not exist for the second — which reads as coherent, so the
    /// broken RBD was rejected while mounted and then remounted five minutes
    /// later by the path that could not see the problem.
    #[test]
    fn the_check_reads_the_store_it_is_given_not_a_path_derived_from_it() {
        let dir = tempfile::tempdir().unwrap();
        // Laid out as a probe mount would be: the store AT the given path,
        // with no var/lib/rancher prefix beneath it.
        let overlay = dir.path().join("io.containerd.snapshotter.v1.overlayfs");
        std::fs::create_dir_all(overlay.join("snapshots")).unwrap();
        std::fs::write(overlay.join("metadata.db"), vec![0u8; 262_144]).unwrap();

        assert!(
            !snapshotter_is_coherent(dir.path()),
            "a probe mount's incoherence must be visible, not hidden behind a \
             path that only exists on the real root"
        );
    }

    // ── overlay_targets_pinning ───────────────────────────────────────────────
    //
    // Lines trimmed from the real /proc/self/mounts of the node in the 2026-09-06
    // incident, which is the shape this has to read correctly.

    const CROOT: &str = "/var/lib/rancher/k3s/agent/containerd";

    fn mounts_with_containers() -> String {
        format!(
            "/dev/rbd0 {CROOT} xfs rw,relatime,inode64 0 0\n\
             overlay /run/k3s/containerd/io.containerd.runtime.v2.task/k8s.io/abc/rootfs \
             overlay rw,relatime,lowerdir={CROOT}/io.containerd.snapshotter.v1.overlayfs/\
             snapshots/1/fs,upperdir={CROOT}/io.containerd.snapshotter.v1.overlayfs/\
             snapshots/206/fs 0 0\n\
             overlay /run/k3s/containerd/io.containerd.runtime.v2.task/k8s.io/def/rootfs \
             overlay rw,relatime,lowerdir={CROOT}/io.containerd.snapshotter.v1.overlayfs/\
             snapshots/2/fs 0 0\n\
             tmpfs /run tmpfs rw,nosuid 0 0\n"
        )
    }

    #[test]
    fn container_overlays_count_as_pinning_the_store() {
        assert_eq!(
            overlay_targets_pinning(&mounts_with_containers(), CROOT).len(),
            2
        );
    }

    /// The store's own mount must never count as pinning itself, or a healthy node
    /// with no containers would look unrecoverable.
    #[test]
    fn the_stores_own_mount_does_not_pin_it() {
        let only_the_store = format!("/dev/rbd0 {CROOT} xfs rw,relatime,inode64 0 0\n");
        assert_eq!(overlay_targets_pinning(&only_the_store, CROOT).len(), 0);
        assert_eq!(overlay_targets_pinning("", CROOT).len(), 0);
    }

    /// The targets are what recovery needs: giving up required only a count,
    /// releasing them requires knowing which mounts to unmount.
    #[test]
    fn pinning_overlays_are_reported_as_unmountable_targets() {
        let targets = overlay_targets_pinning(&mounts_with_containers(), CROOT);
        assert_eq!(targets.len(), 2);
        assert!(targets
            .iter()
            .all(|t| t.starts_with("/run/k3s/containerd/")));
        // The store's own mount must never be handed to umount here — that is
        // done separately and afterwards, and doing it first would leave the
        // overlays pinned by a path that no longer resolves.
        assert!(!targets.iter().any(|t| t == CROOT));
    }

    /// Nested mounts must come off child-first, or unmounting the parent strands
    /// the child on a path that no longer resolves.
    #[test]
    fn deeper_mounts_are_released_before_their_parents() {
        let nested = format!(
            "/dev/rbd0 {CROOT} xfs rw 0 0\n\
             overlay /run/k3s/a/rootfs overlay rw,lowerdir={CROOT}/snapshots/1/fs 0 0\n\
             shm /run/k3s/a/rootfs/deeper/shm tmpfs rw,lowerdir={CROOT}/snapshots/2/fs 0 0\n"
        );
        let targets = overlay_targets_pinning(&nested, CROOT);
        assert_eq!(
            targets,
            vec!["/run/k3s/a/rootfs/deeper/shm", "/run/k3s/a/rootfs"]
        );
    }

    #[tokio::test]
    async fn releasing_overlays_unmounts_every_one_of_them() {
        let host = FakeHost::new().ok("umount", "");
        let released = release_pinning_overlays(&host, &mounts_with_containers(), CROOT).await;

        assert_eq!(released, 2);
        let calls = host.calls();
        // The store itself is NOT unmounted here — that is the caller's job, and
        // doing it first would strand the overlays on a path that no longer
        // resolves.
        assert!(
            !calls.iter().any(|c| c == &format!("umount {CROOT}")),
            "must not unmount the store itself, calls were: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("umount /run/k3s/"))
                .count(),
            2
        );
    }

    /// A mount that refuses a plain umount must still be tried lazily before
    /// being counted as stuck — containerd holds descriptors on a filesystem
    /// that has shut down, which is exactly when a plain umount refuses.
    #[tokio::test]
    async fn a_busy_overlay_falls_back_to_a_lazy_unmount() {
        let host = FakeHost::new()
            .fail("umount /run", "target is busy")
            .ok("umount -l /run", "");
        let released = release_pinning_overlays(&host, &mounts_with_containers(), CROOT).await;

        assert_eq!(released, 2, "the lazy fallback should have carried both");
        assert!(
            host.calls().iter().any(|c| c.starts_with("umount -l /run")),
            "calls were: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn nothing_pinning_means_nothing_unmounted() {
        let host = FakeHost::new().ok("umount", "");
        let only_the_store = format!("/dev/rbd0 {CROOT} xfs rw 0 0\n");
        assert_eq!(
            release_pinning_overlays(&host, &only_the_store, CROOT).await,
            0
        );
        assert!(host.calls().is_empty(), "should not have run any command");
    }

    /// Mounts belonging to anything else — the CephFS volumes an app's PVC brings,
    /// /run, the root filesystem — are not references to the image store.
    #[test]
    fn unrelated_mounts_do_not_pin_the_store() {
        let unrelated = "[fd00:cafe::5]:6789:/volumes/csi/csi-vol-0eb /var/lib/kubelet/pods/\
                         18f5/volumes/kubernetes.io~csi/pvc-9026/mount ceph rw,relatime 0 0\n\
                         /dev/sda2 / ext4 rw,relatime 0 0\n";
        assert_eq!(overlay_targets_pinning(unrelated, CROOT).len(), 0);
    }

    /// The image-sized filesystem operations must sit above the generic 600s
    /// command bound and below the unit's own 3600s `TimeoutStartSec`. Both ends
    /// matter: a full scan measured 134s against a 163 GiB image and the image
    /// grows with the pool, so 600s is a bound this will eventually cross the way
    /// the old copy did; and above 3600s systemd kills the wrapper first, which
    /// loses the reason entirely.
    #[test]
    fn image_sized_operations_sit_between_the_command_bound_and_the_units_timeout() {
        assert!(
            FS_OP_TIMEOUT.as_secs() > 600,
            "mkfs/xfs_repair scale with the image, not with how fast Ceph answers"
        );
        assert!(
            FS_OP_TIMEOUT.as_secs() < 3600,
            "must fire before the unit's TimeoutStartSec so the reason gets logged"
        );
    }

    #[test]
    fn dir_has_any_entries_distinguishes_empty_from_populated() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir_has_any_entries(dir.path()));
        std::fs::write(dir.path().join("layer.tar"), "x").unwrap();
        assert!(dir_has_any_entries(dir.path()));
    }

    // ── run(): the decision tree, against a FakeHost + tempdir root ──────────

    #[tokio::test]
    async fn does_nothing_while_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        let dir = tempfile::tempdir().unwrap();
        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();
        assert!(host.calls().is_empty() || !host.ran("rbd map"));
    }

    #[tokio::test]
    async fn does_nothing_before_this_nodes_image_exists() {
        let host = FakeHost::new().ok("ceph -s", "").ok("rbd ls images", "");
        let dir = tempfile::tempdir().unwrap();
        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("rbd map"));
    }

    /// THE 23-HOUR OUTAGE, 2026-09-10.
    ///
    /// Every other test in this file mocks `rbd ls images` as SUCCEEDING, and
    /// that is precisely why none of them caught this. The incident's shape is
    /// the opposite: node3 was lost, the pools were size 1, two of the images
    /// pool's PGs went `down`, and a pool with down PGs does not fail a read —
    /// it never answers one. `rbd ls` blocked for the full 600s command bound,
    /// `image_exists` swallowed the timeout as "no such image", and `run()`
    /// returned announcing it was "leaving containerd on the root disk".
    ///
    /// It was not. The data-root was a mounted RBD whose XFS had shut down, and
    /// that mount was still there — so kubelet went on failing its runtime
    /// sanity check with EIO and all three nodes stayed NotReady all night,
    /// while this unit reported success every five minutes.
    ///
    /// The repair needs NOTHING from Ceph: unmounting a filesystem that has
    /// already stopped answering is what lets containerd fall back to the root
    /// disk and the node come back. So it must happen even when — especially
    /// when — the pool cannot be reached at all.
    #[tokio::test]
    async fn a_pool_that_cannot_answer_still_releases_a_dead_store() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            // The pool is up enough to talk to a mon, but cannot serve reads.
            .fail("rbd ls images", "timed out")
            .ok("findmnt -rno TARGET --mountpoint", "") // success = IS a mountpoint
            .ok("umount", "")
            .ok("systemctl is-active", "active")
            .ok("systemctl", "");
        let dir = tempfile::tempdir().unwrap();
        // containerd_root deliberately NOT created: is_readable_dir -> false,
        // the same shape a dead XFS mount produces — mounted, nothing readable.

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        let calls = host.calls();
        assert!(
            host.ran("umount"),
            "the dead mount must be released without waiting on Ceph, calls were: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("systemctl start --no-block k3s.service")),
            "and k3s must be started again, or the repair strands the node it just fixed, \
             calls were: {calls:?}"
        );
        assert!(
            !host.ran("rbd map"),
            "nothing should be mapped from a pool that cannot serve reads"
        );
    }

    /// 2026-09-11: RELEASING THE MOUNT IS ONLY HALF OF GETTING OFF A DEAD POOL.
    ///
    /// The node came back Ready with containerd on the root disk, which looked like
    /// a complete repair — and the RBD stayed mapped. `lvs` opened it, wedged
    /// unkillably, and took the OSD start path with it, which blocked
    /// switch-to-configuration, which stopped the fix from ever reaching the third
    /// node. All three machines had to be rebooted.
    #[tokio::test]
    async fn abandoning_a_dead_pool_also_unmaps_the_device() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .fail("rbd ls images", "timed out")
            .ok(
                "rbd showmapped",
                r#"[{"pool":"images","name":"yolab-n1","device":"/dev/rbd0"}]"#,
            )
            .ok("rbd unmap", "")
            .fail("findmnt -rno TARGET --mountpoint", "not a mountpoint")
            .fail("systemctl is-active", "not active");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(containerd_root(dir.path())).unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        let calls = host.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("rbd unmap -o force /dev/rbd0")),
            "a device the pool cannot serve must not be left mapped — it wedges \
             anything that enumerates block devices, calls were: {calls:?}"
        );
    }

    /// The other half of the same incident: the probe's BOUND.
    ///
    /// Reverting `image_state` to a plain `run_cmd` would reintroduce the ten
    /// minutes of dead time per timer tick even with the ordering fixed, since
    /// the triage above only runs once per invocation and every invocation
    /// would again spend most of its life blocked here.
    #[test]
    fn the_pool_probe_gives_up_long_before_the_generic_command_bound() {
        assert!(
            POOL_PROBE_TIMEOUT.as_secs() < 600,
            "a pool that has not answered a one-object read in seconds is not slow, \
             it is unable — waiting out the 600s run_cmd bound buys only downtime"
        );
    }

    #[tokio::test]
    async fn an_already_mounted_and_readable_store_is_left_alone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(containerd_root(dir.path())).unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            !host.ran("rbd map"),
            "a healthy mount must not be touched at all"
        );
        assert!(
            !host.ran("systemctl stop"),
            "k3s must not be stopped for a no-op"
        );
    }

    /// THE NODE1 STATE, END TO END. Readable, mounted, and unusable: the
    /// snapshotter db lists layers whose directories are gone. Nothing in the
    /// old check could see it, so the loop reported the store healthy every five
    /// minutes while no pod on the machine could start.
    #[tokio::test]
    async fn a_readable_but_incoherent_store_is_torn_down_and_rebuilt() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("umount", "")
            .ok("systemctl is-active", "active")
            .ok("systemctl", "")
            .fail("rbd map", "no route to host"); // stop before the real mount dance
        let dir = tempfile::tempdir().unwrap();
        // db claims layers, snapshots directory empty — the contradiction.
        snapshotter_at(dir.path(), 262_144, 0);

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            host.ran("umount"),
            "an incoherent store must be torn down, calls were: {:?}",
            host.calls()
        );
        assert!(
            host.ran("systemctl stop k3s.service"),
            "k3s holds the store open; it has to stop before the rebuild"
        );
    }

    /// THE FALLTHROUGH THIS GUARDS. The first version of the fix let the
    /// incoherent branch fall into the unreadable one, which sets
    /// needs_rebuild=false — so the store was torn down and then handed back
    /// exactly as broken, with the whole repair silently undone. The three
    /// states must be exclusive branches.
    #[tokio::test]
    async fn an_incoherent_store_does_not_fall_through_to_the_remount_path() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("umount", "")
            .ok("systemctl is-active", "active")
            .ok("systemctl", "")
            .fail("rbd map", "no route to host");
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 262_144, 0);

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        // The remount path logs about a latched XFS shutdown and unmounts once.
        // Landing in it as well would mean unmounting a store already unmounted
        // and, worse, resetting the rebuild decision.
        let umounts = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("umount"))
            .count();
        assert!(
            umounts <= 2,
            "one teardown (plain, with a lazy fallback), not two passes: {:?}",
            host.calls()
        );
    }

    /// A HEALTHY node must never be wiped by this. Both halves present is the
    /// normal state of every working machine, and stopping k3s to rebuild there
    /// would turn the repair into the outage.
    #[tokio::test]
    async fn a_coherent_store_is_still_left_completely_alone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "");
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 262_144, 4);

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(!host.ran("umount"), "calls were: {:?}", host.calls());
        assert!(
            !host.ran("systemctl stop"),
            "calls were: {:?}",
            host.calls()
        );
        assert!(!host.ran("rbd map"), "calls were: {:?}", host.calls());
    }

    /// A FRESH node has no db and no snapshots. That is a new machine, not a
    /// broken one, and it must take the ordinary path rather than a rebuild.
    #[tokio::test]
    async fn a_fresh_store_is_not_mistaken_for_a_corrupt_one() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "");
        let dir = tempfile::tempdir().unwrap();
        snapshotter_at(dir.path(), 0, 0);

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            !host.ran("systemctl stop"),
            "a new node must not have k3s stopped: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_mounted_but_unreadable_store_is_unmounted_and_rebuilt() {
        // is_readable_dir cannot distinguish "empty" from "EIO" without a
        // real corrupted mount, so this drives the unmount path via a
        // containerd root that does not exist at all (is_readable_dir ->
        // false) while the mount table still reports mounted — the same shape a
        // dead XFS mount produces: mounted, but nothing can be read from it.
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "") // success = IS a mountpoint
            .ok("umount", "")
            .fail("systemctl is-active", "not active")
            .fail("rbd map", "no route to host"); // stop short of the real mount dance
        let dir = tempfile::tempdir().unwrap();
        // containerd_root deliberately NOT created.

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(host.ran("umount"));
    }

    /// The regression that let the test above pass while the real thing could
    /// never happen. `mountpoint`, `stat`, `test -d` — anything that has to
    /// touch the filesystem to answer — returns EIO once XFS has shut the mount
    /// down, which reads as "not a mountpoint" and skips the rebuild branch
    /// entirely. Only the mount table can answer this question about a mount
    /// that no longer works, so pin the tool, not just the branch.
    #[tokio::test]
    async fn is_mountpoint_never_touches_the_filesystem_it_asks_about() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("umount", "")
            .fail("systemctl is-active", "not active")
            .fail("rbd map", "no route to host");
        let dir = tempfile::tempdir().unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            host.ran("findmnt -rno TARGET --mountpoint"),
            "the mounted-or-not question must be answered from /proc/self/mountinfo"
        );
        // `ran` is a substring match and `--mountpoint` contains "mountpoint",
        // so this has to look at what was actually invoked.
        assert!(
            !host.calls().iter().any(|c| c.starts_with("mountpoint ")),
            "a stat-based probe returns EIO on exactly the mount this must recognise"
        );
    }

    /// Gives `mount`/`umount`/`cp -a` a persistent backing directory standing
    /// in for the RBD, so a mount reveals whatever a previous mount+umount
    /// cycle last wrote there — the same round-trip a real block device
    /// provides. `rbd map`/`blkid`/`xfs_repair` are stubbed to succeed.
    /// Confined to tests: production always shells to the real binaries.
    #[derive(Clone)]
    struct SimulatedDisk {
        inner: FakeHost,
        device_backing: PathBuf,
    }

    fn ok_output(stdout: &str) -> Result<CommandOutput> {
        Ok(CommandOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn copy_dir_all(src: &Path, dst: &Path) {
        let _ = std::fs::create_dir_all(dst);
        let Ok(entries) = std::fs::read_dir(src) else {
            return;
        };
        for entry in entries.flatten() {
            let dest = dst.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir_all(&entry.path(), &dest);
            } else {
                let _ = std::fs::copy(entry.path(), dest);
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for SimulatedDisk {
        fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.ceph(args)
        }
        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            self.inner.ceph_json(args)
        }
        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.ceph_volume(args)
        }
        fn kubectl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.kubectl(args)
        }
        fn kubectl_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            self.inner.kubectl_json(args)
        }
        fn kubectl_apply<'a>(
            &self,
            manifest: &'a str,
        ) -> impl Future<Output = Result<()>> + Send + 'a {
            self.inner.kubectl_apply(manifest)
        }
        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            self.inner.systemctl(args)
        }
        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                match bin {
                    "rbd" if args.first() == Some(&"map") => ok_output("/dev/rbd0"),
                    "blkid" | "xfs_repair" => ok_output(""),
                    // args: [dev, target] — reveal whatever the last umount
                    // persisted, the same as mounting a real device twice.
                    "mount" => {
                        copy_dir_all(&me.device_backing, Path::new(args[1]));
                        ok_output("")
                    }
                    // args: ["-a", "<src>/.", "<dst>/"]
                    "cp" => {
                        let src = args[1].trim_end_matches("/.");
                        let dst = args[2].trim_end_matches('/');
                        copy_dir_all(Path::new(src), Path::new(dst));
                        ok_output("")
                    }
                    // Persist the just-mounted target's content onto the
                    // simulated device before it "unmounts".
                    "umount" => {
                        let target = args.last().copied().unwrap_or("");
                        copy_dir_all(Path::new(target), &me.device_backing);
                        ok_output("")
                    }
                    // `findmnt --mountpoint` is the is-it-mounted question, and
                    // each test answers that one for itself; the other forms are
                    // device lookups, which always resolve to the simulated device.
                    "findmnt" if args.contains(&"--mountpoint") => {
                        me.inner.run_cmd(bin, args).await
                    }
                    "findmnt" => ok_output("/dev/rbd0"),
                    _ => me.inner.run_cmd(bin, args).await,
                }
            }
        }
    }

    /// The inversion of what this test used to assert.
    ///
    /// It required the pre-existing layer to SURVIVE onto the device, which is
    /// what made the copy load-bearing and cost ~17 minutes of cluster-wide API
    /// outage per attempt. The layer is a container layer: a registry will send
    /// it again. What must be true now is the opposite — the root-disk copy is
    /// gone, so its space is actually reclaimed rather than hidden under a mount.
    #[tokio::test]
    async fn discards_the_root_disk_store_rather_than_copying_it() {
        let dir = tempfile::tempdir().unwrap();
        let host = SimulatedDisk {
            inner: FakeHost::new()
                .ok("ceph -s", "")
                .ok("rbd ls images", "yolab-n1\n")
                .fail("findmnt -rno TARGET --mountpoint", "not a mountpoint")
                .fail("systemctl is-active", "not active"),
            device_backing: dir.path().join("simulated-rbd0"),
        };
        let croot = containerd_root(dir.path());
        std::fs::create_dir_all(&croot).unwrap();
        std::fs::write(croot.join("existing-layer.tar"), b"layer bytes").unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            !croot.join("existing-layer.tar").exists(),
            "the root-disk store must be discarded, not preserved — leaving it \
             costs the space it occupies and buys nothing a registry cannot resend"
        );
        assert!(
            !host.inner.ran("cp -a"),
            "copying the store is what forced k3s to stay stopped for ~17 minutes"
        );
        assert!(
            !host.inner.ran("systemctl stop"),
            "k3s was never active, so it must not be stopped"
        );
    }

    /// The wipe must never happen unless the device has been shown to mount.
    /// Destroying the old store and then failing to mount the new one leaves the
    /// node with no image store at all, which is worse than an empty one — it is
    /// not a state containerd can start from.
    #[tokio::test]
    async fn an_unmountable_device_leaves_the_root_disk_store_alone() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .fail("findmnt -rno TARGET --mountpoint", "not a mountpoint")
            .fail("systemctl is-active", "not active")
            .ok("rbd showmapped", "[]")
            .ok("rbd map", "/dev/rbd0")
            .ok("blkid", "")
            .ok("xfs_repair", "")
            .fail("mount", "device is busy");
        let croot = containerd_root(dir.path());
        std::fs::create_dir_all(&croot).unwrap();
        std::fs::write(croot.join("existing-layer.tar"), b"layer bytes").unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(
            croot.join("existing-layer.tar").exists(),
            "nothing may be destroyed until the replacement is known to mount"
        );
    }

    /// A k3s that is stuck ACTIVATING still has to be stopped and restarted.
    ///
    /// That is the state a node sits in when its image store has shut down: k3s
    /// never finishes starting because the snapshotter cannot open its
    /// data-root. `is-active --quiet` exits non-zero there, so keying off the
    /// exit status read the one node needing repair as "k3s was not running" —
    /// tearing its mounts out from under a live process and never starting it
    /// again.
    #[tokio::test]
    async fn a_k3s_stuck_activating_is_still_stopped_and_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let host = SimulatedDisk {
            inner: FakeHost::new()
                .ok("ceph -s", "")
                .ok("rbd ls images", "yolab-n1\n")
                .fail("findmnt -rno TARGET --mountpoint", "not a mountpoint")
                .ok("systemctl is-active", "activating"),
            device_backing: dir.path().join("simulated-rbd0"),
        };
        std::fs::create_dir_all(containerd_root(dir.path())).unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        let calls = host.inner.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("systemctl stop k3s.service")),
            "an activating k3s must still be stopped, calls were: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("systemctl start --no-block k3s.service")),
            "and it must be started again, calls were: {calls:?}"
        );
    }

    #[tokio::test]
    async fn stops_and_restarts_k3s_around_an_active_migration() {
        let dir = tempfile::tempdir().unwrap();
        let host = SimulatedDisk {
            inner: FakeHost::new()
                .ok("ceph -s", "")
                .ok("rbd ls images", "yolab-n1\n")
                .fail("findmnt -rno TARGET --mountpoint", "not a mountpoint")
                // The literal word systemctl prints, not just a zero exit: the
                // decision keys off the state text now, because "activating" is
                // the state a k3s stuck on a dead image store actually reports.
                .ok("systemctl is-active", "active"),
            device_backing: dir.path().join("simulated-rbd0"),
        };
        std::fs::create_dir_all(containerd_root(dir.path())).unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        let calls = host.inner.calls();
        let stop_at = calls
            .iter()
            .position(|c| c.contains("systemctl stop k3s.service"));
        let start_at = calls
            .iter()
            .position(|c| c.contains("systemctl start --no-block k3s.service"));
        assert!(
            stop_at.is_some() && start_at.is_some(),
            "calls were: {calls:?}"
        );
        assert!(
            stop_at.unwrap() < start_at.unwrap(),
            "k3s must be stopped before the migration and started again after it"
        );
    }
}
