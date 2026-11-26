pub mod bootstrap;
pub(crate) mod capacity;
pub mod ceph_csi;
mod ceph_shared;
pub mod cephx;
pub mod controllers;
pub mod csi_secrets;
pub mod dashboard;
pub mod image_store;
pub mod images_grow;
pub mod images_rbd;
mod images_sizing;
pub mod keys;
pub mod mon_member;
pub mod movement;
pub mod noout;
pub mod osd;
pub mod overview;
pub mod release;
pub mod reset_wipe;
pub mod settings;
pub mod wait;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

#[cfg(not(test))]
pub(crate) use crate::live::machine_root as root;

#[cfg(test)]
pub(crate) fn root() -> &'static Path {
    thread_local! {
        static ROOT: &'static Path = Box::leak(
            std::path::PathBuf::from(crate::testkit::thread_dir("root")).into_boxed_path(),
        );
    }
    ROOT.with(|root| *root)
}

pub(crate) fn on_machine(path: &str) -> std::path::PathBuf {
    root().join(path.trim_start_matches('/'))
}

#[derive(Clone, Debug)]
pub struct StorageEnv {
    pub fsid: String,
    pub mon_addr: String,
    pub join_seed_addr: String,
    pub config_path: String,
    pub api_port: u16,
    pub images_pool: String,
    pub images_share: f64,
    pub images_min_gb: u64,
    pub images_fs: image_store::Filesystem,
    pub dashboard_port: u16,
    pub dashboard_prefix: String,
    pub dashboard_password_file: String,
}

impl StorageEnv {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let s = |name: &str| get(name).unwrap_or_default();
        let or = |name: &str, default: &str| {
            get(name)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            fsid: s("YOLAB_CEPH_FSID"),
            mon_addr: s("YOLAB_CEPH_MON_ADDR"),
            join_seed_addr: s("YOLAB_CEPH_JOIN_SEED_ADDR"),
            config_path: s("YOLAB_CONFIG"),
            api_port: s("YOLAB_PORT").parse().unwrap_or(3001),
            images_pool: or("YOLAB_CEPH_IMAGES_POOL", "images"),
            images_share: s("YOLAB_CEPH_IMAGES_SHARE").parse().unwrap_or(0.25),
            images_min_gb: s("YOLAB_CEPH_IMAGES_MIN_GB").parse().unwrap_or(40),
            images_fs: image_store::Filesystem::parse(&s("YOLAB_CEPH_IMAGES_FS")),
            dashboard_port: s("YOLAB_CEPH_DASHBOARD_PORT").parse().unwrap_or(7000),
            dashboard_prefix: or("YOLAB_CEPH_DASHBOARD_PREFIX", "/ceph-dashboard"),
            dashboard_password_file: or(
                "YOLAB_CEPH_DASHBOARD_PASSWORD_FILE",
                "/var/lib/ceph/dashboard-password",
            ),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.fsid.is_empty() && !self.mon_addr.is_empty()
    }

    pub fn bootstrap_args(&self) -> bootstrap::BootstrapArgs {
        bootstrap::BootstrapArgs {
            fsid: self.fsid.clone(),
            mon_addr: self.mon_addr.clone(),
            join_seed_addr: self.join_seed_addr.clone(),
            config_path: self.config_path.clone(),
            api_port: self.api_port,
        }
    }

    pub fn mon_member_args(&self) -> mon_member::MonMemberArgs {
        mon_member::MonMemberArgs {
            mon_addr: self.mon_addr.clone(),
        }
    }

    pub fn images_rbd_policy(&self) -> images_rbd::ImagesRbdPolicy {
        images_rbd::ImagesRbdPolicy {
            pool_name: self.images_pool.clone(),
            share_of_pool: self.images_share,
            min_size_gb: self.images_min_gb,
        }
    }

    pub fn image_store_policy(&self) -> image_store::ImageStorePolicy {
        image_store::ImageStorePolicy {
            pool_name: self.images_pool.clone(),
            filesystem: self.images_fs,
        }
    }

    pub fn grow_policy(&self) -> images_grow::GrowPolicy {
        images_grow::GrowPolicy {
            pool_name: self.images_pool.clone(),
            share_of_pool: self.images_share,
            min_size_gb: self.images_min_gb,
            filesystem: self.images_fs,
        }
    }

    pub fn dashboard_policy(&self) -> dashboard::DashboardPolicy {
        dashboard::DashboardPolicy {
            port: self.dashboard_port,
            url_prefix: self.dashboard_prefix.clone(),
            password_file: self.dashboard_password_file.clone(),
            mon_addr: self.mon_addr.clone(),
        }
    }
}

async fn image_store_up<H: crate::host::Host>(
    host: &H,
    env: &StorageEnv,
    node: &str,
) -> Result<()> {
    if !env.is_configured() {
        anyhow::bail!(
            "this machine has no Ceph settings, so it has no image store to start k3s on"
        );
    }
    let policy = env.image_store_policy();
    match image_store::up(host, root(), node, &policy, image_store::RETRY).await {
        image_store::Up::Mounted => tracing::info!("image store: mounted, starting k3s"),
        image_store::Up::LeftOnRootDisk(why) => tracing::warn!("image store: {why}"),
    }
    let started = host
        .systemctl(&["start", "--no-block", controllers::K3S_UNIT])
        .await?;
    if !started.success {
        anyhow::bail!(
            "could not start {}: {}",
            controllers::K3S_UNIT,
            started.stderr.trim()
        );
    }
    Ok(())
}

pub fn lock_name(job: &str) -> String {
    format!("storage-{job}")
}

const BOOT_LOCK_WAIT: Duration = Duration::from_secs(120);

enum Job {
    NooutClear,
    NooutSet,
    Bootstrap,
    ResetWipe,
    Release,
    VdiskDetachAll,
    ImageStoreUp,
}

impl Job {
    fn named(sub: &str) -> Option<Self> {
        match sub {
            "noout-clear" => Some(Job::NooutClear),
            "noout-set" => Some(Job::NooutSet),
            "bootstrap" => Some(Job::Bootstrap),
            "reset-wipe" => Some(Job::ResetWipe),
            "release" => Some(Job::Release),
            "vdisk-detach-all" => Some(Job::VdiskDetachAll),
            "image-store-up" => Some(Job::ImageStoreUp),
            _ => None,
        }
    }

    fn takes_the_lock(&self) -> bool {
        !matches!(self, Job::NooutSet | Job::Release | Job::VdiskDetachAll)
    }
}

pub async fn run(args: &[String]) -> i32 {
    run_on(
        &crate::tools::Tools::real().host,
        args,
        &StorageEnv::from_env(),
        &crate::system::hostname(),
        Path::new(crate::runtime::lock::LOCK_DIR),
        &crate::config::machine_dir().join("config.toml"),
    )
    .await
}

async fn run_on<H: crate::host::Host>(
    host: &H,
    args: &[String],
    env: &StorageEnv,
    node: &str,
    lock_dir: &Path,
    config: &Path,
) -> i32 {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("storage: missing subcommand");
        return 2;
    };
    let Some(job) = Job::named(sub) else {
        eprintln!("storage: unknown subcommand '{sub}'");
        return 2;
    };

    let _guard = if job.takes_the_lock() {
        match crate::runtime::lock::acquire_in(lock_dir, &lock_name(sub), BOOT_LOCK_WAIT).await {
            Ok(Some(g)) => Some(g),
            Ok(None) => {
                eprintln!(
                    "storage {sub}: local-api has been running the same job for over {}s — giving up",
                    BOOT_LOCK_WAIT.as_secs()
                );
                return 1;
            }
            Err(e) => {
                eprintln!("storage {sub}: could not take the job lock: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let result: Result<()> = match job {
        Job::NooutClear => noout::clear(host, root()).await,
        Job::NooutSet => noout::set(host, root()).await,
        Job::Release => release::run(host, root(), controllers::K3S_UNIT, release::WAITS).await,
        Job::VdiskDetachAll => crate::vdisk::detach_all(host, root()).await,
        Job::ImageStoreUp => image_store_up(host, env, node).await,
        Job::Bootstrap => bootstrap::run(host, root(), node, &env.bootstrap_args()).await,
        Job::ResetWipe => reset_wipe::run(host, root(), config).await,
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("storage {sub}: {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn ceph_settings() -> StorageEnv {
        StorageEnv::from_lookup(|name| match name {
            "YOLAB_CEPH_FSID" => Some("f".into()),
            "YOLAB_CEPH_MON_ADDR" => Some("fd00:cafe::1".into()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn the_image_store_starts_k3s_once_it_is_mounted() {
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .ok("systemctl start --no-block k3s.service", "");
        image_store_up(&host, &ceph_settings(), "yolab-n1")
            .await
            .unwrap();
        let calls = host.calls();
        let mounted = calls.iter().position(|c| c.starts_with("findmnt")).unwrap();
        let started = calls
            .iter()
            .position(|c| c == "systemctl start --no-block k3s.service")
            .expect("k3s was never started");
        assert!(mounted < started, "{calls:?}");
    }

    #[tokio::test]
    async fn a_k3s_that_cannot_be_started_fails_the_unit_so_systemd_tries_again() {
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .fail("systemctl start --no-block k3s.service", "unit masked");
        let err = image_store_up(&host, &ceph_settings(), "yolab-n1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unit masked"), "{err}");
    }

    async fn within_seconds<F: std::future::Future>(work: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .expect("waited on an image store a machine without Ceph settings does not have")
    }

    #[tokio::test]
    async fn a_machine_without_ceph_settings_fails_instead_of_starting_k3s_on_the_root_disk() {
        let host = FakeHost::new().ok("systemctl", "");
        let err = within_seconds(image_store_up(
            &host,
            &StorageEnv::from_lookup(|_| None),
            "yolab-n1",
        ))
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no Ceph settings"), "{err}");
        assert!(!host.ran("systemctl start"), "{:?}", host.calls());
    }

    #[test]
    fn defaults_apply_only_to_settings_that_have_one() {
        let env = StorageEnv::from_lookup(|_| None);
        assert_eq!(env.images_pool, "images");
        assert_eq!(env.images_min_gb, 40);
        assert_eq!(env.dashboard_port, 7000);
        assert_eq!(env.dashboard_prefix, "/ceph-dashboard");
        assert_eq!(env.api_port, 3001);
        assert_eq!(env.bootstrap_args().api_port, 3001);
        assert!(!env.is_configured());
    }

    #[test]
    fn ceph_needs_both_a_cluster_id_and_a_monitor_address() {
        let only = |key: &'static str| {
            StorageEnv::from_lookup(move |name| (name == key).then(|| "x".to_string()))
        };
        assert!(!only("YOLAB_CEPH_FSID").is_configured());
        assert!(!only("YOLAB_CEPH_MON_ADDR").is_configured());
        assert!(ceph_settings().is_configured());
    }

    #[test]
    fn a_configured_environment_is_read_through() {
        let vars = std::collections::HashMap::from([
            ("YOLAB_CEPH_FSID", "abc"),
            ("YOLAB_CEPH_MON_ADDR", "fd00:cafe::5"),
            ("YOLAB_CEPH_IMAGES_POOL", "imgs"),
            ("YOLAB_CEPH_IMAGES_FS", "ext4"),
        ]);
        let env = StorageEnv::from_lookup(|k| vars.get(k).map(|v| v.to_string()));
        assert!(env.is_configured());
        assert_eq!(env.images_pool, "imgs");
        assert_eq!(env.images_fs, image_store::Filesystem::Ext4);
        assert_eq!(env.images_rbd_policy().pool_name, "imgs");
        assert_eq!(env.dashboard_policy().mon_addr, "fd00:cafe::5");
    }

    #[test]
    fn a_subcommand_and_its_controller_share_one_lock_name() {
        assert_eq!(lock_name("images-rbd"), "storage-images-rbd");
    }

    #[tokio::test]
    async fn a_missing_or_unknown_subcommand_is_a_usage_error_that_touches_nothing() {
        assert_eq!(run(&[]).await, 2);
        assert_eq!(run(&["images-recover".to_string()]).await, 2);
    }

    #[test]
    fn every_policy_is_built_from_the_one_environment() {
        let vars = std::collections::HashMap::from([
            ("YOLAB_CEPH_FSID", "abc"),
            ("YOLAB_CEPH_MON_ADDR", "fd00::1"),
            ("YOLAB_CEPH_JOIN_SEED_ADDR", "fd00::2"),
            ("YOLAB_CONFIG", "/etc/yolab.toml"),
            ("YOLAB_CEPH_IMAGES_SHARE", "0.5"),
            ("YOLAB_CEPH_IMAGES_MIN_GB", "80"),
            ("YOLAB_CEPH_DASHBOARD_PORT", "8443"),
        ]);
        let env = StorageEnv::from_lookup(|k| vars.get(k).map(|v| v.to_string()));
        let b = env.bootstrap_args();
        assert_eq!(
            (
                b.fsid.as_str(),
                b.join_seed_addr.as_str(),
                b.config_path.as_str()
            ),
            ("abc", "fd00::2", "/etc/yolab.toml")
        );
        assert_eq!(env.mon_member_args().mon_addr, "fd00::1");
        let rbd = env.images_rbd_policy();
        assert_eq!((rbd.share_of_pool, rbd.min_size_gb), (0.5, 80));
        let grow = env.grow_policy();
        assert_eq!((grow.share_of_pool, grow.min_size_gb), (0.5, 80));
        assert_eq!(env.image_store_policy().pool_name, "images");
        assert_eq!(env.dashboard_policy().port, 8443);
    }

    #[test]
    fn a_join_asks_the_seed_on_the_port_this_api_is_served_on() {
        let env = StorageEnv::from_lookup(|k| (k == "YOLAB_PORT").then(|| "3100".to_string()));
        assert_eq!(env.bootstrap_args().api_port, 3100);
    }

    #[test]
    fn unparseable_numbers_fall_back_to_their_defaults() {
        let env = StorageEnv::from_lookup(|k| match k {
            "YOLAB_CEPH_IMAGES_SHARE"
            | "YOLAB_CEPH_IMAGES_MIN_GB"
            | "YOLAB_CEPH_DASHBOARD_PORT" => Some("lots".to_string()),
            _ => None,
        });
        assert_eq!(env.images_share, 0.25);
        assert_eq!(env.images_min_gb, 40);
        assert_eq!(env.dashboard_port, 7000);
    }

    #[tokio::test]
    async fn a_containerd_already_on_the_root_disk_still_lets_k3s_start() {
        let proc = root().join("proc/4242");
        std::fs::create_dir_all(&proc).unwrap();
        std::fs::write(proc.join("comm"), "containerd\n").unwrap();
        let host = FakeHost::new()
            .fail("findmnt -rno TARGET --mountpoint", "")
            .ok("systemctl start --no-block k3s.service", "");
        image_store_up(&host, &ceph_settings(), "yolab-n1")
            .await
            .unwrap();
        assert!(host.ran("systemctl start --no-block k3s.service"));
        assert!(!host.ran("rbd"), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_systemctl_that_cannot_be_started_is_an_error() {
        let host = FakeHost::new()
            .ok("findmnt -rno TARGET --mountpoint", "")
            .unstartable("systemctl start");
        assert!(image_store_up(&host, &ceph_settings(), "yolab-n1")
            .await
            .is_err());
    }

    async fn run_job(host: &FakeHost, sub: &str, env: &StorageEnv, lock_dir: &Path) -> i32 {
        run_on(
            host,
            &[sub.to_string()],
            env,
            "yolab-n1",
            lock_dir,
            &lock_dir.join("missing-config.toml"),
        )
        .await
    }

    fn lock_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn noout_clear_runs_under_its_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("ceph -s", "");
        let env = ceph_settings();
        assert_eq!(run_job(&host, "noout-clear", &env, dir.path()).await, 0);
        assert_eq!(lock_files(dir.path()), ["storage-noout-clear.lock"]);
    }

    #[tokio::test]
    async fn noout_set_runs_without_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new();
        let env = ceph_settings();
        assert_eq!(run_job(&host, "noout-set", &env, dir.path()).await, 0);
        assert!(lock_files(dir.path()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_while_k3s_keeps_running_fails_without_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("systemctl is-active --quiet k3s.service", "");
        let env = ceph_settings();
        assert_eq!(run_job(&host, "release", &env, dir.path()).await, 1);
        assert!(lock_files(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn detaching_no_virtual_disks_succeeds_without_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new();
        let env = ceph_settings();
        assert_eq!(
            run_job(&host, "vdisk-detach-all", &env, dir.path()).await,
            0
        );
        assert!(lock_files(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn an_image_store_without_ceph_settings_fails_the_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new();
        let env = StorageEnv::from_lookup(|_| None);
        assert_eq!(
            within_seconds(run_job(&host, "image-store-up", &env, dir.path())).await,
            1
        );
        assert_eq!(lock_files(dir.path()), ["storage-image-store-up.lock"]);
    }

    #[tokio::test]
    async fn a_join_without_an_account_token_fails_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir
            .path()
            .join("no-config.toml")
            .to_string_lossy()
            .into_owned();
        let env = StorageEnv::from_lookup(|name| match name {
            "YOLAB_CEPH_FSID" => Some("f".into()),
            "YOLAB_CEPH_MON_ADDR" => Some("fd00:cafe::1".into()),
            "YOLAB_CEPH_JOIN_SEED_ADDR" => Some("fd00:cafe::2".into()),
            "YOLAB_CONFIG" => Some(config.clone()),
            _ => None,
        });
        let host = FakeHost::new();
        assert_eq!(run_job(&host, "bootstrap", &env, dir.path()).await, 1);
        assert!(host.calls().is_empty(), "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_reset_wipe_without_a_machine_config_fails() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new();
        let env = ceph_settings();
        assert_eq!(run_job(&host, "reset-wipe", &env, dir.path()).await, 1);
        assert!(host.calls().is_empty(), "{:?}", host.calls());
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_whose_lock_stays_held_gives_up_without_running() {
        let dir = tempfile::tempdir().unwrap();
        let held = crate::runtime::lock::try_acquire_in(dir.path(), "storage-noout-clear")
            .unwrap()
            .unwrap();
        let host = FakeHost::new().ok("ceph -s", "");
        let env = ceph_settings();
        assert_eq!(run_job(&host, "noout-clear", &env, dir.path()).await, 1);
        assert!(host.calls().is_empty(), "{:?}", host.calls());
        drop(held);
    }

    #[tokio::test]
    async fn a_lock_folder_that_cannot_be_made_fails_without_running() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        let host = FakeHost::new().ok("ceph -s", "");
        let env = ceph_settings();
        assert_eq!(
            run_job(&host, "noout-clear", &env, &file.join("locks")).await,
            1
        );
        assert!(host.calls().is_empty(), "{:?}", host.calls());
    }
}
