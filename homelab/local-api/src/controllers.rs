//! Every controller local-api runs, in one place.
//!
//! `spawn_all` is what main.rs starts; `NAMES` is the same set for things that
//! cannot see a live registry — the runtime's watch test (a watch may only wake
//! a controller that exists) and `local-api run <name>`. `spawn` asserts the two
//! agree in debug builds, so a controller added to one and forgotten in the
//! other is caught the first time it starts.

use crate::runtime::{self, leader::Leadership};
use crate::storage::StorageEnv;

/// Every controller name, node- and cluster-scoped, including the storage
/// agent's. Kept in step with `spawn_all` by `spawn`'s debug assertion.
pub const NAMES: &[&str] = &[
    // Cluster coordination and operations.
    "backup-scheduler",
    "restore-watchdog",
    "uninstall-watchdog",
    "backup-lock-sweeper",
    // Node and cluster reconcilers.
    "disks",
    "cephfs",
    "topology",
    "storage-heal",
    "mesh-paths",
    "mesh-discovery",
    "chart-sync",
    // The storage agent's own jobs (formerly systemd timers).
    "osd-activate",
    "images-grow",
    "ceph-dashboard",
    "mon-member",
    "csi-secrets",
    "ceph-keys",
    "ceph-join",
    "csi-recovery",
];

fn spawn<C: runtime::Controller>(controller: C, leader: &Leadership) {
    debug_assert!(
        NAMES.contains(&controller.name()),
        "controller {} is missing from controllers::NAMES",
        controller.name()
    );
    runtime::spawn(controller, leader.clone());
}

/// Starts every controller and the event sources that wake them. Called once,
/// from main, after tracing is installed.
pub fn spawn_all(leader: Leadership) {
    use crate::routers::{apps, backup, backups, restore};
    use crate::storage::controllers as storage;

    // Scheduled backups and the per-app restore watchdog. Both drive long
    // operations, so they keep their records' claims fresh — the fix for a node2
    // watchdog treating node1's live restore as abandoned.
    spawn(backup::BackupSchedulerController, &leader);
    spawn(restore::RestoreWatchdogController, &leader);
    backup::start_heartbeat();
    restore::start_heartbeat();
    // Finishes uninstalls whose driving request died with a local-api restart.
    spawn(apps::UninstallWatchdogController, &leader);
    // Clears restic locks left behind when a lock-taking command was interrupted.
    spawn(backups::LockSweeperController, &leader);

    // OSD active-state (crush weight + in/out) is driven by the disk controller,
    // the single actuator for the DISK→ON/OFF config — no separate watcher.
    spawn(crate::disks_reconciler::DisksController, &leader);
    spawn(crate::cephfs::CephFsController, &leader);
    spawn(crate::topology::TopologyController, &leader);
    // Records lost disks, rebuilds the image store and mgr pool, and runs a
    // recovery from backup once the owner asks for one.
    spawn(crate::storage_heal::StorageHealController, &leader);
    spawn(crate::mesh::MeshPathsController::new(), &leader);
    spawn(crate::mesh::MeshDiscoveryController::new(), &leader);
    // Keeps the app catalog current without a nixos-rebuild.
    spawn(crate::charts::ChartSyncController, &leader);

    // The storage agent's own jobs, which used to be eleven systemd timers. Only
    // on a machine whose storage settings reached the process: a dev box must not
    // act on empty addresses.
    let env = StorageEnv::from_env();
    if env.is_configured() {
        spawn(storage::OsdActivateController { env: env.clone() }, &leader);
        spawn(storage::ImagesGrowController { env: env.clone() }, &leader);
        spawn(storage::DashboardController { env: env.clone() }, &leader);
        spawn(storage::MonMemberController { env: env.clone() }, &leader);
        spawn(storage::CephKeysController { env: env.clone() }, &leader);
        spawn(storage::CephJoinController { env: env.clone() }, &leader);
        spawn(storage::CsiSecretsController, &leader);
        spawn(storage::CsiRecoveryController, &leader);
    }

    // Event sources that wake controllers early. Events are hints, state is the
    // truth: a line from udev or a kubectl watch only means "look now".
    for watch in runtime::watch::standard() {
        runtime::watch::spawn(watch);
    }
}

/// Runs one controller's tick once, by name — `local-api run <name>`. So a
/// person over SSH can drive exactly what the daemon would, without waiting for
/// its interval.
pub async fn run_named(name: &str) -> anyhow::Result<runtime::Tick> {
    use crate::routers::{apps, backup, backups, restore};
    use crate::storage::controllers as storage;

    let env = StorageEnv::from_env();
    match name {
        "backup-scheduler" => runtime::run_once(&backup::BackupSchedulerController).await,
        "restore-watchdog" => runtime::run_once(&restore::RestoreWatchdogController).await,
        "uninstall-watchdog" => runtime::run_once(&apps::UninstallWatchdogController).await,
        "backup-lock-sweeper" => runtime::run_once(&backups::LockSweeperController).await,
        "disks" => runtime::run_once(&crate::disks_reconciler::DisksController).await,
        "cephfs" => runtime::run_once(&crate::cephfs::CephFsController).await,
        "topology" => runtime::run_once(&crate::topology::TopologyController).await,
        "storage-heal" => runtime::run_once(&crate::storage_heal::StorageHealController).await,
        "mesh-paths" => runtime::run_once(&crate::mesh::MeshPathsController::new()).await,
        "mesh-discovery" => runtime::run_once(&crate::mesh::MeshDiscoveryController::new()).await,
        "chart-sync" => runtime::run_once(&crate::charts::ChartSyncController).await,
        "osd-activate" => runtime::run_once(&storage::OsdActivateController { env }).await,
        "images-grow" => runtime::run_once(&storage::ImagesGrowController { env }).await,
        "ceph-dashboard" => runtime::run_once(&storage::DashboardController { env }).await,
        "mon-member" => runtime::run_once(&storage::MonMemberController { env }).await,
        "csi-secrets" => runtime::run_once(&storage::CsiSecretsController).await,
        "ceph-keys" => runtime::run_once(&storage::CephKeysController { env }).await,
        "ceph-join" => runtime::run_once(&storage::CephJoinController { env }).await,
        "csi-recovery" => runtime::run_once(&storage::CsiRecoveryController).await,
        _ => anyhow::bail!("unknown controller '{name}' (known: {})", NAMES.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_non_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for name in NAMES {
            assert!(!name.is_empty(), "a controller has no name");
            assert!(seen.insert(*name), "duplicate controller name {name}");
        }
    }

    #[tokio::test]
    async fn an_unknown_controller_name_is_refused() {
        let err = run_named("not-a-controller").await.unwrap_err();
        assert!(err.to_string().contains("unknown controller"), "{err}");
    }
}
