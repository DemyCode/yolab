use std::future::Future;
use std::pin::Pin;

use crate::config::Config;
use crate::runtime::{self, leader::Leadership};
use crate::storage::StorageEnv;
use crate::tools::Tools;

pub struct Setup {
    pub env: StorageEnv,
    pub tools: Tools,
}

impl Setup {
    fn from_state(state: &crate::AppState) -> Self {
        Setup {
            env: StorageEnv::from_env(),
            tools: state.tools(),
        }
    }

    fn from_env() -> Self {
        Setup {
            env: StorageEnv::from_env(),
            tools: Tools::real(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    Storage,
}

type Spawn = Box<dyn Fn(&Setup, &Leadership) + Send + Sync>;
type Once = Pin<Box<dyn Future<Output = anyhow::Result<runtime::Tick>> + Send>>;
type OnceFn = Box<dyn Fn(&Setup) -> Once + Send + Sync>;
#[cfg(test)]
type BuiltName = Box<dyn Fn(&Setup) -> &'static str + Send + Sync>;

pub struct Entry {
    pub name: &'static str,
    pub needs: Needs,
    spawn: Spawn,
    once: OnceFn,
    #[cfg(test)]
    built_name: BuiltName,
}

fn entry<C: runtime::Resource>(name: &'static str, needs: Needs, make: fn(&Setup) -> C) -> Entry {
    Entry {
        name,
        needs,
        spawn: Box::new(move |setup, leader| {
            runtime::spawn(make(setup), leader.clone(), setup.tools.clone())
        }),
        once: Box::new(move |setup| {
            let controller = make(setup);
            Box::pin(async move { runtime::run_once(&controller).await })
        }),
        #[cfg(test)]
        built_name: Box::new(move |setup| make(setup).name()),
    }
}

pub fn table() -> Vec<Entry> {
    use crate::backup::{check as backup_check, restore, run as backup};
    use crate::routers::{apps, backups, install};
    use crate::storage::controllers as storage;

    use Needs::{Nothing, Storage};

    vec![
        entry("backup-scheduler", Nothing, |_| {
            backup::BackupSchedulerController
        }),
        entry("restores", Nothing, |_| restore::RestoresController),
        entry("uninstalls", Nothing, |_| apps::UninstallsController),
        entry("installs", Nothing, |_| install::InstallsController),
        entry("outputs", Nothing, |_| crate::outputs::OutputsController),
        entry("store-sync", Nothing, |_| {
            crate::store::sync::StoreSyncController {
                config: Config::from_env(),
            }
        }),
        entry("ssh-keys", Nothing, |_| crate::ssh_keys::SshKeysController),
        entry("backup-lock-sweeper", Nothing, |_| {
            backups::LockSweeperController
        }),
        entry(crate::volsync_removal::NAME, Nothing, |_| {
            crate::volsync_removal::VolsyncRemovalController
        }),
        entry("backup-check", Nothing, |_| {
            backup_check::BackupCheckController
        }),
        entry("disks", Nothing, |_| {
            crate::disks_reconciler::DisksController
        }),
        entry("cephfs", Nothing, |_| crate::cephfs::CephFsController),
        entry("topology", Nothing, |_| crate::topology::TopologyController),
        entry("heal", Nothing, |_| crate::heal::HealController {
            config: Config::from_env(),
        }),
        entry("backup-credentials", Nothing, |_| {
            crate::heal::credentials::BackupCredentialsController
        }),
        entry("backup-key", Nothing, |_| backups::BackupKeyController {
            config: Config::from_env(),
        }),
        entry("notifier", Nothing, |_| {
            crate::notify::alerts::NotifierController {
                config: Config::from_env(),
            }
        }),
        entry("shared-names", Nothing, |_| {
            crate::shared_names::SharedNamesController {
                config: Config::from_env(),
            }
        }),
        entry("mesh-paths", Nothing, |_| {
            crate::mesh::MeshPathsController::new()
        }),
        entry("mesh-discovery", Nothing, |_| {
            crate::mesh::MeshDiscoveryController::new(Config::from_env())
        }),
        entry("hardware-labels", Nothing, |_| {
            crate::hardware::HardwareLabelsController::real()
        }),
        entry("chart-sync", Nothing, |_| {
            crate::charts::ChartSyncController
        }),
        entry(crate::vdisk::NAME, Storage, |s| {
            crate::vdisk::VdisksController {
                config: Config::from_env(),
                host: s.tools.host.clone(),
                http: s.tools.http.clone(),
            }
        }),
        entry("osd-activate", Storage, |s| {
            storage::OsdActivateController { env: s.env.clone() }
        }),
        entry("system-osd", Storage, |s| storage::SystemOsdController {
            env: s.env.clone(),
        }),
        entry("images-rbd", Storage, |s| storage::ImagesRbdController {
            env: s.env.clone(),
        }),
        entry("images-grow", Storage, |s| storage::ImagesGrowController {
            env: s.env.clone(),
        }),
        entry("ceph-dashboard", Storage, |s| {
            storage::DashboardController { env: s.env.clone() }
        }),
        entry("mon-member", Storage, |s| storage::MonMemberController {
            env: s.env.clone(),
        }),
        entry("ceph-keys", Storage, |_| storage::CephKeysController),
        entry("ceph-join", Storage, |s| storage::CephJoinController {
            env: s.env.clone(),
        }),
        entry("csi-secrets", Storage, |_| storage::CsiSecretsController),
        entry("csi-recovery", Storage, |_| storage::CsiRecoveryController),
        entry("ceph-csi", Storage, |_| {
            crate::storage::ceph_csi::CephCsiController
        }),
    ]
}

pub fn names() -> Vec<&'static str> {
    table().iter().map(|e| e.name).collect()
}

fn runs_here(entry: &Entry, env: &StorageEnv) -> bool {
    entry.needs == Needs::Nothing || env.is_configured()
}

fn spawn_observed(leader: &Leadership, tools: &Tools) {
    for r in runtime::Requirement::ALL {
        runtime::spawn(
            runtime::resource::Observed(r),
            leader.clone(),
            tools.clone(),
        );
    }
}

pub fn spawn_all(leader: Leadership, state: &crate::AppState) {
    let setup = Setup::from_state(state);
    for entry in table() {
        if runs_here(&entry, &setup.env) {
            (entry.spawn)(&setup, &leader);
        }
    }

    spawn_observed(&leader, &setup.tools);

    for watch in runtime::watch::standard() {
        runtime::watch::spawn(watch);
    }

    for problem in runtime::resource::problems() {
        tracing::error!("resource graph: {problem}");
    }
}

pub async fn run_named(name: &str) -> anyhow::Result<runtime::Tick> {
    let table = table();
    let Some(entry) = table.iter().find(|e| e.name == name) else {
        anyhow::bail!(
            "unknown controller '{name}' (known: {})",
            names().join(", ")
        );
    };
    let setup = Setup::from_env();
    (entry.once)(&setup).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_setup() -> Setup {
        Setup {
            env: StorageEnv::from_lookup(|_| None),
            tools: Tools::unreachable(),
        }
    }

    #[test]
    fn names_are_unique_and_non_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for name in names() {
            assert!(!name.is_empty(), "a controller has no name");
            assert!(seen.insert(name), "duplicate controller name {name}");
        }
    }

    #[tokio::test]
    async fn every_entry_is_named_after_the_controller_it_builds() {
        let setup = test_setup();
        for e in table() {
            assert_eq!((e.built_name)(&setup), e.name);
        }
    }

    #[tokio::test]
    async fn storage_controllers_run_only_where_storage_is_configured() {
        let table = table();
        let osd = table.iter().find(|e| e.name == "osd-activate").unwrap();
        let backups = table.iter().find(|e| e.name == "backup-scheduler").unwrap();
        let unconfigured = test_setup().env;
        assert!(!unconfigured.is_configured());
        assert!(!runs_here(osd, &unconfigured));
        assert!(runs_here(backups, &unconfigured));
    }

    #[test]
    fn the_volsync_removal_controller_is_registered() {
        assert!(names().contains(&crate::volsync_removal::NAME));
    }

    #[test]
    fn the_virtual_disks_controller_is_registered() {
        assert!(names().contains(&crate::vdisk::NAME));
    }

    #[tokio::test]
    async fn a_cluster_controller_run_by_hand_needs_a_cluster() {
        let err = run_named("backup-scheduler").await.unwrap_err();
        assert_eq!(err.to_string(), "this test runs without a cluster");
    }

    struct Dangling;

    impl runtime::Resource for Dangling {
        fn name(&self) -> &'static str {
            "test-controllers-dangling"
        }
        fn depends_on(&self) -> &[&'static str] {
            &["test-controllers-nobody"]
        }
        fn interval(&self) -> std::time::Duration {
            std::time::Duration::from_secs(1)
        }
        async fn converge(&self, _ctx: &runtime::Ctx) -> anyhow::Result<runtime::Tick> {
            Ok(runtime::Tick::Idle("nothing depends on this".into()))
        }
    }

    #[test]
    fn every_controller_and_watch_is_started_and_a_broken_graph_is_reported() {
        use crate::runtime::Resource as _;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            rt.block_on(async {
                Dangling
                    .converge(&runtime::Ctx {
                        node: "node1".into(),
                        tools: Tools::unreachable(),
                    })
                    .await
            })
            .unwrap(),
            runtime::Tick::Idle("nothing depends on this".into())
        );
        let _inside = rt.enter();
        runtime::spawn(
            Dangling,
            Leadership::fixed_for_tests(false),
            Tools::unreachable(),
        );
        let dir = tempfile::tempdir().unwrap();
        let config = std::sync::Arc::new(Config::for_test(&dir.path().join("config.toml")));
        let state = crate::AppState {
            auth: crate::auth::AuthState::isolated(std::sync::Arc::clone(&config)),
            config,
            kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::Client::new(),
            proxy_http: crate::http::Client::new(),
            host: crate::host::fake::FakeHost::new(),
        };
        spawn_all(Leadership::fixed_for_tests(false), &state);
        assert!(runtime::resource::problems().contains(
            &"test-controllers-dangling depends on 'test-controllers-nobody', which is not a registered resource"
                .to_string()
        ));
        assert!(runtime::registry().get("backup-scheduler").is_some());
        assert!(runtime::registry().get(runtime::KUBE_API).is_some());
    }

    #[tokio::test]
    async fn an_unknown_controller_name_is_refused() {
        let err = run_named("not-a-controller").await.unwrap_err();
        assert!(err.to_string().contains("unknown controller"), "{err}");
    }
}
