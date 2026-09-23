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
        if s.eq_ignore_ascii_case("ext4") {
            Filesystem::Ext4
        } else {
            Filesystem::Xfs
        }
    }
}

const FS_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

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

pub async fn attempt<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
) -> Result<Attempt<()>> {
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();

    if is_mountpoint(host, &croot_s).await {
        return Ok(Attempt::Ready(()));
    }

    let image = format!("{}/{node}", policy.pool_name);
    match image_state(host, &policy.pool_name, node).await {
        ImageState::Present => {}
        ImageState::Absent => {
            return Ok(Attempt::NotYet(format!(
                "{image} does not exist yet (the images-rbd resource creates it)"
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

    if let Some(why) = carry_over_existing_store(host, root, &dev, &croot).await? {
        return Ok(Attempt::NotYet(why));
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
        Ok(o) => ImageState::Unavailable(o.stderr.trim().to_string()),
        Err(e) => ImageState::Unavailable(e.to_string()),
    }
}

async fn is_mountpoint<H: Host>(host: &H, path: &str) -> bool {
    host.run_cmd("findmnt", &["-rno", "TARGET", "--mountpoint", path])
        .await
        .is_ok_and(|o| o.success)
}

fn is_readable_dir(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(entries) => entries.into_iter().all(|e| e.is_ok()),
        Err(_) => false,
    }
}

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

fn should_carry_over(local_has_store: bool, image_has_store: bool) -> bool {
    local_has_store && !image_has_store
}

fn staging_dir(root: &Path) -> PathBuf {
    let uniq: u64 = rand::random();
    root.join(format!("tmp/yolab-containerd-staging-{uniq:016x}"))
}

async fn carry_over_existing_store<H: Host>(
    host: &H,
    root: &Path,
    dev: &str,
    croot: &Path,
) -> Result<Option<String>> {
    if !dir_has_any_entries(croot) {
        return Ok(None);
    }

    let staging = staging_dir(root);
    std::fs::create_dir_all(&staging)?;
    let staging_s = staging.to_string_lossy().into_owned();

    let mounted = host
        .run_cmd("mount", &[dev, staging_s.as_str()])
        .await?
        .success;
    if !mounted {
        let _ = std::fs::remove_dir(&staging);
        return Ok(Some(format!(
            "cannot stage {dev} at {staging_s} to carry the existing image store over"
        )));
    }

    let outcome = if !should_carry_over(dir_has_any_entries(croot), dir_has_any_entries(&staging)) {
        Ok(None)
    } else {
        let from = format!("{}/.", croot.to_string_lossy());
        let copied = host
            .run_cmd_bounded("cp", &["-a", &from, staging_s.as_str()], FS_OP_TIMEOUT)
            .await;
        match copied {
            Ok(o) if o.success => Ok(None),
            Ok(o) => Ok(Some(format!(
                "could not carry the existing image store onto {dev}: {}",
                o.stderr.trim()
            ))),
            Err(e) => Ok(Some(format!(
                "could not carry the existing image store onto {dev}: {e}"
            ))),
        }
    };

    host.run_cmd("umount", &[staging_s.as_str()])
        .await
        .warn_on_err(format!("unmount the staging mount {staging_s}"));
    std::fs::remove_dir(&staging).debug_on_err(format!("remove {staging_s}"));
    outcome
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

pub async fn is_mounted<H: Host>(host: &H, root: &Path) -> bool {
    let croot = containerd_root(root);
    is_mountpoint(host, &croot.to_string_lossy()).await
}

pub async fn pivot<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
    k3s_unit: &str,
) -> Result<Attempt<()>> {
    if is_mounted(host, root).await {
        return Ok(Attempt::Ready(()));
    }
    match image_state(host, &policy.pool_name, node).await {
        ImageState::Present => {}
        ImageState::Absent => {
            return Ok(Attempt::NotYet(format!(
                "{}/{node} does not exist yet",
                policy.pool_name
            )))
        }
        ImageState::Unavailable(why) => {
            return Ok(Attempt::NotYet(format!(
                "the {} pool cannot answer: {why}",
                policy.pool_name
            )))
        }
    }

    let running = host
        .systemctl(&["is-active", "--quiet", k3s_unit])
        .await
        .is_ok_and(|o| o.success);
    if running {
        let stopped = host.systemctl(&["stop", k3s_unit]).await?;
        if !stopped.success {
            return Ok(Attempt::NotYet(format!(
                "could not stop {k3s_unit} to swap the image store: {}",
                stopped.stderr.trim()
            )));
        }
    }

    let outcome = attempt(host, root, node, policy).await;

    if running {
        let _ = host.systemctl(&["reset-failed", k3s_unit]).await;
        host.systemctl(&["start", "--no-block", k3s_unit])
            .await
            .warn_on_err(format!("start {k3s_unit} after swapping the image store"));
    }
    outcome
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

    fn booting() -> FakeHost {
        FakeHost::new().fail("findmnt -rno TARGET --mountpoint", "")
    }

    fn ran_mount(host: &FakeHost) -> bool {
        host.calls().iter().any(|c| c.starts_with("mount "))
    }

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
        assert!(!ran_mount(&host), "{:?}", host.calls());
    }

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
        assert!(
            !host.ran("rbd map") && !ran_mount(&host),
            "{:?}",
            host.calls()
        );
    }

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
        assert!(!host.ran("mkfs") && !ran_mount(&host), "{:?}", host.calls());
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
        let cases = [(262_144, 0, false), (0, 0, true), (262_144, 4, true)];
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

    const K3S: &str = "k3s.service";

    #[tokio::test]
    async fn a_pivot_with_no_image_never_touches_k3s() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting().fail("rbd ls images", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("cannot answer"));
        assert!(
            !host.ran("systemctl stop"),
            "k3s was stopped for a pivot that could not proceed: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn an_already_mounted_store_is_not_pivoted_again() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("findmnt -rno TARGET --mountpoint", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        assert!(!host.ran("systemctl stop"));
        assert!(!ran_mount(&host));
    }

    #[tokio::test]
    async fn a_pivot_stops_k3s_before_mounting_and_starts_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid", "")
            .ok("mkfs.xfs", "")
            .ok("mount ", "")
            .ok("systemctl is-active", "")
            .ok("systemctl stop", "")
            .ok("systemctl reset-failed", "")
            .ok("systemctl start", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        let stopped = host
            .position("systemctl stop")
            .expect("k3s was never stopped");
        let mounted = host
            .calls()
            .iter()
            .position(|c| c.starts_with("mount "))
            .expect("never mounted");
        let started = host
            .position("systemctl start")
            .expect("k3s was never started again");
        assert!(
            stopped < mounted && mounted < started,
            "containerd must not be running while its data-root is mounted over: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_pivot_on_a_node_where_k3s_is_not_running_does_not_start_it() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid", "")
            .ok("mkfs.xfs", "")
            .ok("mount ", "")
            .fail("systemctl is-active", "inactive");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        assert!(!host.ran("systemctl stop"));
        assert!(
            !host.ran("systemctl start"),
            "a k3s that was not running before the pivot must not be started by it"
        );
    }

    #[tokio::test]
    async fn k3s_is_brought_back_even_when_the_mount_fails() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid", "")
            .ok("mkfs.xfs", "")
            .fail("mount ", "no such device")
            .ok("systemctl is-active", "")
            .ok("systemctl stop", "")
            .ok("systemctl reset-failed", "")
            .ok("systemctl start", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("mount"));
        assert!(
            host.ran("systemctl start"),
            "a failed pivot left k3s stopped: {:?}",
            host.calls()
        );
    }

    fn store_with(root: &Path, entries: &[&str]) -> PathBuf {
        let croot = containerd_root(root);
        std::fs::create_dir_all(&croot).unwrap();
        for e in entries {
            std::fs::write(croot.join(e), b"x").unwrap();
        }
        croot
    }

    #[tokio::test]
    async fn an_empty_store_is_not_carried_over() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(containerd_root(dir.path())).unwrap();
        let host = FakeHost::new();
        let out =
            carry_over_existing_store(&host, dir.path(), "/dev/rbd0", &containerd_root(dir.path()))
                .await
                .unwrap();
        assert_eq!(out, None);
        assert!(
            !host.ran("cp -a"),
            "nothing to carry over, so nothing should have been copied"
        );
    }

    #[tokio::test]
    async fn a_populated_store_is_copied_onto_the_image_before_it_is_mounted_over() {
        let dir = tempfile::tempdir().unwrap();
        let croot = store_with(dir.path(), &["layer-a", "layer-b"]);
        let host = FakeHost::new()
            .ok("mount ", "")
            .ok("cp -a", "")
            .ok("umount", "");
        let out = carry_over_existing_store(&host, dir.path(), "/dev/rbd0", &croot)
            .await
            .unwrap();
        assert_eq!(out, None);
        let staged = host
            .position("mount /dev/rbd0")
            .expect("never staged the image");
        let copied = host
            .position("cp -a")
            .expect("the existing image store was discarded instead of carried over");
        let released = host
            .position("umount")
            .expect("never unmounted the staging mount");
        assert!(staged < copied && copied < released, "{:?}", host.calls());
    }

    #[test]
    fn an_image_that_already_holds_a_store_is_never_overwritten() {
        assert!(
            !should_carry_over(true, true),
            "the image already carries this node's store from a previous boot; \
             copying the root filesystem over it would replace newer layers with older"
        );
    }

    #[test]
    fn a_populated_local_store_is_carried_onto_a_blank_image() {
        assert!(should_carry_over(true, false));
    }

    #[test]
    fn nothing_is_carried_over_when_there_is_nothing_to_carry() {
        assert!(!should_carry_over(false, false));
        assert!(!should_carry_over(false, true));
    }

    #[tokio::test]
    async fn a_failed_copy_leaves_the_old_store_in_place_and_does_not_mount() {
        let dir = tempfile::tempdir().unwrap();
        let croot = store_with(dir.path(), &["layer-a"]);
        let host = FakeHost::new()
            .ok("mount ", "")
            .fail("cp -a", "no space left on device")
            .ok("umount", "");
        let why = carry_over_existing_store(&host, dir.path(), "/dev/rbd0", &croot)
            .await
            .unwrap()
            .expect("a failed copy must not report success");
        assert!(why.contains("no space left"), "{why}");
        assert!(
            host.ran("umount"),
            "the staging mount was leaked after a failed copy"
        );
        assert!(
            std::fs::read_dir(&croot).unwrap().count() > 0,
            "the old store was destroyed by a failed carry-over"
        );
    }

    #[tokio::test]
    async fn a_staging_mount_that_fails_is_reported_and_nothing_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let croot = store_with(dir.path(), &["layer-a"]);
        let host = FakeHost::new().fail("mount ", "no such device");
        let why = carry_over_existing_store(&host, dir.path(), "/dev/rbd0", &croot)
            .await
            .unwrap()
            .expect("a failed staging mount must not report success");
        assert!(why.contains("stage"), "{why}");
        assert!(!host.ran("cp -a"));
    }

    #[tokio::test]
    async fn a_pivot_carries_the_running_nodes_images_across() {
        let dir = tempfile::tempdir().unwrap();
        store_with(dir.path(), &["layer-a"]);
        let host = mapped()
            .fail("blkid", "")
            .ok("mkfs.xfs", "")
            .ok("mount ", "")
            .ok("cp -a", "")
            .ok("umount", "")
            .ok("systemctl is-active", "")
            .ok("systemctl stop", "")
            .ok("systemctl reset-failed", "")
            .ok("systemctl start", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        assert!(
            host.ran("cp -a"),
            "the pivot threw away every image the node had already pulled: {:?}",
            host.calls()
        );
    }
}
