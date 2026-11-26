use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex, OnceLock};

use crate::runtime::activity::Cache;
use crate::store::Store;

pub fn leader_renewed() -> Arc<AtomicI64> {
    static RENEWED: OnceLock<Arc<AtomicI64>> = OnceLock::new();
    RENEWED.get_or_init(|| Arc::new(AtomicI64::new(0))).clone()
}

pub fn scratch_dir() -> PathBuf {
    std::env::temp_dir()
}

pub fn updating() -> &'static AtomicBool {
    static IS_UPDATING: AtomicBool = AtomicBool::new(false);
    &IS_UPDATING
}

pub fn alerts_state_root() -> PathBuf {
    PathBuf::from("/")
}

pub fn ssh_keys_file() -> PathBuf {
    std::env::var("YOLAB_SSH_KEYS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(format!(
                "/var/lib/yolab-ssh/{}.keys",
                crate::ssh_keys::LOGIN_USER
            ))
        })
}

fn this_node() -> String {
    let node = crate::system::hostname();
    assert!(
        !node.is_empty(),
        "/etc/hostname is missing or empty, so this machine cannot sign the changes it makes to \
         the store"
    );
    node
}

pub fn shared_store() -> &'static Mutex<Store> {
    static SHARED: OnceLock<Mutex<Store>> = OnceLock::new();
    SHARED.get_or_init(|| {
        let node = this_node();
        let path = store_path();
        let store = Store::open(&node, &path).unwrap_or_else(|e| {
            tracing::error!(
                "the desired-state store at {} cannot be read ({e}) — starting empty, and it will \
                 not be acted on until it has been seeded from the cluster again",
                path.display()
            );
            Store::new(&node)
        });
        Mutex::new(store)
    })
}

pub fn store_path() -> PathBuf {
    std::env::var("YOLAB_STORE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/yolab/store.automerge"))
}

pub fn activity_cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Cache::default)
}

pub fn charts_cache_root() -> PathBuf {
    PathBuf::from("/var/lib/yolab/charts")
}

pub fn official_chart_repo() -> String {
    crate::charts::default_official_url()
}

pub fn backup_staging_root() -> String {
    "/var/lib/yolab/backup-staging".into()
}

pub fn etcd_snapshot_dir() -> String {
    "/var/lib/rancher/k3s/server/db/snapshots".into()
}

pub fn built_hash_file() -> String {
    "/var/lib/yolab/built-hash".into()
}

pub fn machine_root() -> &'static Path {
    Path::new("/")
}

pub fn ntfy_local() -> String {
    crate::notify::NTFY_LOCAL.to_string()
}

pub fn publish_snapshot() {
    crate::runtime::status::write_snapshot(Path::new(crate::runtime::status::SNAPSHOT_PATH));
}
