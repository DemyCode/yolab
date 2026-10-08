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
    root.join(format!("tmp/yolab-containerd-probe-{}", std::process::id()))
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

    let fresh = if !has_filesystem(host, &dev).await {
        tracing::info!("{dev} is blank — creating {:?}", policy.filesystem);
        true
    } else if !filesystem_is_usable(host, root, &dev).await {
        tracing::warn!(
            "the image store on {dev} was not built by this swap, will not mount, read or \
             start pods, or names layers it no longer has — rebuilding it empty; images are \
             pulled again"
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
    if let Err(e) = mark_in_use(&croot) {
        bail!("{croot_s} was mounted from {dev} but cannot be marked in use: {e}");
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
    let usable = mounted
        && is_readable_dir(&probe)
        && is_built_here(&probe)
        && was_released_cleanly(&probe)
        && snapshotter_is_coherent(&probe)
        && missing_layers(&probe, &containerd_root(root)).is_empty();
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

const IN_USE: &str = "yolab-in-use";
const RELEASED: &str = "yolab-released-cleanly";

pub const RELEASE_UNIT: &str = "yolab-image-store.service";

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

fn mark_in_use(store: &Path) -> std::io::Result<()> {
    std::fs::write(store.join(IN_USE), b"")?;
    match std::fs::remove_file(store.join(RELEASED)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    sync_dir(store)
}

fn mark_released(store: &Path) -> std::io::Result<()> {
    std::fs::write(store.join(RELEASED), b"")?;
    std::fs::remove_file(store.join(IN_USE))?;
    sync_dir(store)
}

fn is_marked_in_use(store: &Path) -> bool {
    store.join(IN_USE).is_file()
}

fn was_released_cleanly(store: &Path) -> bool {
    store.join(RELEASED).is_file() && !is_marked_in_use(store)
}

const CONTAINERD_LOG: &str = "containerd.log";
const LOG_TAIL_BYTES: u64 = 4 << 20;
const OVERLAY_SNAPSHOTS: &str = "io.containerd.snapshotter.v1.overlayfs/snapshots";
const MISSING_LAYER: &str = "/fs: no such file or directory";

fn log_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn layer_named_missing(line: &str, snapshots: &str) -> Option<String> {
    if !line.contains("failed to create containerd container") {
        return None;
    }
    let (before, _) = line.split_once(MISSING_LAYER)?;
    let id = &before[before.rfind(snapshots)? + snapshots.len()..];
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn missing_layers(store: &Path, seen_at: &Path) -> Vec<String> {
    let snapshots = format!("{}/", seen_at.join(OVERLAY_SNAPSHOTS).display());
    let mut out: Vec<String> = log_tail(&store.join(CONTAINERD_LOG))
        .lines()
        .filter_map(|l| layer_named_missing(l, &snapshots))
        .filter(|id| !store.join(OVERLAY_SNAPSHOTS).join(id).join("fs").exists())
        .collect();
    out.sort();
    out.dedup();
    out
}

async fn is_mounted<H: Host>(host: &H, root: &Path) -> bool {
    let croot = containerd_root(root);
    is_mountpoint(host, &croot.to_string_lossy()).await
}

pub async fn workloads_run_here<H: Host>(host: &H, k3s_unit: &str) -> bool {
    host.systemctl(&["is-active", "--quiet", k3s_unit])
        .await
        .is_ok_and(|o| o.success)
}

fn is_whole(store: &Path) -> bool {
    is_built_here(store) && is_marked_in_use(store) && missing_layers(store, store).is_empty()
}

async fn release_is_armed<H: Host>(host: &H) -> bool {
    host.systemctl(&["is-active", "--quiet", RELEASE_UNIT])
        .await
        .is_ok_and(|o| o.success)
}

async fn arm_release<H: Host>(host: &H) -> Attempt<()> {
    match host.systemctl(&["start", "--no-block", RELEASE_UNIT]).await {
        Ok(o) if o.success => Attempt::Ready(()),
        Ok(o) => Attempt::NotYet(format!(
            "could not arm {RELEASE_UNIT}, which unmounts the image store before Ceph stops: {}",
            o.stderr.trim()
        )),
        Err(e) => Attempt::NotYet(format!("could not arm {RELEASE_UNIT}: {e}")),
    }
}

pub async fn is_in_place<H: Host>(host: &H, root: &Path) -> bool {
    is_mounted(host, root).await && is_whole(&containerd_root(root)) && release_is_armed(host).await
}

const CONTAINERD: &str = "containerd";

pub async fn release<H: Host>(host: &H, root: &Path, k3s_unit: &str) -> Result<()> {
    if !is_mounted(host, root).await {
        return Ok(());
    }
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    let stopped = host.systemctl(&["stop", k3s_unit]).await?;
    if !stopped.success {
        bail!(
            "could not stop {k3s_unit} to release the image store: {}",
            stopped.stderr.trim()
        );
    }
    let _ = host.run_cmd("pkill", &["-TERM", "-x", CONTAINERD]).await;
    if let Some(why) = quiesce(host, root).await {
        bail!("{why}");
    }
    let marked = is_marked_in_use(&croot);
    if marked {
        mark_released(&croot)?;
    } else {
        tracing::warn!(
            "the image store at {croot_s} was not mounted by a swap that marks it — unmounting it \
             unmarked, so it is rebuilt empty at the next swap"
        );
    }
    let out = host
        .run_cmd_bounded("umount", &[croot_s.as_str()], FS_OP_TIMEOUT)
        .await?;
    if !out.success {
        if marked {
            mark_in_use(&croot)?;
        }
        bail!("umount {croot_s}: {}", out.stderr.trim());
    }
    Ok(())
}

pub const PODS_SLICE: &str = "kubepods.slice";
const SHIM: &str = "containerd-shim-runc-v2";
const QUIET_CHECKS: u32 = 10;
const QUIET_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

pub async fn pivot<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
    k3s_unit: &str,
) -> Result<Attempt<()>> {
    if is_mounted(host, root).await && is_whole(&containerd_root(root)) {
        return Ok(arm_release(host).await);
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

    match super::pivot_lock::claim(host, node, crate::system::now_secs()).await? {
        super::pivot_lock::Claim::Held => {}
        super::pivot_lock::Claim::Busy(why) => return Ok(Attempt::NotYet(why)),
    }
    let outcome = swap(host, root, node, policy, k3s_unit).await;
    super::pivot_lock::release(host, node).await;
    match outcome? {
        Attempt::Ready(()) => Ok(arm_release(host).await),
        not_yet => Ok(not_yet),
    }
}

async fn swap<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
    k3s_unit: &str,
) -> Result<Attempt<()>> {
    let stopped = host.systemctl(&["stop", k3s_unit]).await?;
    if !stopped.success {
        return Ok(Attempt::NotYet(format!(
            "could not stop {k3s_unit} to swap the image store: {}",
            stopped.stderr.trim()
        )));
    }

    let outcome = match quiesce(host, root).await {
        Some(why) => Ok(Attempt::NotYet(why)),
        None => match unmount_untrusted(host, root).await? {
            Some(why) => Ok(Attempt::NotYet(why)),
            None => attempt(host, root, node, policy).await,
        },
    };

    if host
        .systemctl(&["is-enabled", "--quiet", k3s_unit])
        .await
        .is_ok_and(|o| o.success)
    {
        let _ = host.systemctl(&["reset-failed", k3s_unit]).await;
        host.systemctl(&["start", "--no-block", k3s_unit])
            .await
            .warn_on_err(format!("start {k3s_unit} after swapping the image store"));
    }
    outcome
}

async fn unmount_untrusted<H: Host>(host: &H, root: &Path) -> Result<Option<String>> {
    if !is_mounted(host, root).await {
        return Ok(None);
    }
    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    let missing = missing_layers(&croot, &croot);
    if missing.is_empty() {
        tracing::warn!(
            "the image store at {croot_s} was not built and marked in use by this swap — \
             rebuilding it"
        );
    } else {
        tracing::warn!(
            "the image store at {croot_s} fails to create containers because layers {} are \
             gone — rebuilding it",
            missing.join(", ")
        );
    }
    let out = host.run_cmd("umount", &[croot_s.as_str()]).await?;
    if out.success {
        return Ok(None);
    }
    Ok(Some(format!(
        "could not unmount the untrusted image store at {croot_s}: {}",
        out.stderr.trim()
    )))
}

async fn quiesce<H: Host>(host: &H, root: &Path) -> Option<String> {
    host.systemctl(&["stop", PODS_SLICE])
        .await
        .warn_on_err(format!("stop {PODS_SLICE}"));
    let _ = host.run_cmd("pkill", &["-KILL", "-f", SHIM]).await;
    let data_root = containerd_root(root);
    for target in mounts_resting_on(root, &data_root) {
        let target_s = target.to_string_lossy().into_owned();
        host.run_cmd("umount", &[target_s.as_str()])
            .await
            .warn_on_err(format!("unmount {target_s}, left behind by a killed shim"));
    }
    for check in 1..=QUIET_CHECKS {
        let holders = holders_of(root, &data_root);
        if holders.is_empty() {
            return None;
        }
        if check == QUIET_CHECKS {
            return Some(format!(
                "containerd's data-root is still in use by {} — not mounting over it",
                holders.join(", ")
            ));
        }
        tokio::time::sleep(QUIET_WAIT).await;
    }
    None
}

fn uses_data_root(pid: &Path, data_root: &Path) -> bool {
    let mut links = vec![pid.join("cwd"), pid.join("root")];
    if let Ok(fds) = std::fs::read_dir(pid.join("fd")) {
        links.extend(fds.filter_map(|fd| fd.ok()).map(|fd| fd.path()));
    }
    links
        .iter()
        .filter_map(|link| std::fs::read_link(link).ok())
        .any(|target| target.starts_with(data_root))
}

fn holders_of(root: &Path, data_root: &Path) -> Vec<String> {
    let proc = root.join("proc");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&proc) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if uses_data_root(&entry.path(), data_root) {
                let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                out.push(format!("pid {name} ({})", comm.trim()));
            }
        }
    }
    for target in mounts_resting_on(root, data_root) {
        out.push(format!("the mount at {}", target.display()));
    }
    out.sort();
    out
}

fn mount_resting_on(line: &str, data_root: &Path) -> Option<PathBuf> {
    let target = Path::new(line.split(' ').nth(4)?);
    let inside = target != data_root && target.starts_with(data_root);
    let layer = format!("{}/", data_root.display());
    let built_on = line
        .split(" - ")
        .nth(1)
        .is_some_and(|sb| sb.split(' ').skip(1).any(|f| f.contains(&layer)));
    (inside || built_on).then(|| target.to_path_buf())
}

fn mounts_resting_on(root: &Path, data_root: &Path) -> Vec<PathBuf> {
    let mountinfo = std::fs::read_to_string(root.join("proc/self/mountinfo")).unwrap_or_default();
    let mut out: Vec<PathBuf> = mountinfo
        .lines()
        .filter_map(|line| mount_resting_on(line, data_root))
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.dedup();
    out
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

    #[tokio::test]
    async fn workloads_run_here_only_while_k3s_is_active() {
        let active = FakeHost::new().ok("systemctl is-active --quiet k3s.service", "");
        assert!(workloads_run_here(&active, "k3s.service").await);

        let stuck_starting = FakeHost::new().fail("systemctl is-active --quiet k3s.service", "");
        assert!(
            !workloads_run_here(&stuck_starting, "k3s.service").await,
            "a k3s that cannot start because its image store is dead runs no workloads to disrupt"
        );
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
        assert!(host.ran("mkfs.xfs -f -K -m crc=1 /dev/rbd0"));
        assert!(host.ran(&format!("mount /dev/rbd0 {}", croot.display())));
        assert!(is_built_here(&croot));
    }

    fn built_here_on_probe(root: &Path) {
        let probe = probe_dir(root);
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join(BUILT_HERE), b"").unwrap();
        std::fs::write(probe.join(RELEASED), b"").unwrap();
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

    fn built_here_in_place(root: &Path) {
        let croot = containerd_root(root);
        std::fs::create_dir_all(&croot).unwrap();
        std::fs::write(croot.join(BUILT_HERE), b"").unwrap();
        std::fs::write(croot.join(IN_USE), b"").unwrap();
    }

    fn mounted_and_armed() -> FakeHost {
        FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("systemctl is-active --quiet yolab-image-store.service", "")
    }

    #[tokio::test]
    async fn an_already_mounted_store_is_not_pivoted_again() {
        let dir = tempfile::tempdir().unwrap();
        built_here_in_place(dir.path());
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("systemctl start", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        assert!(!host.ran("systemctl stop"));
        assert!(!ran_mount(&host));
    }

    #[tokio::test]
    async fn only_a_mounted_store_this_swap_built_is_in_place() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_in_place(&mounted_and_armed(), dir.path()).await);
        built_here_in_place(dir.path());
        assert!(is_in_place(&mounted_and_armed(), dir.path()).await);
        assert!(!is_in_place(&booting(), dir.path()).await);
    }

    #[tokio::test]
    async fn a_store_no_swap_marked_in_use_is_not_in_place() {
        let dir = tempfile::tempdir().unwrap();
        built_here_in_place(dir.path());
        std::fs::remove_file(containerd_root(dir.path()).join(IN_USE)).unwrap();
        assert!(!is_in_place(&mounted_and_armed(), dir.path()).await);
    }

    #[tokio::test]
    async fn a_store_is_in_place_only_while_its_release_at_shutdown_is_armed() {
        let dir = tempfile::tempdir().unwrap();
        built_here_in_place(dir.path());
        let unarmed = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .fail("systemctl is-active", "inactive");
        assert!(!is_in_place(&unarmed, dir.path()).await);
    }

    #[tokio::test]
    async fn a_whole_mounted_store_only_has_its_release_armed_and_k3s_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        built_here_in_place(dir.path());
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("systemctl start", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        assert!(host.ran("systemctl start --no-block yolab-image-store.service"));
        assert!(!host.ran("systemctl stop"));
    }

    #[tokio::test]
    async fn a_release_that_cannot_be_armed_is_waited_for_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        built_here_in_place(dir.path());
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .fail(
                "systemctl start",
                "Unit yolab-image-store.service not found.",
            );
        let why = not_yet(
            pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
                .await
                .unwrap(),
        );
        assert!(why.contains("not found"), "{why}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_swap_arms_the_release_once_the_store_is_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let host = swappable();
        pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(
            first_mount(&host)
                < at(
                    &host,
                    "systemctl start --no-block yolab-image-store.service"
                ),
            "{:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_store_mounted_after_a_clean_release_is_marked_in_use_again() {
        let dir = tempfile::tempdir().unwrap();
        built_here_on_probe(dir.path());
        let croot = containerd_root(dir.path());
        std::fs::create_dir_all(&croot).unwrap();
        std::fs::write(croot.join(RELEASED), b"").unwrap();
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "");

        attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert!(!host.ran("mkfs"));
        assert!(is_marked_in_use(&croot));
        assert!(!croot.join(RELEASED).exists());
    }

    #[tokio::test]
    async fn a_store_that_was_never_released_cleanly_is_rebuilt() {
        for leftover in [None, Some(IN_USE)] {
            let dir = tempfile::tempdir().unwrap();
            let probe = probe_dir(dir.path());
            std::fs::create_dir_all(&probe).unwrap();
            std::fs::write(probe.join(BUILT_HERE), b"").unwrap();
            if let Some(marker) = leftover {
                std::fs::write(probe.join(RELEASED), b"").unwrap();
                std::fs::write(probe.join(marker), b"").unwrap();
            }
            let host = mapped()
                .ok("blkid /dev/rbd0", "TYPE=xfs")
                .ok("mount", "")
                .ok("umount", "")
                .ok("mkfs.xfs", "");

            attempt(&host, dir.path(), "yolab-n1", &policy())
                .await
                .unwrap();

            assert!(host.ran("mkfs.xfs"), "leftover={leftover:?}");
        }
    }

    fn releasable() -> FakeHost {
        FakeHost::new()
            .ok(FINDMNT, "")
            .ok("pkill", "")
            .ok("systemctl stop", "")
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_stops_containerd_then_marks_the_store_clean_and_unmounts_it() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        let host = releasable().ok("umount", "");

        release(&host, dir.path(), K3S).await.unwrap();

        let k3s = at(&host, "systemctl stop k3s.service");
        let containerd = at(&host, "pkill -TERM -x containerd");
        let shims = at(&host, "pkill -KILL -f containerd-shim-runc-v2");
        let unmounted = at(&host, &format!("umount {}", croot.display()));
        assert!(
            k3s < containerd && containerd < shims && shims < unmounted,
            "{:?}",
            host.calls()
        );
        assert!(was_released_cleanly(&croot));
    }

    #[tokio::test(start_paused = true)]
    async fn a_k3s_that_will_not_stop_keeps_its_image_store_mounted_and_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        let host = releasable()
            .fail("systemctl stop k3s.service", "Job canceled")
            .ok("umount", "");

        let err = release(&host, dir.path(), K3S).await.unwrap_err().to_string();

        assert!(err.contains("could not stop k3s.service"), "{err}");
        assert!(!host.ran("pkill"));
        assert!(!host.ran(&format!("umount {}", croot.display())));
        assert!(is_marked_in_use(&croot));
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_that_cannot_unmount_leaves_the_store_marked_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        let host = releasable().fail("umount", "target is busy");

        assert!(release(&host, dir.path(), K3S).await.is_err());

        assert!(is_marked_in_use(&croot));
        assert!(!was_released_cleanly(&croot));
    }

    #[tokio::test(start_paused = true)]
    async fn a_store_no_swap_marked_is_unmounted_but_never_marked_clean() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        std::fs::remove_file(croot.join(IN_USE)).unwrap();
        let host = releasable().ok("umount", "");

        release(&host, dir.path(), K3S).await.unwrap();

        assert!(host.ran(&format!("umount {}", croot.display())));
        assert!(!croot.join(RELEASED).exists());
    }

    #[tokio::test]
    async fn releasing_a_store_that_is_not_mounted_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().fail(FINDMNT, "");
        release(&host, dir.path(), K3S).await.unwrap();
        assert_eq!(host.calls().len(), 1, "{:?}", host.calls());
    }

    #[tokio::test(start_paused = true)]
    async fn a_store_something_still_holds_is_not_unmounted_at_release() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        a_process_holding(dir.path(), 4242, "containerd", &croot.join("meta.db"));
        let host = releasable().ok("umount", "");

        let err = release(&host, dir.path(), K3S).await.unwrap_err().to_string();

        assert!(err.contains("pid 4242 (containerd)"), "{err}");
        assert!(!host.ran(&format!("umount {}", croot.display())));
        assert!(is_marked_in_use(&croot));
    }

    const CLAIM: &str = "ceph config-key get yolab/containerd-pivot";
    const MINE: &str = r#"{"node":"yolab-n1","until":99999999999}"#;

    fn claimable(host: FakeHost) -> FakeHost {
        host.fail(CLAIM, "Error ENOENT: no such key")
            .ok(CLAIM, MINE)
            .ok("ceph config-key set", "")
            .ok("ceph config-key rm", "")
    }

    fn swap_base() -> FakeHost {
        claimable(
            mapped()
                .fail("blkid", "")
                .ok("mkfs.xfs", "")
                .ok("systemctl stop", "")
                .ok("systemctl reset-failed", "")
                .ok("systemctl start", ""),
        )
    }

    fn swappable() -> FakeHost {
        swap_base().ok("mount ", "").ok("systemctl is-enabled", "")
    }

    fn at(host: &FakeHost, needle: &str) -> usize {
        host.position(needle)
            .unwrap_or_else(|| panic!("never ran {needle}: {:?}", host.calls()))
    }

    fn first_mount(host: &FakeHost) -> usize {
        host.calls()
            .iter()
            .position(|c| c.starts_with("mount "))
            .unwrap_or_else(|| panic!("never mounted: {:?}", host.calls()))
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_runs_on_the_old_store_when_the_new_one_is_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let host = swappable();
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        let k3s = at(&host, "systemctl stop k3s.service");
        let pods = at(&host, "systemctl stop kubepods.slice");
        let shims = at(&host, "pkill -KILL -f containerd-shim-runc-v2");
        let mounted = first_mount(&host);
        let started = at(&host, "systemctl start --no-block k3s.service");
        assert!(
            k3s < pods && pods < shims && shims < mounted && mounted < started,
            "{:?}",
            host.calls()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_k3s_that_is_still_starting_is_stopped_like_a_running_one() {
        let dir = tempfile::tempdir().unwrap();
        let host = swappable().fail("systemctl is-active", "activating");
        pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(
            at(&host, "systemctl stop k3s.service") < first_mount(&host),
            "the data-root was mounted over a k3s that was only starting: {:?}",
            host.calls()
        );
        assert!(!host.ran("systemctl is-active"));
    }

    #[tokio::test(start_paused = true)]
    async fn k3s_is_started_after_the_swap_whether_or_not_it_was_running_before() {
        let dir = tempfile::tempdir().unwrap();
        let host = swappable().fail("systemctl is-active", "inactive");
        pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(host.ran("systemctl start --no-block k3s.service"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_k3s_that_is_switched_off_stays_off() {
        let dir = tempfile::tempdir().unwrap();
        let host = swap_base()
            .ok("mount ", "")
            .fail("systemctl is-enabled", "disabled");
        pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(!host.ran("systemctl start --no-block k3s.service"));
    }

    #[tokio::test(start_paused = true)]
    async fn k3s_is_brought_back_even_when_the_mount_fails() {
        let dir = tempfile::tempdir().unwrap();
        let host = swap_base()
            .fail("mount ", "no such device")
            .ok("systemctl is-enabled", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("mount"));
        assert!(host.ran("systemctl start --no-block k3s.service"));
        assert!(host.ran("ceph config-key rm yolab/containerd-pivot"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_k3s_that_will_not_stop_is_never_mounted_under() {
        let dir = tempfile::tempdir().unwrap();
        let host = swappable().fail("systemctl stop k3s.service", "Job canceled");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("could not stop"));
        assert!(!host.calls().iter().any(|c| c.starts_with("mount ")));
        assert!(host.ran("ceph config-key rm yolab/containerd-pivot"));
    }

    const FINDMNT: &str = "findmnt -rno TARGET --mountpoint";

    fn mounted_untrusted() -> FakeHost {
        claimable(
            FakeHost::new()
                .ok(FINDMNT, "")
                .ok(FINDMNT, "")
                .fail(FINDMNT, "")
                .ok("rbd ls images", "yolab-n1\n")
                .ok("rbd showmapped --format json", "[]")
                .ok("rbd map images/yolab-n1", "/dev/rbd0\n")
                .ok("blkid /dev/rbd0", "TYPE=xfs")
                .ok("mkfs.xfs", "")
                .ok("mount ", "")
                .ok("systemctl stop", "")
                .ok("systemctl reset-failed", "")
                .ok("systemctl start", "")
                .ok("systemctl is-enabled", ""),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_mounted_store_this_swap_did_not_build_is_rebuilt_while_k3s_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        let host = mounted_untrusted().ok("umount", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        let stopped = at(&host, "systemctl stop k3s.service");
        let unmounted = at(&host, &format!("umount {}", croot.display()));
        let rebuilt = at(&host, "mkfs.xfs -f -K -m crc=1 /dev/rbd0");
        let remounted = at(&host, &format!("mount /dev/rbd0 {}", croot.display()));
        let started = at(&host, "systemctl start --no-block k3s.service");
        assert!(
            stopped < unmounted
                && unmounted < rebuilt
                && rebuilt < remounted
                && remounted < started,
            "{:?}",
            host.calls()
        );
        assert!(is_built_here(&croot));
    }

    #[tokio::test(start_paused = true)]
    async fn an_untrusted_store_that_will_not_unmount_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let host = mounted_untrusted().fail("umount", "target is busy");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("target is busy"));
        assert!(!host.ran("mkfs"));
        assert!(!ran_mount(&host));
        assert!(host.ran("systemctl start --no-block k3s.service"));
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_machine_swaps_its_image_store_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let host = mapped().ok(CLAIM, r#"{"node":"yolab-n2","until":99999999999}"#);
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(not_yet(out).contains("yolab-n2 is swapping"));
        assert!(!host.ran("systemctl stop"));
        assert!(!host.ran("config-key set"));
    }

    fn a_process_holding(root: &Path, pid: u32, comm: &str, target: &Path) {
        let fd = root.join(format!("proc/{pid}/fd"));
        std::fs::create_dir_all(&fd).unwrap();
        std::fs::write(root.join(format!("proc/{pid}/comm")), format!("{comm}\n")).unwrap();
        std::os::unix::fs::symlink(target, fd.join("3")).unwrap();
        std::os::unix::fs::symlink("/", root.join(format!("proc/{pid}/root"))).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_data_root_something_still_uses_is_not_mounted_over() {
        let dir = tempfile::tempdir().unwrap();
        let meta = containerd_root(dir.path()).join("io.containerd.metadata.v1.bolt/meta.db");
        a_process_holding(dir.path(), 12564, "containerd", &meta);
        let host = swappable();
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        let why = not_yet(out);
        assert!(why.contains("pid 12564 (containerd)"), "{why}");
        assert!(!host.calls().iter().any(|c| c.starts_with("mount ")));
        assert!(host.ran("systemctl start --no-block k3s.service"));
    }

    #[test]
    fn only_processes_and_mounts_inside_the_data_root_hold_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let data_root = containerd_root(root);
        a_process_holding(root, 10, "containerd", &data_root.join("meta.db"));
        a_process_holding(root, 11, "sshd", Path::new("/var/log/lastlog"));
        a_process_holding(
            root,
            12,
            "neighbour",
            &PathBuf::from(format!("{}-old/x", data_root.display())),
        );
        std::fs::create_dir_all(root.join("proc/13")).unwrap();
        std::os::unix::fs::symlink(&data_root, root.join("proc/13/cwd")).unwrap();
        std::fs::create_dir_all(root.join("proc/self")).unwrap();
        std::fs::write(
            root.join("proc/self/mountinfo"),
            format!(
                "22 1 0:21 / / rw - ext4 /dev/dm-0 rw\n\
                 90 22 0:50 / {dr} rw - xfs /dev/rbd0 rw\n\
                 91 22 0:51 / {dr}/io.containerd.grpc.v1.cri/sandboxes/x/shm rw - tmpfs shm rw\n",
                dr = data_root.display()
            ),
        )
        .unwrap();
        assert_eq!(
            holders_of(root, &data_root),
            vec![
                "pid 10 (containerd)".to_string(),
                "pid 13 ()".to_string(),
                format!(
                    "the mount at {}/io.containerd.grpc.v1.cri/sandboxes/x/shm",
                    data_root.display()
                ),
            ]
        );
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

    #[tokio::test(start_paused = true)]
    async fn a_rootfs_a_killed_shim_left_behind_is_unmounted_and_blocks_the_swap_until_gone() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = containerd_root(dir.path());
        std::fs::create_dir_all(dir.path().join("proc/self")).unwrap();
        std::fs::write(
            dir.path().join("proc/self/mountinfo"),
            orphaned_rootfs(&data_root),
        )
        .unwrap();
        let host = swappable().ok("umount", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert!(
            at(&host, "pkill -KILL") < at(&host, &format!("umount {ORPHAN}")),
            "{:?}",
            host.calls()
        );
        assert!(not_yet(out).contains(ORPHAN));
        assert!(!host.ran("mkfs"));
        assert!(!ran_mount(&host));
    }

    fn failed_create(seen_at: &Path, id: u32) -> String {
        format!(
            "time=\"2026-10-08T20:54:31Z\" level=error msg=\"CreateContainer within sandbox \
             \\\"442401\\\" for name:\\\"coredns\\\" attempt:20 failed\" error=\"rpc error: \
             code = Unknown desc = failed to create containerd container: open {}/{OVERLAY_SNAPSHOTS}/{id}/fs: \
             no such file or directory\"\n",
            seen_at.display()
        )
    }

    fn logged(store: &Path, text: &str) {
        std::fs::create_dir_all(store.join(OVERLAY_SNAPSHOTS)).unwrap();
        let mut log = std::fs::read_to_string(store.join(CONTAINERD_LOG)).unwrap_or_default();
        log.push_str(text);
        std::fs::write(store.join(CONTAINERD_LOG), log).unwrap();
    }

    #[test]
    fn only_a_container_that_could_not_be_created_names_a_missing_layer() {
        let dr = Path::new("/var/lib/rancher/k3s/agent/containerd");
        let snapshots = format!("{}/", dr.join(OVERLAY_SNAPSHOTS).display());
        assert_eq!(
            layer_named_missing(&failed_create(dr, 48), &snapshots),
            Some("48".into())
        );
        let usage = format!(
            "level=error msg=\"Failed to get usage for snapshot\" error=\"lstat {snapshots}1661/fs: \
             no such file or directory\""
        );
        assert_eq!(layer_named_missing(&usage, &snapshots), None);
        let elsewhere = failed_create(Path::new("/var/lib/other"), 48);
        assert_eq!(layer_named_missing(&elsewhere, &snapshots), None);
    }

    #[test]
    fn a_layer_is_missing_only_while_it_is_still_gone() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        logged(store, &failed_create(store, 48));
        logged(store, &failed_create(store, 48));
        logged(store, &failed_create(store, 7));
        std::fs::create_dir_all(store.join(OVERLAY_SNAPSHOTS).join("7/fs")).unwrap();
        assert_eq!(missing_layers(store, store), vec!["48".to_string()]);
    }

    #[test]
    fn a_store_with_no_log_misses_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(missing_layers(dir.path(), dir.path()).is_empty());
    }

    #[test]
    fn only_the_tail_of_a_long_log_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        logged(store, &failed_create(store, 48));
        logged(store, &"x".repeat(LOG_TAIL_BYTES as usize + 1));
        assert!(missing_layers(store, store).is_empty());
    }

    #[tokio::test]
    async fn a_marked_store_that_cannot_create_containers_is_not_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        assert!(is_in_place(&mounted_and_armed(), dir.path()).await);
        logged(&croot, &failed_create(&croot, 48));
        assert!(!is_in_place(&mounted_and_armed(), dir.path()).await);
    }

    #[tokio::test]
    async fn a_marked_store_whose_log_names_a_missing_layer_is_rebuilt_at_boot() {
        let dir = tempfile::tempdir().unwrap();
        built_here_on_probe(dir.path());
        snapshotter_at(&probe_dir(dir.path()), 262_144, 4);
        logged(
            &probe_dir(dir.path()),
            &failed_create(&containerd_root(dir.path()), 48),
        );
        let host = mapped()
            .ok("blkid /dev/rbd0", "TYPE=xfs")
            .ok("mount", "")
            .ok("umount", "")
            .ok("mkfs.xfs", "");

        attempt(&host, dir.path(), "yolab-n1", &policy())
            .await
            .unwrap();

        assert!(host.ran("mkfs.xfs"), "{:?}", host.calls());
    }

    #[tokio::test(start_paused = true)]
    async fn a_mounted_store_missing_layers_is_rebuilt_while_k3s_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let croot = containerd_root(dir.path());
        built_here_in_place(dir.path());
        logged(&croot, &failed_create(&croot, 48));
        let host = mounted_untrusted().ok("umount", "");
        let out = pivot(&host, dir.path(), "yolab-n1", &policy(), K3S)
            .await
            .unwrap();
        assert_eq!(out, Attempt::Ready(()));
        let stopped = at(&host, "systemctl stop k3s.service");
        let unmounted = at(&host, &format!("umount {}", croot.display()));
        let rebuilt = at(&host, "mkfs.xfs -f -K -m crc=1 /dev/rbd0");
        let started = at(&host, "systemctl start --no-block k3s.service");
        assert!(
            stopped < unmounted && unmounted < rebuilt && rebuilt < started,
            "{:?}",
            host.calls()
        );
    }

    #[test]
    fn a_machine_with_nothing_on_the_data_root_has_no_holders() {
        let dir = tempfile::tempdir().unwrap();
        assert!(holders_of(dir.path(), &containerd_root(dir.path())).is_empty());
    }
}
