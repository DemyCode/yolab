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
//! Nothing under containerd's data-root is the owner's data: every byte is a
//! container layer a registry will send again. So the right response to any
//! doubt here is to rebuild, never to try to preserve it.

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

async fn image_exists<H: Host>(host: &H, pool: &str, name: &str) -> bool {
    host.run_cmd("rbd", &["ls", pool])
        .await
        .map(|o| o.stdout.lines().any(|l| l.trim() == name))
        .unwrap_or(false)
}

async fn is_mountpoint<H: Host>(host: &H, path: &str) -> bool {
    host.run_cmd("mountpoint", &["-q", path])
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

fn dir_has_any_entries(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut e| e.next().is_some())
        .unwrap_or(false)
}

/// `.[] | select(.pool==pool and .name==name) | .device` from `rbd showmapped
/// --format json`. Pure and separate from the fetch so the shape of that JSON
/// (an array, `pool`/`name`/`device` keys) is testable without a real rbd.
fn find_mapped_device(showmapped: &Value, pool: &str, name: &str) -> Option<String> {
    showmapped.as_array()?.iter().find_map(|e| {
        (e["pool"] == pool && e["name"] == name)
            .then(|| e["device"].as_str())
            .flatten()
            .map(str::to_string)
    })
}

/// `rbd map` on an already-mapped image just reports it, so this is
/// idempotent. `osd_request_timeout=30` is THE setting behind the worst
/// failure this storage stack has had: krbd defaults to waiting forever, so
/// when the pool cannot serve a read, anything touching the device parks in
/// uninterruptible sleep — a state SIGKILL cannot end. With a timeout the
/// same situation produces a recoverable I/O error instead.
async fn mapped_device<H: Host>(host: &H, pool: &str, name: &str) -> Option<String> {
    if let Ok(out) = host
        .run_cmd(
            "rbd",
            &[
                "map",
                &format!("{pool}/{name}"),
                "-o",
                "osd_request_timeout=30",
            ],
        )
        .await
    {
        let dev = out.stdout.trim();
        if out.success && !dev.is_empty() {
            return Some(dev.to_string());
        }
    }
    let out = host
        .run_cmd("rbd", &["showmapped", "--format", "json"])
        .await
        .ok()?;
    let v: Value = serde_json::from_str(&out.stdout).ok()?;
    find_mapped_device(&v, pool, name)
}

async fn has_filesystem<H: Host>(host: &H, dev: &str) -> bool {
    host.run_cmd("blkid", &[dev])
        .await
        .map(|o| o.success)
        .unwrap_or(false)
}

async fn filesystem_is_healthy<H: Host>(host: &H, dev: &str, fs: Filesystem) -> bool {
    match fs {
        Filesystem::Xfs => host
            .run_cmd("xfs_repair", &["-n", dev])
            .await
            .map(|o| o.success)
            .unwrap_or(false),
        Filesystem::Ext4 => host
            .run_cmd("fsck.ext4", &["-n", "-f", dev])
            .await
            .map(|o| o.success)
            .unwrap_or(false),
    }
}

async fn mkfs<H: Host>(host: &H, dev: &str, fs: Filesystem) -> Result<()> {
    let ok = match fs {
        Filesystem::Xfs => {
            host.run_cmd("mkfs.xfs", &["-f", "-m", "crc=1", dev])
                .await?
                .success
        }
        Filesystem::Ext4 => {
            host.run_cmd("mkfs.ext4", &["-q", "-m0", dev])
                .await?
                .success
        }
    };
    if !ok {
        bail!("mkfs failed");
    }
    Ok(())
}

/// Copies the existing (root-disk) image store onto the freshly mapped
/// device via a staging mount, then clears the root copy. Every exit path —
/// mount failure, copy failure, success — unmounts and removes the staging
/// dir, matching the bash `trap`'s unconditional cleanup this replaces.
async fn migrate_existing_store<H: Host>(
    host: &H,
    root: &Path,
    croot: &Path,
    dev: &str,
) -> Result<()> {
    let stage = stage_dir(root);
    std::fs::create_dir_all(&stage)?;
    let stage_s = stage.to_string_lossy().into_owned();

    if !host
        .run_cmd("mount", &[dev, &stage_s])
        .await
        .is_ok_and(|o| o.success)
    {
        let _ = std::fs::remove_dir(&stage);
        bail!("could not mount {dev} for migration — staying on the root disk");
    }

    let croot_glob = format!("{}/.", croot.to_string_lossy());
    let stage_dest = format!("{stage_s}/");
    // -a preserves hardlinks, xattrs and sparseness, all of which
    // containerd's content store relies on.
    let copied = host
        .run_cmd("cp", &["-a", &croot_glob, &stage_dest])
        .await
        .is_ok_and(|o| o.success);

    // Unconditional, on both the success and failure branch below — this is
    // the replacement for the bash `trap cleanup EXIT` that covered the
    // staging mount regardless of how the script left this block.
    let _ = host.run_cmd("umount", &[stage_s.as_str()]).await;
    let _ = std::fs::remove_dir(&stage);

    if !copied {
        // Most likely the image is smaller than the existing store — roll
        // back rather than mount a half-populated store, which containerd
        // would read as a corrupt content store.
        bail!("copy failed (is the RBD large enough?) — staying on the root disk");
    }

    // Remove and recreate rather than clearing the directory's contents in
    // place: a glob misses dotfiles, which would leave stale state behind
    // for containerd to trip over.
    std::fs::remove_dir_all(croot)?;
    std::fs::create_dir_all(croot)?;
    tracing::info!("migration complete, freed the copy on root");
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
        && !filesystem_is_healthy(host, &dev, policy.filesystem).await
    {
        tracing::warn!("the image store on {dev} is damaged — rebuilding it");
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

    // One-time migration off the root disk. Safe here because this unit runs
    // Before=k3s (enforced by run()'s k3s-stop bracket below), so nothing
    // holds these files open.
    if dir_has_any_entries(&croot) {
        tracing::info!("migrating the existing image store off the root disk");
        if let Err(e) = migrate_existing_store(host, root, &croot, &dev).await {
            tracing::warn!("{e}");
            return Ok(());
        }
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

pub async fn run<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    policy: &ContainerdStorePolicy,
) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("ceph not reachable — leaving containerd on the root disk for this boot");
        return Ok(());
    }
    if !image_exists(host, &policy.pool_name, node).await {
        tracing::info!(
            "no {}/{node} image yet — leaving containerd on the root disk for this boot",
            policy.pool_name
        );
        return Ok(());
    }

    let croot = containerd_root(root);
    let croot_s = croot.to_string_lossy().into_owned();
    let mut needs_rebuild = false;

    if is_mountpoint(host, &croot_s).await {
        if is_readable_dir(&croot) {
            tracing::info!("{croot_s} is already mounted and readable");
            return Ok(());
        }
        tracing::warn!("{croot_s} is mounted but cannot be read — rebuilding the image store");
        // Lazy as a fallback: containerd may already hold descriptors on a
        // filesystem that has shut down, and a plain umount would refuse.
        if !host
            .run_cmd("umount", &[&croot_s])
            .await
            .is_ok_and(|o| o.success)
        {
            let _ = host.run_cmd("umount", &["-l", &croot_s]).await;
        }
        needs_rebuild = true;
    }

    // From here on this may stop k3s, and every exit path has to put it back
    // — captured explicitly (not a `trap`) so the restart runs whether
    // `mount_the_store` returns Ok or Err.
    let was_active = host
        .systemctl(&["is-active", "--quiet", "k3s.service"])
        .await
        .map(|o| o.success)
        .unwrap_or(false);
    if was_active {
        tracing::info!("stopping k3s to move its image store onto Ceph");
        let _ = host.systemctl(&["stop", "k3s.service"]).await;
    }

    let result = mount_the_store(host, root, node, policy, needs_rebuild).await;

    if was_active {
        tracing::info!("starting k3s again");
        // --no-block, and not optional: k3s.service is After= this unit, so
        // a blocking start would deadlock against systemd's own ordering.
        let _ = host
            .systemctl(&["start", "--no-block", "k3s.service"])
            .await;
    }

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
    fn find_mapped_device_matches_pool_and_name() {
        let v = serde_json::json!([
            {"pool": "images", "name": "yolab-n1", "device": "/dev/rbd0"},
            {"pool": "images", "name": "yolab-n2", "device": "/dev/rbd1"},
        ]);
        assert_eq!(
            find_mapped_device(&v, "images", "yolab-n1").as_deref(),
            Some("/dev/rbd0")
        );
        assert_eq!(find_mapped_device(&v, "images", "yolab-n9"), None);
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

    #[tokio::test]
    async fn an_already_mounted_and_readable_store_is_left_alone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("mountpoint -q", "");
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

    #[tokio::test]
    async fn a_mounted_but_unreadable_store_is_unmounted_and_rebuilt() {
        // is_readable_dir cannot distinguish "empty" from "EIO" without a
        // real corrupted mount, so this drives the unmount path via a
        // containerd root that does not exist at all (is_readable_dir ->
        // false) while `mountpoint` still reports mounted — the same shape a
        // dead XFS mount produces: mounted, but nothing can be read from it.
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("rbd ls images", "yolab-n1\n")
            .ok("mountpoint -q", "") // success = IS a mountpoint
            .ok("umount", "")
            .fail("systemctl is-active", "not active")
            .fail("rbd map", "no route to host"); // stop short of the real mount dance
        let dir = tempfile::tempdir().unwrap();
        // containerd_root deliberately NOT created.

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert!(host.ran("umount"));
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
                    "findmnt" => ok_output("/dev/rbd0"),
                    _ => me.inner.run_cmd(bin, args).await,
                }
            }
        }
    }

    #[tokio::test]
    async fn migrates_an_existing_root_disk_store_onto_the_rbd() {
        let dir = tempfile::tempdir().unwrap();
        let host = SimulatedDisk {
            inner: FakeHost::new()
                .ok("ceph -s", "")
                .ok("rbd ls images", "yolab-n1\n")
                .fail("mountpoint -q", "not a mountpoint")
                .fail("systemctl is-active", "not active"),
            device_backing: dir.path().join("simulated-rbd0"),
        };
        let croot = containerd_root(dir.path());
        std::fs::create_dir_all(&croot).unwrap();
        std::fs::write(croot.join("existing-layer.tar"), b"layer bytes").unwrap();

        run(&host, dir.path(), "yolab-n1", &policy()).await.unwrap();

        assert_eq!(
            std::fs::read(croot.join("existing-layer.tar")).unwrap(),
            b"layer bytes",
            "the pre-existing layer must survive the migration onto the device"
        );
        assert!(
            !host.inner.ran("systemctl stop"),
            "k3s was never active, so it must not be stopped"
        );
    }

    #[tokio::test]
    async fn stops_and_restarts_k3s_around_an_active_migration() {
        let dir = tempfile::tempdir().unwrap();
        let host = SimulatedDisk {
            inner: FakeHost::new()
                .ok("ceph -s", "")
                .ok("rbd ls images", "yolab-n1\n")
                .fail("mountpoint -q", "not a mountpoint")
                .ok("systemctl is-active", ""),
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
