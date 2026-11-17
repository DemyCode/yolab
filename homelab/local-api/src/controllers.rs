use crate::runtime::{self, leader::Leadership};
use crate::storage::StorageEnv;
use crate::tools::Tools;

pub const NAMES: &[&str] = &[
    "backup-scheduler",
    "restores",
    "uninstalls",
    "outputs",
    "store-sync",
    "ssh-keys",
    "backup-lock-sweeper",
    "backup-check",
    "disks",
    "cephfs",
    "topology",
    "heal",
    "backup-credentials",
    "backup-key",
    "notifier",
    "shared-names",
    "mesh-paths",
    "mesh-discovery",
    "hardware-labels",
    "chart-sync",
    "osd-activate",
    "system-osd",
    "images-rbd",
    "images-grow",
    "ceph-dashboard",
    "mon-member",
    "csi-secrets",
    "ceph-keys",
    "ceph-join",
    "csi-recovery",
    "ceph-csi",
];

fn spawn_observed(leader: &Leadership, tools: &Tools) {
    for r in runtime::Requirement::ALL {
        runtime::resource::spawn(
            runtime::resource::Observed(r),
            leader.clone(),
            tools.clone(),
        );
    }
}

fn spawn<C: runtime::Controller>(controller: C, leader: &Leadership, tools: &Tools) {
    debug_assert!(
        NAMES.contains(&controller.name()),
        "controller {} is missing from controllers::NAMES",
        controller.name()
    );
    runtime::spawn(controller, leader.clone(), tools.clone());
}

pub fn spawn_all(leader: Leadership, state: &crate::AppState) {
    use crate::routers::{apps, backup, backups, restore};
    use crate::storage::controllers as storage;

    let tools = state.tools();
    spawn(backup::BackupSchedulerController, &leader, &tools);
    spawn(restore::RestoresController, &leader, &tools);
    backup::start_heartbeat(tools.kube.clone());
    spawn(apps::UninstallsController, &leader, &tools);
    spawn(crate::outputs::OutputsController, &leader, &tools);
    spawn(backups::LockSweeperController, &leader, &tools);
    spawn(
        crate::routers::backup_check::BackupCheckController,
        &leader,
        &tools,
    );

    spawn(crate::store::sync::StoreSyncController, &leader, &tools);
    spawn(crate::ssh_keys::SshKeysController, &leader, &tools);
    spawn(crate::disks_reconciler::DisksController, &leader, &tools);
    spawn(crate::cephfs::CephFsController, &leader, &tools);
    spawn(crate::topology::TopologyController, &leader, &tools);
    spawn(crate::mesh::MeshPathsController::new(), &leader, &tools);
    spawn(crate::mesh::MeshDiscoveryController::new(), &leader, &tools);
    spawn(
        crate::hardware::HardwareLabelsController::real(),
        &leader,
        &tools,
    );
    spawn(crate::charts::ChartSyncController, &leader, &tools);
    spawn(
        crate::heal::HealController {
            config: crate::config::Config::from_env(),
        },
        &leader,
        &tools,
    );
    spawn(
        crate::heal::credentials::BackupCredentialsController,
        &leader,
        &tools,
    );
    spawn(
        backups::BackupKeyController {
            config: crate::config::Config::from_env(),
        },
        &leader,
        &tools,
    );
    spawn(
        crate::notify::alerts::NotifierController {
            config: crate::config::Config::from_env(),
        },
        &leader,
        &tools,
    );
    spawn(
        crate::shared_names::SharedNamesController {
            config: crate::config::Config::from_env(),
        },
        &leader,
        &tools,
    );

    let env = StorageEnv::from_env();
    if env.is_configured() {
        spawn(
            crate::vdisk::VdisksController {
                config: crate::config::Config::from_env(),
                host: state.host.clone(),
                http: state.http.clone(),
            },
            &leader,
            &tools,
        );
        spawn(
            storage::OsdActivateController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(
            storage::SystemOsdController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(
            storage::ImagesRbdController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(
            storage::ImagesGrowController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(
            storage::DashboardController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(
            storage::MonMemberController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(storage::CephKeysController, &leader, &tools);
        spawn(
            storage::CephJoinController { env: env.clone() },
            &leader,
            &tools,
        );
        spawn(storage::CsiSecretsController, &leader, &tools);
        spawn(storage::CsiRecoveryController, &leader, &tools);
        spawn(crate::storage::ceph_csi::CephCsiController, &leader, &tools);
    }

    spawn_observed(&leader, &tools);

    for watch in runtime::watch::standard() {
        runtime::watch::spawn(watch);
    }

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
        "restores" => runtime::run_once(&restore::RestoresController).await,
        "uninstalls" => runtime::run_once(&apps::UninstallsController).await,
        "outputs" => runtime::run_once(&crate::outputs::OutputsController).await,
        "store-sync" => runtime::run_once(&crate::store::sync::StoreSyncController).await,
        "ssh-keys" => runtime::run_once(&crate::ssh_keys::SshKeysController).await,
        "backup-lock-sweeper" => runtime::run_once(&backups::LockSweeperController).await,
        "backup-check" => {
            runtime::run_once(&crate::routers::backup_check::BackupCheckController).await
        }
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
        "backup-key" => {
            runtime::run_once(&backups::BackupKeyController {
                config: crate::config::Config::from_env(),
            })
            .await
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
        "hardware-labels" => {
            runtime::run_once(&crate::hardware::HardwareLabelsController::real()).await
        }
        "chart-sync" => runtime::run_once(&crate::charts::ChartSyncController).await,
        "osd-activate" => runtime::run_once(&storage::OsdActivateController { env }).await,
        "system-osd" => runtime::run_once(&storage::SystemOsdController { env }).await,
        "images-rbd" => runtime::run_once(&storage::ImagesRbdController { env }).await,
        "images-grow" => runtime::run_once(&storage::ImagesGrowController { env }).await,
        "ceph-dashboard" => runtime::run_once(&storage::DashboardController { env }).await,
        "mon-member" => runtime::run_once(&storage::MonMemberController { env }).await,
        "csi-secrets" => runtime::run_once(&storage::CsiSecretsController).await,
        "ceph-keys" => runtime::run_once(&storage::CephKeysController).await,
        "ceph-join" => runtime::run_once(&storage::CephJoinController { env }).await,
        "csi-recovery" => runtime::run_once(&storage::CsiRecoveryController).await,
        "ceph-csi" => runtime::run_once(&crate::storage::ceph_csi::CephCsiController).await,
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
