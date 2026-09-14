//! Boot step: containerd's data-root is this node's images RBD. Always.
//!
//! One path, decided before k3s starts and never changed while it runs: the
//! system LV is an OSD, the images pool and this node's RBD exist, the RBD is
//! mounted here, then k3s starts. This step waits for its preconditions
//! (`storage::wait`) and has no fallback, so there is nothing to move later and
//! nothing that ever stops k3s to change its store.
//!
//! Nothing under the data-root is the owner's data — every byte is a layer a
//! registry will send again — so any doubt about the filesystem is answered by
//! formatting it. A store that breaks while the node runs is repaired by a
//! reboot, the one operation that takes every container off it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use crate::error::Outcome;
use crate::host::Host;

use super::ceph_shared::POOL_PROBE_TIMEOUT;
use super::wait::Attempt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filesystem {
    Xfs,
    Ext4,
}

impl Filesystem {
    pub fn parse(s: &str) -> Self {
        // Anything unrecognised defaults to xfs, matching the Nix option's own
        // default — never silently ext4, which most k8s distros do not use.
        if s.eq_ignore_ascii_case("ext4") {
            Filesystem::Ext4
        } else {
            Filesystem::Xfs
        }
    }
}

/// Budget for `mkfs`, whose runtime scales with the SIZE OF THE IMAGE rather
/// than with how quickly Ceph answers. Measured: a full pass over node2's 163 GiB
/// image took 134s; the image grows with the pool, so the generic 600s command
/// bound is the wrong question to ask of it.
const FS_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// `-o osd_request_timeout`: krbd defaults to waiting forever, so a pool that
/// cannot serve a read parks anything touching the device in uninterruptible
/// sleep, which no signal ends. With a bound the same situation is an I/O error.
/// Long enough to ride out an OSD restart (a few minutes), short of forever.
const OSD_REQUEST_TIMEOUT_SECS: u32 = 300;

pub struct ContainerdStorePolicy {
    pub pool_name: String,
    pub filesystem: Filesystem,
}

pub fn containerd_root(root: &Path) -> PathBuf {
    root.join("var/lib/rancher/k3s/agent/containerd")
}

fn probe_dir(root: &Path) -> PathBuf {
    let uniq: u64 = rand::random();
    root.join(format!("tmp/yolab-containerd-probe-{uniq:016x}"))
}

/// One attempt at putting containerd's data-root on this node's RBD.
pub async fn attempt<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
) -> Result<Attempt<()>> {
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();

    // Already in place (the unit was started again by hand): nothing to do.
    if is_mountpoint(host, &croot_s).await {
        return Ok(Attempt::Ready(()));
    }

    let image = format!("{}/{node}", policy.pool_name);
    match image_state(host, &policy.pool_name, node).await {
        ImageState::Present => {}
        ImageState::Absent => {
            return Ok(Attempt::NotYet(format!(
                "{image} does not exist yet (yolab-images-rbd creates it)"
            )))
        }
        ImageState::Unavailable(why) => {
            return Ok(Attempt::NotYet(format!(
                "the {} pool cannot answer: {why}",
                policy.pool_name
            )))
        }
    }

    let dev = match mapped_device(host, &policy.pool_name, node).await {
        Ok(dev) => dev,
        Err(why) => return Ok(Attempt::NotYet(format!("cannot map {image}: {why}"))),
    };

    if !has_filesystem(host, &dev).await {
        tracing::info!("{dev} is blank — creating {:?}", policy.filesystem);
        mkfs(host, &dev, policy.filesystem).await?;
    } else if !filesystem_is_usable(host, root, &dev).await {
        tracing::warn!(
            "the image store on {dev} will not mount, read or start pods — rebuilding it"
        );
        mkfs(host, &dev, policy.filesystem).await?;
    }

    std::fs::create_dir_all(&croot)?;

    let mounted = host
        .run_cmd("mount", &[dev.as_str(), croot_s.as_str()])
        .await?;
    if !mounted.success {
        return Ok(Attempt::NotYet(format!(
            "mount {dev} {croot_s}: {}",
            mounted.stderr.trim()
        )));
    }
    if !is_readable_dir(&croot) {
        bail!("{croot_s} was mounted from {dev} but cannot be read");
    }
    tracing::info!("containerd's data-root is {dev} ({image})");
    Ok(Attempt::Ready(()))
}

/// Whether this node's image is there — and, KEPT SEPARATE, whether the pool
/// could answer at all. A pool that cannot serve a read is not a pool with no
/// image in it; collapsing the two was the silent half of the 2026-09-10 outage.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ImageState {
    Present,
    Absent,
    Unavailable(String),
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
        // Includes the pool not existing yet: that is waited on either way.
        Ok(o) => ImageState::Unavailable(o.stderr.trim().to_string()),
        Err(e) => ImageState::Unavailable(e.to_string()),
    }
}

/// Answers from the mount table, never by touching the mount: stat() on a
/// filesystem XFS has shut down returns EIO, and `mountpoint -q` reported "not
/// mounted" about exactly the broken mount it was asked about (2026-09-06).
async fn is_mountpoint<H: Host>(host: &H, path: &str) -> bool {
    host.run_cmd("findmnt", &["-rno", "TARGET", "--mountpoint", path])
        .await
        .is_ok_and(|o| o.success)
}

/// A real, partial read — opendir() can succeed against a mount that returns
/// EIO on the first readdir().
fn is_readable_dir(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(entries) => entries.into_iter().all(|e| e.is_ok()),
        Err(_) => false,
    }
}

/// Whether containerd can actually USE this store, as opposed to merely read it.
///
/// ("Snapshots" here are containerd's image layers, nothing to do with backups.)
/// An XFS shutdown mid-write can leave the snapshotter's metadata.db listing
/// layers whose directories are gone. Every read succeeds and containerd cannot
/// start a single pod ("failed to create snapshot: missing parent"). Observed on
/// node1, 2026-09-08, and it survived a reboot.
///
/// Deliberately narrow: a db WITH content beside an EMPTY snapshots directory. A
/// fresh store has neither and is fine; a working store has both.
fn snapshotter_is_coherent(store: &Path) -> bool {
    let overlay = store.join("io.containerd.snapshotter.v1.overlayfs");
    let db_has_content = std::fs::metadata(overlay.join("metadata.db"))
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    !db_has_content || dir_has_any_entries(&overlay.join("snapshots"))
}

fn dir_has_any_entries(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut e| e.next().is_some())
        .unwrap_or(false)
}

/// Every `device` mapped to `pool/name` in `rbd showmapped --format json`.
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

async fn existing_mapping<H: Host>(host: &H, pool: &str, name: &str) -> Option<String> {
    let out = host
        .run_cmd("rbd", &["showmapped", "--format", "json"])
        .await
        .ok()?;
    let v = serde_json::from_str::<Value>(&out.stdout).ok()?;
    find_mapped_devices(&v, pool, name).into_iter().next()
}

/// The device `pool/name` is mapped at, mapping it if it is not. Reuses an
/// existing mapping: kernel `rbd map` does not dedupe, and a second map of the
/// same image is a second device and a second watch. `Err` is rbd's own reason.
async fn mapped_device<H: Host>(host: &H, pool: &str, name: &str) -> Result<String, String> {
    if let Some(dev) = existing_mapping(host, pool, name).await {
        return Ok(dev);
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
        .map_err(|e| e.to_string())?;
    let dev = out.stdout.trim();
    if out.success && !dev.is_empty() {
        return Ok(dev.to_string());
    }
    let why = out.stderr.trim();
    Err(if why.is_empty() {
        format!("rbd map printed no device (success={})", out.success)
    } else {
        why.to_string()
    })
}

async fn has_filesystem<H: Host>(host: &H, dev: &str) -> bool {
    host.run_cmd("blkid", &[dev]).await.is_ok_and(|o| o.success)
}

/// "Can containerd use this?" — answered by mounting it, which is how containerd
/// will find out. Not `xfs_repair -n`: a dirty log after an unclean shutdown makes
/// that refuse outright, and on 2026-09-07 it had both nodes' perfectly good
/// stores reformatted after an ordinary reboot. A mount replays the log.
async fn filesystem_is_usable<H: Host>(host: &H, root: &Path, dev: &str) -> bool {
    let probe = probe_dir(root);
    if std::fs::create_dir_all(&probe).is_err() {
        return false;
    }
    let probe_s = probe.to_string_lossy().into_owned();
    let mounted = host
        .run_cmd("mount", &[dev, probe_s.as_str()])
        .await
        .is_ok_and(|o| o.success);
    let usable = mounted && is_readable_dir(&probe) && snapshotter_is_coherent(&probe);
    if mounted {
        host.run_cmd("umount", &[probe_s.as_str()])
            .await
            .warn_on_err(format!("unmount the probe mount {probe_s}"));
    }
    std::fs::remove_dir(&probe).debug_on_err(format!("remove {probe_s}"));
    usable
}

async fn mkfs<H: Host>(host: &H, dev: &str, fs: Filesystem) -> Result<()> {
    let out = match fs {
        Filesystem::Xfs => {
            host.run_cmd_bounded("mkfs.xfs", &["-f", "-m", "crc=1", dev], FS_OP_TIMEOUT)
                .await?
        }
        Filesystem::Ext4 => {
            host.run_cmd_bounded("mkfs.ext4", &["-q", "-F", "-m0", dev], FS_OP_TIMEOUT)
                .await?
        }
    };
    if !out.success {
        bail!("mkfs on {dev}: {}", out.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn policy() -> ContainerdStorePolicy {
        ContainerdStorePolicy {
            pool_name: "images".into(),
            filesystem: Filesystem::Xfs,
        }
    }

    fn not_yet(a: Attempt<()>) -> String {
        match a {
            Attempt::NotYet(why) => why,
            Attempt::Ready(()) => panic!("expected NotYet"),
        }
    }

    /// A node at boot: nothing is mounted yet. FakeHost queues answers per
    /// command, so this scripts only what no test answers differently.
    fn booting() -> FakeHost {
        FakeHost::new().fail("findmnt -rno TARGET --mountpoint", "")
    }

    /// …and this node's image exists and maps to /dev/rbd0.
    fn mapped() -> FakeHost {
        booting()
            .ok("rbd ls images", "yolab-n1\n")
            .ok("rbd showmapped --format json", "[]")
            .ok("rbd map images/yolab-n1", "/dev/rbd0\n")
    }

    fn snapshotter_at(store: &Path, db_bytes: usize, snapshot_dirs: usize) {
        let overlay = store.join("io.containerd.snapshotter.v1.overlayfs");
        let snaps = overlay.join("snapshots");
        std::fs::create_dir_all(&snaps).unwrap();
        if db_bytes > 0 {
            std::fs::write(overlay.join("metadata.db"), vec![0u8; db_bytes]).unwrap();
        }
        for i in 0..snapshot_dirs {
            std::fs::create_dir_all(snaps.join(i.to_string())).unwrap();
        }
    }

    // ── The boot path ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_blank_image_is_formatted_and_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .ok("mkfs.xfs", "")
            .ok("mount", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(host.ran("mkfs.xfs -f -m crc=1 /dev/rbd0"));
        assert!(host.ran(&format!("mount /dev/rbd0 {}", croot.display())));
    }

    #[tokio::test]
    async fn a_usable_filesystem_is_mounted_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(!host.ran("mkfs"), "a good store keeps its images");
        assert!(host.ran("mount /dev/rbd0"));
    }

    #[tokio::test]
    async fn a_filesystem_that_will_not_mount_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            // The probe mount fails; the real mount after mkfs succeeds.
            .fail("mount", "wrong fs type, bad superblock")
            .ok("mount", "")
            .ok("mkfs.xfs", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(host.ran("mkfs.xfs"));
    }

    #[tokio::test]
    async fn an_existing_mapping_is_reused_never_mapped_twice() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting()
            .ok("rbd ls images", "yolab-n1\n")
            .ok(
                "rbd showmapped --format json",
                r#"[{"pool":"images","name":"yolab-n1","device":"/dev/rbd3"}]"#,
            )
            .ok("blkid /dev/rbd3", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(!host.ran("rbd map"));
        assert!(host.ran("mount /dev/rbd3"));
    }

    #[tokio::test]
    async fn a_failed_mkfs_is_an_error_and_nothing_is_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .fail("mkfs.xfs", "cannot open /dev/rbd0: Device or resource busy");

        assert!(attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .is_err());
        assert!(!host.ran("mount"));
    }

    // ── Waiting, never falling back ──────────────────────────────────────────

    #[tokio::test]
    async fn a_missing_image_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting().ok("rbd ls images", "someone-else\n");
        let why = not_yet(
            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap(),
        );
        assert!(why.contains("does not exist yet"), "{why}");
        assert!(!host.ran("rbd map") && !host.ran("mount"));
    }

    /// 2026-09-10: a pool that cannot answer is not a pool without the image.
    #[tokio::test]
    async fn a_pool_that_cannot_answer_is_waited_for_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting().fail("rbd ls images", "rbd: error opening pool 'images'");
        let why = not_yet(
            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap(),
        );
        assert!(why.contains("error opening pool"), "{why}");
        assert!(!host.ran("rbd map"));
    }

    #[tokio::test]
    async fn a_failed_map_is_waited_for_with_rbds_own_reason() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting()
            .ok("rbd ls images", "yolab-n1\n")
            .ok("rbd showmapped --format json", "[]")
            .fail("rbd map", "rbd: sysfs write failed");
        let why = not_yet(
            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap(),
        );
        assert!(why.contains("sysfs write failed"), "{why}");
        assert!(!host.ran("mkfs") && !host.ran("mount"));
    }

    #[tokio::test]
    async fn a_failed_mount_is_waited_for_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .ok("mkfs.xfs", "")
            .fail("mount", "mount: /dev/rbd0: can't read superblock");
        let why = not_yet(
            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap(),
        );
        assert!(why.contains("can't read superblock"), "{why}");
    }

    // ── Idempotence ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_already_mounted_store_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("findmnt -rno TARGET --mountpoint", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert_eq!(host.calls().len(), 1, "{:?}", host.calls());
    }

    // ── Pure pieces ──────────────────────────────────────────────────────────

    #[test]
    fn filesystem_parse_defaults_to_xfs() {
        assert_eq!(Filesystem::parse("xfs"), Filesystem::Xfs);
        assert_eq!(Filesystem::parse("EXT4"), Filesystem::Ext4);
        assert_eq!(Filesystem::parse("nonsense"), Filesystem::Xfs);
    }

    #[test]
    fn find_mapped_devices_matches_pool_and_name_and_returns_every_duplicate() {
        let v = serde_json::json!([
            {"pool": "images", "name": "yolab-n1", "device": "/dev/rbd0"},
            {"pool": "images", "name": "yolab-n2", "device": "/dev/rbd1"},
            {"pool": "images", "name": "yolab-n1", "device": "/dev/rbd2"},
        ]);
        assert_eq!(
            find_mapped_devices(&v, "images", "yolab-n1"),
            ["/dev/rbd0", "/dev/rbd2"]
        );
        assert!(find_mapped_devices(&v, "images", "yolab-n9").is_empty());
        assert!(find_mapped_devices(&serde_json::json!({}), "images", "yolab-n1").is_empty());
    }

    #[test]
    fn readable_dirs_are_told_apart_from_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_readable_dir(dir.path()));
        assert!(!is_readable_dir(&dir.path().join("missing")));
        assert!(!dir_has_any_entries(dir.path()));
        std::fs::write(dir.path().join("f"), "").unwrap();
        assert!(dir_has_any_entries(dir.path()));
    }

    #[test]
    fn only_a_db_with_layers_beside_no_layer_dirs_is_incoherent() {
        let cases = [
            (262_144, 0, false), // node1, 2026-09-08
            (0, 0, true),        // fresh store
            (262_144, 4, true),  // working store
        ];
        for (db, dirs, coherent) in cases {
            let dir = tempfile::tempdir().unwrap();
            snapshotter_at(dir.path(), db, dirs);
            assert_eq!(
                snapshotter_is_coherent(dir.path()),
                coherent,
                "db={db} dirs={dirs}"
            );
        }
        let nothing = tempfile::tempdir().unwrap();
        assert!(snapshotter_is_coherent(&nothing.path().join("not-created")));
    }

    #[test]
    fn filesystem_work_is_bounded_by_image_size_not_the_command_default() {
        assert!(FS_OP_TIMEOUT > crate::host::RUN_CMD_TIMEOUT);
    }
}
