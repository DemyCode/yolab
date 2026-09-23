use crate::runtime::{self, leader::Leadership};
use crate::storage::StorageEnv;

pub const NAMES: &[&str] = &[
    "backup-scheduler",
    "restore-watchdog",
    "uninstall-watchdog",
    "backup-lock-sweeper",
    "disks",
    "cephfs",
    "topology",
    "heal",
    "backup-credentials",
    "notifier",
    "shared-names",
    "mesh-paths",
    "mesh-discovery",
    "chart-sync",
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

pub fn spawn_all(leader: Leadership) {
    use crate::routers::{apps, backup, backups, restore};
    use crate::storage::controllers as storage;

    spawn(backup::BackupSchedulerController, &leader);
    spawn(restore::RestoreWatchdogController, &leader);
    backup::start_heartbeat();
    restore::start_heartbeat();
    spawn(apps::UninstallWatchdogController, &leader);
    spawn(backups::LockSweeperController, &leader);

    spawn(crate::disks_reconciler::DisksController, &leader);
    spawn(crate::cephfs::CephFsController, &leader);
    spawn(crate::topology::TopologyController, &leader);
    spawn(crate::mesh::MeshPathsController::new(), &leader);
    spawn(crate::mesh::MeshDiscoveryController::new(), &leader);
    spawn(crate::charts::ChartSyncController, &leader);
    spawn(
        crate::heal::HealController {
            config: crate::config::Config::from_env(),
        },
        &leader,
    );
    spawn(
        crate::heal::credentials::BackupCredentialsController,
        &leader,
    );
    spawn(
        crate::notify::alerts::NotifierController {
            config: crate::config::Config::from_env(),
        },
        &leader,
    );
    spawn(
        crate::shared_names::SharedNamesController {
            config: crate::config::Config::from_env(),
        },
        &leader,
    );

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

    for watch in runtime::watch::standard() {
        runtime::watch::spawn(watch);
    }

    // Every resource has registered its edges by now. A dependency naming
    // something that does not exist, or a cycle, means part of the graph can
    // never converge — and unlike a systemd ordering bug it would otherwise be
    // silent, because a resource waiting on a name nobody provides simply waits.
    for problem in runtime::resource::problems() {
        tracing::error!("resource graph: {problem}");
    }
}

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
        "heal" => {
            runtime::run_once(&crate::heal::HealController {
                config: crate::config::Config::from_env(),
            })
            .await
        }
        "backup-credentials" => {
            runtime::run_once(&crate::heal::credentials::BackupCredentialsController).await
        }
        "notifier" => {
            runtime::run_once(&crate::notify::alerts::NotifierController {
                config: crate::config::Config::from_env(),
            })
            .await
        }
        "shared-names" => {
            runtime::run_once(&crate::shared_names::SharedNamesController {
                config: crate::config::Config::from_env(),
            })
            .await
        }
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
