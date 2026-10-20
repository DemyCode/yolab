use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::host::Host;

use super::containerd_store::{mount_resting_on, PODS_SLICE};

const SHIM: &str = "containerd-shim-runc-v2";
const CONTAINERD: &str = "containerd";
const UMOUNT_TIMEOUT: Duration = Duration::from_secs(60);
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
pub struct Waits {
    pub k3s: Duration,
    pub pods: Duration,
    pub poll: Duration,
}

pub const WAITS: Waits = Waits {
    k3s: Duration::from_secs(45),
    pods: Duration::from_secs(30),
    poll: Duration::from_secs(1),
};

pub async fn run<H: Host>(host: &H, root: &Path, k3s_unit: &str, waits: Waits) -> Result<()> {
    let k3s_down = wait_until(waits.k3s, waits.poll, move || async move {
        !host.unit_is_active(k3s_unit).await
    })
    .await;
    if !k3s_down {
        bail!(
            "{k3s_unit} is still running after {}s — not killing pods it would restart",
            waits.k3s.as_secs()
        );
    }

    stop_pods(host, root, waits).await;
    kill(host, "-TERM", &["-x", CONTAINERD]).await;
    kill(host, "-KILL", &["-f", SHIM]).await;
    let containerd_gone = wait_until(waits.pods, waits.poll, move || async move {
        !process_named(root, CONTAINERD)
    })
    .await;
    if !containerd_gone {
        kill(host, "-KILL", &["-x", CONTAINERD]).await;
    }

    let mut stuck = Vec::new();
    for target in ceph_backed_mounts(root) {
        if !unmount(host, &target).await {
            stuck.push(target.display().to_string());
        }
    }

    let synced = host.run_cmd_bounded("sync", &[], SYNC_TIMEOUT).await?;
    if !synced.success {
        bail!("sync failed: {}", synced.stderr.trim());
    }
    if !stuck.is_empty() {
        bail!("could not unmount {} before Ceph stops", stuck.join(", "));
    }
    Ok(())
}

async fn wait_until<F, Fut>(limit: Duration, poll: Duration, done: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if done().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

async fn stop_pods<H: Host>(host: &H, root: &Path, waits: Waits) {
    if !pods_running(root) {
        return;
    }
    signal_pods(host, "--signal=SIGTERM").await;
    if wait_until(waits.pods, waits.poll, move || async move { !pods_running(root) }).await {
        return;
    }
    tracing::warn!(
        "pods still running {}s after SIGTERM — killing them",
        waits.pods.as_secs()
    );
    signal_pods(host, "--signal=SIGKILL").await;
    if !wait_until(waits.pods, waits.poll, move || async move { !pods_running(root) }).await {
        tracing::warn!("pods still running after SIGKILL — unmounting under them");
    }
}

async fn signal_pods<H: Host>(host: &H, signal: &str) {
    let out = host.systemctl(&["kill", signal, PODS_SLICE]).await;
    if !out.as_ref().is_ok_and(|o| o.success) {
        tracing::warn!("systemctl kill {signal} {PODS_SLICE}: {}", reason(&out));
    }
}

fn pods_running(root: &Path) -> bool {
    let events = root.join("sys/fs/cgroup").join(PODS_SLICE).join("cgroup.events");
    std::fs::read_to_string(events)
        .map(|text| text.lines().any(|l| l.trim() == "populated 1"))
        .unwrap_or(false)
}

fn process_named(root: &Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("proc")) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let pid = e.file_name();
        pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit())
            && std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == name)
    })
}

async fn kill<H: Host>(host: &H, signal: &str, matching: &[&str]) {
    let args: Vec<&str> = std::iter::once(signal).chain(matching.iter().copied()).collect();
    match host.run_cmd("pkill", &args).await {
        Ok(o) if !o.success && !o.stderr.trim().is_empty() => {
            tracing::warn!("pkill {}: {}", args.join(" "), o.stderr.trim())
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("pkill {}: {e}", args.join(" ")),
    }
}

async fn unmount<H: Host>(host: &H, target: &Path) -> bool {
    let target_s = target.to_string_lossy().into_owned();
    let out = host.run_cmd_bounded("umount", &[target_s.as_str()], UMOUNT_TIMEOUT).await;
    if out.as_ref().is_ok_and(|o| o.success) {
        return true;
    }
    tracing::warn!("unmount {target_s} failed ({}) — detaching it lazily", reason(&out));
    let lazy = host.run_cmd_bounded("umount", &["-l", target_s.as_str()], UMOUNT_TIMEOUT).await;
    if lazy.as_ref().is_ok_and(|o| o.success) {
        return true;
    }
    tracing::warn!("lazy unmount of {target_s} failed: {}", reason(&lazy));
    false
}

fn reason(out: &crate::host::HostResult<crate::host::CommandOutput>) -> String {
    match out {
        Ok(o) => o.stderr.trim().to_string(),
        Err(e) => e.to_string(),
    }
}

struct Mount {
    target: PathBuf,
    fstype: String,
    source: String,
}

fn parse(line: &str) -> Option<Mount> {
    let target = PathBuf::from(line.split(' ').nth(4)?);
    let mut after = line.split(" - ").nth(1)?.split(' ');
    Some(Mount {
        target,
        fstype: after.next()?.to_string(),
        source: after.next()?.to_string(),
    })
}

fn is_ceph_backed(m: &Mount) -> bool {
    m.fstype == "ceph" || m.source.starts_with("/dev/rbd")
}

pub fn ceph_backed_mounts(root: &Path) -> Vec<PathBuf> {
    let mountinfo = std::fs::read_to_string(root.join("proc/self/mountinfo")).unwrap_or_default();
    let lines: Vec<&str> = mountinfo.lines().collect();
    let backed: Vec<PathBuf> = lines
        .iter()
        .filter_map(|l| parse(l))
        .filter(is_ceph_backed)
        .map(|m| m.target)
        .collect();

    let mut resting: Vec<PathBuf> = backed
        .iter()
        .flat_map(|base| lines.iter().filter_map(|l| mount_resting_on(l, base)))
        .filter(|t| !backed.contains(t))
        .collect();
    resting.sort_by(|a, b| b.cmp(a));
    resting.dedup();

    let mut own = backed;
    own.sort_by(|a, b| b.cmp(a));
    own.dedup();

    resting.into_iter().chain(own).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const NO_WAIT: Waits = Waits {
        k3s: Duration::ZERO,
        pods: Duration::ZERO,
        poll: Duration::ZERO,
    };

    const MOUNTINFO: &str = "\
22 1 254:0 / / rw,relatime - ext4 /dev/mapper/pool-root rw
30 22 251:0 / /var/lib/rancher/k3s/agent/containerd rw - xfs /dev/rbd0 rw
31 22 0:50 / /run/k3s/containerd/io.containerd.runtime.v2.task/k8s.io/abc/rootfs rw - overlay overlay rw,lowerdir=/var/lib/rancher/k3s/agent/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots/9/fs,upperdir=/var/lib/rancher/k3s/agent/containerd/io.containerd.snapshotter.v1.overlayfs/snapshots/10/fs
40 22 0:60 /volumes/csi/x /var/lib/kubelet/plugins/kubernetes.io/csi/cephfs.csi.ceph.com/h/globalmount rw - ceph [fd00:cafe::8]:6789:/volumes/csi/x rw
41 22 0:60 /volumes/csi/x /var/lib/kubelet/pods/u/volumes/kubernetes.io~csi/pvc-1/mount rw - ceph [fd00:cafe::8]:6789:/volumes/csi/x rw
42 22 0:60 /volumes/csi/x/app/state /var/lib/kubelet/pods/u/volume-subpaths/pvc-1/app/0 rw - ceph [fd00:cafe::8]:6789:/volumes/csi/x rw
50 22 0:70 / /run/user/0 rw - tmpfs tmpfs rw
";

    fn machine(mountinfo: &str, pods: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proc/self")).unwrap();
        std::fs::write(root.path().join("proc/self/mountinfo"), mountinfo).unwrap();
        let slice = root.path().join("sys/fs/cgroup").join(PODS_SLICE);
        std::fs::create_dir_all(&slice).unwrap();
        std::fs::write(slice.join("cgroup.events"), pods).unwrap();
        root
    }

    fn stopped_k3s() -> FakeHost {
        FakeHost::new()
            .fail("systemctl is-active --quiet k3s.service", "inactive")
            .ok("systemctl kill", "")
            .ok("pkill", "")
            .ok("umount", "")
            .ok("sync", "")
    }

    #[test]
    fn every_ceph_backed_mount_is_found_and_nothing_else() {
        let root = machine(MOUNTINFO, "populated 0\n");
        let found = ceph_backed_mounts(root.path());
        let names: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(names.len(), 5, "{names:?}");
        assert!(!names.iter().any(|n| n == "/" || n == "/run/user/0"));
    }

    #[test]
    fn overlays_on_the_image_store_come_off_before_the_store_itself() {
        let root = machine(MOUNTINFO, "populated 0\n");
        let found = ceph_backed_mounts(root.path());
        let overlay = found.iter().position(|p| p.ends_with("abc/rootfs")).unwrap();
        let store = found
            .iter()
            .position(|p| p == Path::new("/var/lib/rancher/k3s/agent/containerd"))
            .unwrap();
        assert!(overlay < store);
    }

    #[test]
    fn a_subpath_comes_off_before_the_volume_it_sits_in() {
        let mountinfo = "\
41 22 0:60 /v /var/lib/kubelet/pods/u/volumes/pvc-1/mount rw - ceph m:/v rw
42 22 0:60 /v/app /var/lib/kubelet/pods/u/volumes/pvc-1/mount/app rw - ceph m:/v rw
";
        let root = machine(mountinfo, "populated 0\n");
        let found = ceph_backed_mounts(root.path());
        assert_eq!(found[0], Path::new("/var/lib/kubelet/pods/u/volumes/pvc-1/mount/app"));
    }

    #[tokio::test]
    async fn nothing_is_killed_while_k3s_is_still_running() {
        let root = machine(MOUNTINFO, "populated 1\n");
        let host = FakeHost::new().ok("systemctl is-active --quiet k3s.service", "");
        let err = run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap_err();
        assert!(err.to_string().contains("still running"));
        assert!(!host.ran("systemctl kill"));
        assert!(!host.ran("pkill"));
        assert!(!host.ran("umount"));
    }

    #[tokio::test]
    async fn running_pods_are_asked_to_stop_before_anything_is_unmounted() {
        let root = machine(MOUNTINFO, "populated 1\n");
        let events = root.path().join("sys/fs/cgroup").join(PODS_SLICE).join("cgroup.events");
        let host = stopped_k3s().effect("systemctl kill --signal=SIGTERM", move |_, _| {
            std::fs::write(&events, "populated 0\n").unwrap();
        });
        run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap();
        let term = host.position("systemctl kill --signal=SIGTERM kubepods.slice").unwrap();
        let first_umount = host.position("umount").unwrap();
        assert!(term < first_umount);
        assert!(!host.ran("--signal=SIGKILL"));
    }

    #[tokio::test]
    async fn pods_that_ignore_sigterm_are_killed() {
        let root = machine(MOUNTINFO, "populated 1\n");
        let host = stopped_k3s();
        run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap();
        assert!(host.ran("systemctl kill --signal=SIGKILL kubepods.slice"));
    }

    #[tokio::test]
    async fn containerd_and_the_shims_go_before_the_image_store_is_unmounted() {
        let root = machine(MOUNTINFO, "populated 0\n");
        let host = stopped_k3s();
        run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap();
        let containerd = host.position("pkill -TERM -x containerd").unwrap();
        let shims = host.position("pkill -KILL -f containerd-shim-runc-v2").unwrap();
        let store = host.position("umount /var/lib/rancher/k3s/agent/containerd").unwrap();
        assert!(containerd < store && shims < store);
    }

    #[tokio::test]
    async fn everything_is_flushed_after_the_unmounts() {
        let root = machine(MOUNTINFO, "populated 0\n");
        let host = stopped_k3s();
        run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap();
        let calls = host.calls();
        let last_umount = calls.iter().rposition(|c| c.starts_with("umount")).unwrap();
        let sync = calls.iter().rposition(|c| c.starts_with("sync")).unwrap();
        assert!(sync > last_umount);
    }

    #[tokio::test]
    async fn a_busy_mount_is_detached_lazily() {
        let mountinfo = "41 22 0:60 /v /mnt/vol rw - ceph m:/v rw\n";
        let root = machine(mountinfo, "populated 0\n");
        let host = stopped_k3s()
            .fail("umount /mnt/vol", "target is busy")
            .ok("umount -l /mnt/vol", "");
        run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap();
        assert!(host.ran("umount -l /mnt/vol"));
    }

    #[tokio::test]
    async fn a_mount_that_will_not_come_off_is_an_error_after_syncing() {
        let mountinfo = "41 22 0:60 /v /mnt/vol rw - ceph m:/v rw\n";
        let root = machine(mountinfo, "populated 0\n");
        let host = stopped_k3s()
            .fail("umount /mnt/vol", "target is busy")
            .fail("umount -l /mnt/vol", "invalid argument");
        let err = run(&host, root.path(), "k3s.service", NO_WAIT).await.unwrap_err();
        assert!(err.to_string().contains("/mnt/vol"));
        assert!(host.ran("sync"));
    }
}
