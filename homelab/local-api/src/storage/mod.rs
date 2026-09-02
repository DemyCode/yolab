//! The early-boot storage agent.
//!
//! `local-api serve` is the API + reconciler, ordered after k3s. These
//! subcommands are the other half: the work that must run *before* the Ceph
//! daemons and k3s, which used to be bash embedded in Nix strings. systemd
//! keeps the ordering/timers; only the script body moved here.
pub mod bootstrap;
mod ceph_shared;
pub mod containerd_store;
pub mod csi_secrets;
pub mod dashboard;
pub mod images_grow;
pub mod images_rbd;
mod images_sizing;
pub mod keys;
pub mod mon_member;
pub mod noout;
pub mod osd;

use std::path::Path;

use anyhow::Result;

use crate::host::RealHost;

/// The real machine's root, for subcommands that build absolute paths
/// (`/var/lib/ceph/...`) so their file-writing logic can be exercised in
/// tests against a tempdir instead.
fn root() -> &'static Path {
    Path::new("/")
}

pub async fn run(args: &[String]) -> i32 {
    let host = RealHost;
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("storage: missing subcommand");
        return 2;
    };

    // The Nix side passes cluster identity through the environment, the same
    // convention the main `yolab-local-api` service uses for YOLAB_CONFIG —
    // never as CLI args, because `bootstrap`'s join-seed-addr is empty on the
    // machine that creates the cluster, and an empty positional through a
    // systemd single-string ExecStart is not something to depend on.
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let node = crate::system::hostname();

    // Shared by images-rbd, containerd-store and images-grow — the pool name,
    // share-of-pool and floor are the yolab.ceph.imagesStore.* Nix options;
    // filesystem is the same option's enum, parsed with a safe default.
    let images_pool = || {
        let v = env("YOLAB_CEPH_IMAGES_POOL");
        if v.is_empty() {
            "images".to_string()
        } else {
            v
        }
    };
    let images_share = || {
        env("YOLAB_CEPH_IMAGES_SHARE")
            .parse::<f64>()
            .unwrap_or(0.25)
    };
    let images_min_gb = || env("YOLAB_CEPH_IMAGES_MIN_GB").parse::<u64>().unwrap_or(40);
    let images_fs = || containerd_store::Filesystem::parse(&env("YOLAB_CEPH_IMAGES_FS"));

    let result: Result<()> = match sub {
        "mgr-key" => keys::mint(&host, "mgr").await,
        "mds-key" => keys::mint(&host, "mds").await,
        "osd-activate" => osd::run(&host).await,
        "noout-clear" => noout::clear(&host, root()).await,
        "noout-set" => noout::set(&host, root()).await,
        "bootstrap" => {
            let args = bootstrap::BootstrapArgs {
                fsid: env("YOLAB_CEPH_FSID"),
                mon_addr: env("YOLAB_CEPH_MON_ADDR"),
                join_seed_addr: env("YOLAB_CEPH_JOIN_SEED_ADDR"),
                config_path: env("YOLAB_CONFIG"),
            };
            bootstrap::run(&host, root(), &node, &args).await
        }
        "mon-member" => {
            let args = mon_member::MonMemberArgs {
                mon_addr: env("YOLAB_CEPH_MON_ADDR"),
            };
            mon_member::run(&host, root(), &node, &args).await
        }
        "images-rbd" => {
            let policy = images_rbd::ImagesRbdPolicy {
                pool_name: images_pool(),
                share_of_pool: images_share(),
                min_size_gb: images_min_gb(),
            };
            images_rbd::run(&host, &node, &policy).await
        }
        "containerd-store" => {
            let policy = containerd_store::ContainerdStorePolicy {
                pool_name: images_pool(),
                filesystem: images_fs(),
            };
            containerd_store::run(&host, root(), &node, &policy).await
        }
        "images-grow" => {
            let policy = images_grow::GrowPolicy {
                pool_name: images_pool(),
                share_of_pool: images_share(),
                min_size_gb: images_min_gb(),
                filesystem: images_fs(),
            };
            images_grow::run(&host, root(), &node, &policy).await
        }
        "dashboard" => {
            let policy = dashboard::DashboardPolicy {
                port: env("YOLAB_CEPH_DASHBOARD_PORT").parse().unwrap_or(7000),
                url_prefix: {
                    let v = env("YOLAB_CEPH_DASHBOARD_PREFIX");
                    if v.is_empty() {
                        "/ceph-dashboard".to_string()
                    } else {
                        v
                    }
                },
                password_file: {
                    let v = env("YOLAB_CEPH_DASHBOARD_PASSWORD_FILE");
                    if v.is_empty() {
                        "/var/lib/ceph/dashboard-password".to_string()
                    } else {
                        v
                    }
                },
                mon_addr: env("YOLAB_CEPH_MON_ADDR"),
            };
            dashboard::run(&host, &node, &policy).await
        }
        "csi-secrets" => csi_secrets::run(&host).await,
        _ => {
            eprintln!("storage: unknown subcommand '{sub}'");
            return 2;
        }
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("storage {sub}: {e:#}");
            1
        }
    }
}
