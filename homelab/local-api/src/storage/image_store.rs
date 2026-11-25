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

pub struct ImageStorePolicy {
    pub pool_name: String,
    pub filesystem: Filesystem,
}

pub fn containerd_root(root: &Path) -> PathBuf {
    root.join("var/lib/rancher/k3s/agent/containerd")
}

fn probe_dir(root: &Path) -> PathBuf {
    root.join(format!("tmp/yolab-containerd-probe-{}", std::process::id()))
}

pub async fn attempt<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ImageStorePolicy,
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

    let dev = match mapped_device(host, root, &policy.pool_name, node).await {
        Ok(dev) => dev,
        Err(why) => return Ok(Attempt::NotYet(format!("cannot map {image}: {why}"))),
    };

    let fresh = if !has_filesystem(host, &dev).await {
        tracing::info!("{dev} is blank — creating {:?}", policy.filesystem);
        true
    } else if !filesystem_is_usable(host, root, &dev).await {
        tracing::warn!(
            "the image store on {dev} was not built by this swap, or will not mount, read or \
             start pods — rebuilding it empty; images are pulled again"
        );
        true
    } else {
        false
    };
    if fresh {
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
    if fresh {
        if let Err(e) = std::fs::write(croot.join(BUILT_HERE), b"") {
            bail!("{croot_s} was rebuilt on {dev} but cannot be written: {e}");
        }
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

async fn mapped_device<H: Host>(
    host: &H,
    root: &Path,
    pool: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(dev) = existing_mapping(host, pool, name).await {
        return Ok(dev);
    }
    let image = format!("{pool}/{name}");
    let options = format!("osd_request_timeout={OSD_REQUEST_TIMEOUT_SECS},ms_mode=secure");
    let keyring = super::cephx::images_identity(host, root, pool)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let keyring_s = keyring.to_string_lossy().into_owned();
    let args = [
        "map",
        image.as_str(),
        "-o",
        options.as_str(),
        "--id",
        super::cephx::IMAGES_ID,
        "--keyring",
        keyring_s.as_str(),
    ];
    let out = host
        .run_cmd("rbd", &args)
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
    let usable = mounted
        && is_readable_dir(&probe)
        && is_built_here(&probe)
        && snapshotter_is_coherent(&probe);
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
            host.run_cmd_bounded("mkfs.xfs", &["-f", "-K", "-m", "crc=1", dev], FS_OP_TIMEOUT)
                .await?
        }
        Filesystem::Ext4 => {
            host.run_cmd_bounded(
                "mkfs.ext4",
                &["-q", "-F", "-m0", "-E", "nodiscard", dev],
                FS_OP_TIMEOUT,
            )
            .await?
        }
    };
    if !out.success {
        bail!("mkfs on {dev}: {}", out.stderr.trim());
    }
    Ok(())
}

const BUILT_HERE: &str = "yolab-built-empty";

fn is_built_here(store: &Path) -> bool {
    store.join(BUILT_HERE).is_file()
}

pub const PODS_SLICE: &str = "kubepods.slice";

pub const RETRY: std::time::Duration = std::time::Duration::from_secs(10);

const ALREADY_RUNNING: [&str; 2] = ["containerd", "containerd-shim"];

#[derive(Debug, PartialEq, Eq)]
pub enum Up {
    Mounted,
    LeftOnRootDisk(String),
}

fn running_on_the_root_disk(root: &Path) -> Option<&'static str> {
    ALREADY_RUNNING
        .into_iter()
        .find(|name| super::release::process_named(root, name))
}

async fn step<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ImageStorePolicy,
) -> Result<Attempt<Up>> {
    let croot_s = containerd_root(root).to_string_lossy().into_owned();
    if is_mountpoint(host, &croot_s).await {
        return Ok(Attempt::Ready(Up::Mounted));
    }
    if let Some(name) = running_on_the_root_disk(root) {
        return Ok(Attempt::Ready(Up::LeftOnRootDisk(format!(
            "{name} is already running on the root disk, so the image store is mounted at the \
             next boot instead of under it"
        ))));
    }
    Ok(match attempt(host, root, node, policy).await? {
        Attempt::Ready(()) => Attempt::Ready(Up::Mounted),
        Attempt::NotYet(why) => Attempt::NotYet(why),
    })
}

pub async fn up<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ImageStorePolicy,
    retry: std::time::Duration,
) -> Up {
    let mut last: Option<String> = None;
    loop {
        match step(host, root, node, policy).await {
            Ok(Attempt::Ready(up)) => return up,
            Ok(Attempt::NotYet(why)) => {
                if last.as_deref() != Some(why.as_str()) {
                    tracing::info!("image store: k3s waits until this is ready — {why}");
                    last = Some(why);
                }
            }
            Err(e) => {
                tracing::warn!("image store: {e:#} — trying again in {}s", retry.as_secs());
                last = None;
            }
        }
        tokio::time::sleep(retry).await;
    }
}

pub(crate) fn mount_resting_on(line: &str, data_root: &Path) -> Option<PathBuf> {
    let target = Path::new(line.split(' ').nth(4)?);
    let inside = target != data_root && target.starts_with(data_root);
    let layer = format!("{}/", data_root.display());
    let built_on = line
        .split(" - ")
        .nth(1)
        .is_some_and(|sb| sb.split(' ').skip(1).any(|f| f.contains(&layer)));
    (inside || built_on).then(|| target.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::storage::wait::not_yet;

    fn policy() -> ImageStorePolicy {
        ImageStorePolicy {
            pool_name: "images".into(),
            filesystem: Filesystem::Xfs,
        }
    }

    fn keyed(host: FakeHost) -> FakeHost {
        host.ok(
            "ceph auth get-or-create client.yolab-images",
            "[client.yolab-images]\n\tkey = K\n",
        )
        .ok("chown ceph:ceph", "")
    }

    fn booting() -> FakeHost {
        keyed(FakeHost::new().fail("findmnt -rno TARGET --mountpoint", ""))
    }

    fn at(host: &FakeHost, needle: &str) -> usize {
        host.position(needle).expect(needle)
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

    #[tokio::test]
    async fn the_images_disk_is_mapped_with_an_encrypted_connection() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped();

        let dev = mapped_device(&host, dir.path(), "images", "yolab-n1")
            .await
            .unwrap();

        assert_eq!(dev, "/dev/rbd0");
        assert!(host.ran(
            "rbd map images/yolab-n1 -o osd_request_timeout=300,ms_mode=secure --id yolab-images --keyring"
        ));
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
        assert!(host.ran("mkfs.xfs -f -K -m crc=1 /dev/rbd0"));
        assert!(host.ran(&format!("mount /dev/rbd0 {}", croot.display())));
        assert!(is_built_here(&croot));
    }

    fn built_here_on_probe(root: &Path) {
        let probe = probe_dir(root);
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join(BUILT_HERE), b"").unwrap();
    }

    #[tokio::test]
    async fn a_usable_filesystem_is_mounted_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        built_here_on_probe(dir.path());
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
    async fn a_store_this_swap_did_not_build_is_rebuilt_empty_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "")
            .ok("mkfs.xfs", "");

        let ready = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert_eq!(ready, Attempt::Ready(()));
        assert!(
            at(&host, "umount") < at(&host, "mkfs.xfs -f -K -m crc=1 /dev/rbd0"),
            "{:?}",
            host.calls()
        );
        assert!(is_built_here(&containerd_root(dir.path())));
    }

    #[tokio::test]
    async fn a_store_whose_layers_are_gone_is_rebuilt_even_when_marked() {
        let dir = tempfile::tempdir().unwrap();
        built_here_on_probe(dir.path());
        snapshotter_at(&probe_dir(dir.path()), 262_144, 0);
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "")
            .ok("mkfs.xfs", "");

        attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert!(host.ran("mkfs.xfs"));
    }

    #[tokio::test]
    async fn an_existing_mapping_is_reused_never_mapped_twice() {
        let dir = tempfile::tempdir().unwrap();
        built_here_on_probe(dir.path());
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

    fn a_process_named(root: &Path, pid: u32, comm: &str) {
        let dir = root.join("proc").join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
    }

    #[tokio::test]
    async fn a_store_that_is_already_mounted_lets_k3s_start_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("findmnt -rno TARGET --mountpoint", "");
        let got = up(&host, dir.path(), "yolab-n1", &policy(), RETRY).await;
        assert_eq!(got, Up::Mounted);
        assert!(!ran_mount(&host), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_blank_image_is_formatted_and_mounted_before_k3s_starts() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .ok("mkfs.xfs", "")
            .ok("mount", "");
        let got = up(&host, dir.path(), "yolab-n1", &policy(), RETRY).await;
        assert_eq!(got, Up::Mounted);
        assert!(host.ran("mkfs.xfs -f -K -m crc=1 /dev/rbd0"));
        assert!(host.ran("mount /dev/rbd0"));
    }

    #[tokio::test]
    async fn nothing_is_ever_mounted_under_a_containerd_that_is_already_running() {
        for comm in ["containerd", "containerd-shim"] {
            let dir = tempfile::tempdir().unwrap();
            a_process_named(dir.path(), 4242, comm);
            let host = mapped()
                .fail("blkid /dev/rbd0", "")
                .ok("mkfs.xfs", "")
                .ok("mount", "");
            assert_eq!(
                up(&host, dir.path(), "yolab-n1", &policy(), RETRY).await,
                Up::LeftOnRootDisk(format!(
                    "{comm} is already running on the root disk, so the image store is mounted \
                     at the next boot instead of under it"
                ))
            );
            assert!(!host.ran("mkfs"), "{comm}: {:?}", host.calls());
            assert!(!ran_mount(&host), "{comm}: {:?}", host.calls());
        }
    }

    #[tokio::test]
    async fn an_unrelated_process_does_not_hold_the_store_back() {
        let dir = tempfile::tempdir().unwrap();
        a_process_named(dir.path(), 7, "containerd-helper");
        a_process_named(dir.path(), 8, "sshd");
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .ok("mkfs.xfs", "")
            .ok("mount", "");
        assert_eq!(
            up(&host, dir.path(), "yolab-n1", &policy(), RETRY).await,
            Up::Mounted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn k3s_keeps_waiting_while_the_image_does_not_exist_and_nothing_is_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting().ok("rbd ls images", "someone-else\n");
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            up(&host, dir.path(), "yolab-n1", &policy(), RETRY),
        )
        .await;
        assert!(waited.is_err(), "up gave up instead of waiting: {waited:?}");
        assert!(
            host.calls()
                .iter()
                .filter(|c| c.starts_with("rbd ls"))
                .count()
                > 1,
            "it never asked again"
        );
        assert!(!ran_mount(&host));
    }

    #[tokio::test(start_paused = true)]
    async fn k3s_keeps_waiting_through_an_error_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped()
            .fail("blkid /dev/rbd0", "")
            .fail("mkfs.xfs", "busy");
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            up(&host, dir.path(), "yolab-n1", &policy(), RETRY),
        )
        .await;
        assert!(waited.is_err(), "an error ended the wait: {waited:?}");
        assert!(!ran_mount(&host));
    }

    const ORPHAN: &str = "/run/k3s/containerd/io.containerd.runtime.v2.task/k8s.io/abc/rootfs";

    fn orphaned_rootfs(data_root: &Path) -> String {
        format!(
            "480 30 0:61 / {ORPHAN} rw,relatime - overlay overlay \
             rw,lowerdir={dr}/io.containerd.snapshotter.v1.overlayfs/snapshots/12/fs,\
             upperdir={dr}/io.containerd.snapshotter.v1.overlayfs/snapshots/13/fs",
            dr = data_root.display()
        )
    }

    #[test]
    fn a_rootfs_whose_layers_live_on_the_data_root_rests_on_it() {
        let dr = Path::new("/var/lib/rancher/k3s/agent/containerd");
        assert_eq!(
            mount_resting_on(&orphaned_rootfs(dr), dr),
            Some(PathBuf::from(ORPHAN))
        );
        let elsewhere = orphaned_rootfs(Path::new("/var/lib/other"));
        assert_eq!(mount_resting_on(&elsewhere, dr), None);
        let neighbour = orphaned_rootfs(Path::new("/var/lib/rancher/k3s/agent/containerd-old"));
        assert_eq!(mount_resting_on(&neighbour, dr), None);
        let the_store_itself = format!("90 22 0:50 / {} rw - xfs /dev/rbd0 rw", dr.display());
        assert_eq!(mount_resting_on(&the_store_itself, dr), None);
    }

    #[test]
    fn a_mountinfo_line_too_short_to_name_a_target_rests_on_nothing() {
        let dr = Path::new("/var/lib/rancher/k3s/agent/containerd");
        assert_eq!(mount_resting_on("90 22 0:50 /", dr), None);
    }

    fn blank_and_formatted() -> FakeHost {
        mapped().fail("blkid /dev/rbd0", "").ok("mkfs.xfs", "")
    }

    #[tokio::test]
    async fn a_store_folder_that_cannot_be_made_is_an_error_and_nothing_is_mounted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/rancher")).unwrap();
        std::fs::write(dir.path().join("var/lib/rancher/k3s"), "").unwrap();
        let host = blank_and_formatted();
        assert!(attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .is_err());
        assert!(!ran_mount(&host), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_mount_that_cannot_be_started_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let host = blank_and_formatted().unstartable("mount ");
        let err = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not start"), "{err}");
    }

    #[tokio::test]
    async fn a_store_that_cannot_be_read_after_mounting_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let host =
            blank_and_formatted()
                .ok("mount", "")
                .effect("mount", |_: &str, args: &[&str]| {
                    std::fs::remove_dir(args[1]).unwrap();
                });
        let err = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
    }

    #[tokio::test]
    async fn a_rebuilt_store_that_cannot_be_marked_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let host =
            blank_and_formatted()
                .ok("mount", "")
                .effect("mount", |_: &str, args: &[&str]| {
                    std::fs::create_dir(Path::new(args[1]).join(BUILT_HERE)).unwrap();
                });
        let err = attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be written"), "{err}");
    }

    #[tokio::test]
    async fn a_pool_listing_that_cannot_start_is_waited_for_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let host = booting().unstartable("rbd ls");
        let why = not_yet(
            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap(),
        );
        assert!(why.contains("could not start"), "{why}");
        assert!(!host.ran("rbd map"));
    }

    #[tokio::test]
    async fn a_mapping_list_that_cannot_start_maps_the_image_anew() {
        let dir = tempfile::tempdir().unwrap();
        let host = keyed(FakeHost::new())
            .unstartable("rbd showmapped")
            .ok("rbd map images/yolab-n1", "/dev/rbd0\n");
        assert_eq!(
            mapped_device(&host, dir.path(), "images", "yolab-n1").await,
            Ok("/dev/rbd0".to_string())
        );
    }

    #[tokio::test]
    async fn an_unreadable_mapping_list_maps_the_image_anew() {
        let dir = tempfile::tempdir().unwrap();
        let host = keyed(FakeHost::new())
            .ok("rbd showmapped --format json", "not json")
            .ok("rbd map images/yolab-n1", "/dev/rbd1\n");
        assert_eq!(
            mapped_device(&host, dir.path(), "images", "yolab-n1").await,
            Ok("/dev/rbd1".to_string())
        );
    }

    #[tokio::test]
    async fn an_image_identity_that_cannot_be_made_is_never_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok("rbd showmapped --format json", "[]")
            .fail(
                "ceph auth get-or-create client.yolab-images",
                "error connecting to the cluster",
            );
        let why = mapped_device(&host, dir.path(), "images", "yolab-n1")
            .await
            .unwrap_err();
        assert!(why.contains("create client.yolab-images"), "{why}");
        assert!(!host.ran("rbd map"));
    }

    #[tokio::test]
    async fn a_map_that_cannot_start_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let host = keyed(FakeHost::new())
            .ok("rbd showmapped --format json", "[]")
            .unstartable("rbd map");
        let why = mapped_device(&host, dir.path(), "images", "yolab-n1")
            .await
            .unwrap_err();
        assert!(why.contains("could not start"), "{why}");
    }

    #[tokio::test]
    async fn a_map_that_prints_no_device_and_no_reason_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let host = keyed(FakeHost::new())
            .ok("rbd showmapped --format json", "[]")
            .ok("rbd map", "");
        assert_eq!(
            mapped_device(&host, dir.path(), "images", "yolab-n1").await,
            Err("rbd map printed no device (success=true)".to_string())
        );
    }

    #[tokio::test]
    async fn a_probe_folder_that_cannot_be_made_reads_as_unusable_without_mounting() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tmp"), "").unwrap();
        let host = FakeHost::new();
        assert!(!filesystem_is_usable(&host, dir.path(), "/dev/rbd0").await);
        assert!(host.calls().is_empty(), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_mkfs_that_cannot_start_is_an_error() {
        let host = FakeHost::new()
            .unstartable("mkfs.xfs")
            .unstartable("mkfs.ext4");
        assert!(mkfs(&host, "/dev/rbd0", Filesystem::Xfs).await.is_err());
        assert!(mkfs(&host, "/dev/rbd0", Filesystem::Ext4).await.is_err());
    }

    #[tokio::test]
    async fn an_ext4_store_is_made_without_discarding_the_image() {
        let host = FakeHost::new().ok("mkfs.ext4", "");
        mkfs(&host, "/dev/rbd0", Filesystem::Ext4).await.unwrap();
        assert!(host.ran("mkfs.ext4 -q -F -m0 -E nodiscard /dev/rbd0"));
    }
}
