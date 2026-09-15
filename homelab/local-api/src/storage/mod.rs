//! The storage agent: the work that keeps host Ceph and the image store on it
//! working.
//!
//! Each job here runs in up to two ways, and they never overlap:
//!
//!   - as a `local-api storage <name>` subcommand from a systemd oneshot, when
//!     it must happen at a fixed point in boot — before a Ceph daemon
//!     (`bootstrap`, `mgr-key`, `mds-key`), on the straight line to k3s
//!     (`system-osd` → `images-rbd` → `containerd-store`, each WAITING for its
//!     preconditions rather than falling back; see `wait`), or at shutdown
//!     (`noout-set`);
//!   - as a controller in the long-running local-api (`controllers.rs`), for
//!     everything that must keep being true afterwards.
//!
//! This replaced eleven systemd timers that re-ran the oneshots every few
//! minutes. Timers were the wrong tool for "keep this true": a
//! `RemainAfterExit` unit silently stops its timer forever (a 32-hour outage),
//! a run that outlives an `OnUnitActiveSec` interval re-fires instantly, and
//! nothing showed whether a timer's last run had worked.
//!
//! EVERY JOB TAKES THE SAME LOCK IN BOTH MODES (`runtime::lock`, an flock in
//! /run/yolab/locks), so the boot unit and the controller can never run the
//! same job at once. The boot unit waits for the lock; the controller skips a
//! tick if the boot unit holds it.
pub mod bootstrap;
mod ceph_shared;
pub mod containerd_store;
pub mod controllers;
pub mod csi_secrets;
pub mod dashboard;
pub mod images_grow;
pub mod images_rbd;
mod images_sizing;
pub mod keys;
pub mod mon_member;
pub mod noout;
pub mod osd;
pub mod reset_wipe;
pub mod settings;
pub mod wait;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::host::RealHost;

/// The real machine's root, for subcommands that build absolute paths
/// (`/var/lib/ceph/...`) so their file-writing logic can be exercised in
/// tests against a tempdir instead.
fn root() -> &'static Path {
    Path::new("/")
}

/// Everything the storage jobs are configured with. The Nix side passes it
/// through the environment — to the boot oneshots AND to yolab-local-api, from
/// the same Nix attrset, so the two modes cannot be configured differently.
/// Never CLI args: `join_seed_addr` is empty on the machine that creates the
/// cluster, and an empty positional through a systemd ExecStart is not
/// something to depend on.
#[derive(Clone, Debug)]
pub struct StorageEnv {
    pub fsid: String,
    pub mon_addr: String,
    pub join_seed_addr: String,
    pub config_path: String,
    pub images_pool: String,
    pub images_share: f64,
    pub images_min_gb: u64,
    pub images_fs: containerd_store::Filesystem,
    pub dashboard_port: u16,
    pub dashboard_prefix: String,
    pub dashboard_password_file: String,
    /// Whether this node runs an MDS (`yolab.ceph.filesystem.enable`).
    pub mds: bool,
}

impl StorageEnv {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
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
            images_pool: or("YOLAB_CEPH_IMAGES_POOL", "images"),
            images_share: s("YOLAB_CEPH_IMAGES_SHARE").parse().unwrap_or(0.25),
            images_min_gb: s("YOLAB_CEPH_IMAGES_MIN_GB").parse().unwrap_or(40),
            images_fs: containerd_store::Filesystem::parse(&s("YOLAB_CEPH_IMAGES_FS")),
            dashboard_port: s("YOLAB_CEPH_DASHBOARD_PORT").parse().unwrap_or(7000),
            dashboard_prefix: or("YOLAB_CEPH_DASHBOARD_PREFIX", "/ceph-dashboard"),
            dashboard_password_file: or(
                "YOLAB_CEPH_DASHBOARD_PASSWORD_FILE",
                "/var/lib/ceph/dashboard-password",
            ),
            mds: s("YOLAB_CEPH_MDS") == "1",
        }
    }

    /// Whether the storage settings reached this process at all. A local-api
    /// started without them (a dev box, WSL) must not run storage controllers
    /// against empty addresses.
    pub fn is_configured(&self) -> bool {
        !self.fsid.is_empty() && !self.mon_addr.is_empty()
    }

    pub fn bootstrap_args(&self) -> bootstrap::BootstrapArgs {
        bootstrap::BootstrapArgs {
            fsid: self.fsid.clone(),
            mon_addr: self.mon_addr.clone(),
            join_seed_addr: self.join_seed_addr.clone(),
            config_path: self.config_path.clone(),
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

    pub fn containerd_store_policy(&self) -> containerd_store::ContainerdStorePolicy {
        containerd_store::ContainerdStorePolicy {
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

/// The lock name for a job, shared by its subcommand and its controller.
pub fn lock_name(job: &str) -> String {
    format!("storage-{job}")
}

/// How long a boot unit waits for the controller to finish the same job. Below
/// every unit's own TimeoutStartSec, so a stuck holder produces a clear error
/// here rather than systemd killing the unit silently.
const BOOT_LOCK_WAIT: Duration = Duration::from_secs(120);

pub async fn run(args: &[String]) -> i32 {
    let host = RealHost;
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("storage: missing subcommand");
        return 2;
    };
    let env = StorageEnv::from_env();
    let node = crate::system::hostname();

    let known = [
        "mgr-key",
        "mds-key",
        "osd-activate",
        "system-osd",
        "noout-clear",
        "noout-set",
        "bootstrap",
        "mon-member",
        "images-rbd",
        "containerd-store",
        "images-grow",
        "dashboard",
        "csi-secrets",
        "reset-wipe",
    ];
    if !known.contains(&sub) {
        eprintln!("storage: unknown subcommand '{sub}'");
        return 2;
    }

    // noout-set runs at shutdown and must never wait on anything.
    let _guard = if sub == "noout-set" {
        None
    } else {
        match crate::runtime::lock::acquire(&lock_name(sub), BOOT_LOCK_WAIT).await {
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
    };

    let result: Result<()> = match sub {
        "mgr-key" => keys::mint(&host, "mgr").await,
        "mds-key" => keys::mint(&host, "mds").await,
        "osd-activate" => osd::run(&host).await,
        "noout-clear" => noout::clear(&host, root()).await,
        "noout-set" => noout::set(&host, root()).await,
        "bootstrap" => bootstrap::run(&host, root(), &node, &env.bootstrap_args()).await,
        "mon-member" => mon_member::run(&host, root(), &node, &env.mon_member_args()).await,
        "system-osd" => {
            wait::until_ready("system-osd", || {
                crate::disks_reconciler::system_osd_attempt(&host)
            })
            .await;
            Ok(())
        }
        "images-rbd" => {
            let policy = env.images_rbd_policy();
            wait::until_ready("images-rbd", || images_rbd::attempt(&host, &node, &policy)).await;
            Ok(())
        }
        "containerd-store" => {
            let policy = env.containerd_store_policy();
            wait::until_ready("containerd-store", || {
                containerd_store::attempt(&host, root(), &node, &policy)
            })
            .await;
            Ok(())
        }
        "images-grow" => images_grow::run(&host, root(), &node, &env.grow_policy()).await,
        "dashboard" => dashboard::run(&host, &node, &env.dashboard_policy()).await,
        "csi-secrets" => csi_secrets::run(&host).await,
        "reset-wipe" => {
            let config = crate::config::machine_dir().join("config.toml");
            reset_wipe::run(&host, root(), &config).await
        }
        _ => unreachable!("checked against `known` above"),
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

    #[test]
    fn defaults_apply_only_to_settings_that_have_one() {
        let env = StorageEnv::from_lookup(|_| None);
        assert_eq!(env.images_pool, "images");
        assert_eq!(env.images_min_gb, 40);
        assert_eq!(env.dashboard_port, 7000);
        assert_eq!(env.dashboard_prefix, "/ceph-dashboard");
        assert!(!env.mds);
        // Cluster identity has no default: an unconfigured process says so.
        assert!(!env.is_configured());
    }

    #[test]
    fn a_configured_environment_is_read_through() {
        let vars = std::collections::HashMap::from([
            ("YOLAB_CEPH_FSID", "abc"),
            ("YOLAB_CEPH_MON_ADDR", "fd00:cafe::5"),
            ("YOLAB_CEPH_IMAGES_POOL", "imgs"),
            ("YOLAB_CEPH_IMAGES_FS", "ext4"),
            ("YOLAB_CEPH_MDS", "1"),
        ]);
        let env = StorageEnv::from_lookup(|k| vars.get(k).map(|v| v.to_string()));
        assert!(env.is_configured());
        assert_eq!(env.images_pool, "imgs");
        assert_eq!(env.images_fs, containerd_store::Filesystem::Ext4);
        assert!(env.mds);
        assert_eq!(env.images_rbd_policy().pool_name, "imgs");
        assert_eq!(env.dashboard_policy().mon_addr, "fd00:cafe::5");
    }

    #[test]
    fn a_subcommand_and_its_controller_share_one_lock_name() {
        assert_eq!(lock_name("containerd-store"), "storage-containerd-store");
    }

    #[tokio::test]
    async fn a_missing_or_unknown_subcommand_is_a_usage_error_that_touches_nothing() {
        // Exit 2 is returned before any lock is taken or any command is run.
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
        assert_eq!(env.containerd_store_policy().pool_name, "images");
        assert_eq!(env.dashboard_policy().port, 8443);
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
}
