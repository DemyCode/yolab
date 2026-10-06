pub mod entry;
pub mod hlc;
pub mod sync;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use automerge::transaction::Transactable;
use automerge::{AutoCommit, AutomergeError, ReadDoc, ROOT};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use entry::{resolve, Entry, Origin};
use hlc::Clock;

use crate::topology::StoragePolicy;

const DISK_CLAIM_PREFIX: &str = "disk_claim:";
const MACHINE_PREFIX: &str = "machine:";
const STORAGE_POLICY_KEY: &str = "policy:storage";

const DISKS_SEEDED_KEY: &str = "meta:disks_seeded";
const MACHINES_SEEDED_KEY: &str = "meta:machines_seeded";
const POLICY_SEEDED_KEY: &str = "meta:policy_seeded";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum DiskIntent {
    On,
    Off,
    Forgotten,
    Unknown(String),
}

impl From<String> for DiskIntent {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "on" => DiskIntent::On,
            "off" => DiskIntent::Off,
            "forgotten" => DiskIntent::Forgotten,
            _ => DiskIntent::Unknown(raw),
        }
    }
}

impl From<DiskIntent> for String {
    fn from(intent: DiskIntent) -> String {
        match intent {
            DiskIntent::On => "on".to_string(),
            DiskIntent::Off => "off".to_string(),
            DiskIntent::Forgotten => "forgotten".to_string(),
            DiskIntent::Unknown(raw) => raw,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum MachineState {
    Member,
    Draining,
    Removed,
    Unknown(String),
}

impl From<String> for MachineState {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "member" => MachineState::Member,
            "draining" => MachineState::Draining,
            "removed" => MachineState::Removed,
            _ => MachineState::Unknown(raw),
        }
    }
}

impl From<MachineState> for String {
    fn from(state: MachineState) -> String {
        match state {
            MachineState::Member => "member".to_string(),
            MachineState::Draining => "draining".to_string(),
            MachineState::Removed => "removed".to_string(),
            MachineState::Unknown(raw) => raw,
        }
    }
}

#[derive(Debug)]
pub enum StoreError {
    Doc(AutomergeError),
    Corrupt { key: String, detail: String },
    Io { path: String, detail: String },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Doc(e) => write!(f, "{e}"),
            StoreError::Corrupt { key, detail } => {
                write!(f, "{key}: stored entry is unreadable: {detail}")
            }
            StoreError::Io { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Doc(e) => Some(e),
            StoreError::Corrupt { .. } | StoreError::Io { .. } => None,
        }
    }
}

impl From<AutomergeError> for StoreError {
    fn from(e: AutomergeError) -> Self {
        StoreError::Doc(e)
    }
}

pub struct Store {
    doc: AutoCommit,
    clock: Clock,
}

impl Store {
    pub fn new(node: &str) -> Self {
        Store {
            doc: AutoCommit::new(),
            clock: Clock::new(node),
        }
    }

    pub fn load(node: &str, bytes: &[u8]) -> Result<Self, StoreError> {
        Ok(Store {
            doc: AutoCommit::load(bytes)?,
            clock: Clock::new(node),
        })
    }

    pub fn open(node: &str, path: &Path) -> Result<Self, StoreError> {
        match std::fs::read(path) {
            Ok(bytes) => Store::load(node, &bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::new(node)),
            Err(e) => Err(StoreError::Io {
                path: path.display().to_string(),
                detail: e.to_string(),
            }),
        }
    }

    pub fn persist(&mut self, path: &Path) -> Result<(), StoreError> {
        let bytes = self.save();
        crate::config::write_private_file(path, &bytes).map_err(|e| StoreError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;

        let readable =
            serde_json::to_vec_pretty(&self.debug_json()?).map_err(|e| StoreError::Corrupt {
                key: path.display().to_string(),
                detail: e.to_string(),
            })?;
        let inspectable = debug_path(path);
        crate::config::write_private_file(&inspectable, &readable).map_err(|e| StoreError::Io {
            path: inspectable.display().to_string(),
            detail: e.to_string(),
        })
    }

    pub fn save(&mut self) -> Vec<u8> {
        self.doc.save()
    }

    pub fn merge(&mut self, other: &mut Store) -> Result<bool, StoreError> {
        let before = self.doc.get_heads();
        let after = self.doc.merge(&mut other.doc)?;
        Ok(after != before)
    }

    fn read_at<T: DeserializeOwned>(&self, key: &str) -> Result<Option<Entry<T>>, StoreError> {
        let mut candidates = Vec::new();
        for (value, _) in self.doc.get_all(ROOT, key)? {
            let Some(raw) = value.as_str() else {
                continue;
            };
            let parsed: Entry<T> = serde_json::from_str(raw).map_err(|e| StoreError::Corrupt {
                key: key.to_string(),
                detail: e.to_string(),
            })?;
            self.clock.observe(&parsed.hlc);
            candidates.push(parsed);
        }
        Ok(resolve(candidates))
    }

    fn write_at<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
        origin: Origin,
    ) -> Result<(), StoreError> {
        let entry = Entry {
            value,
            origin,
            hlc: self.clock.now(),
        };
        let raw = serde_json::to_string(&entry).map_err(|e| StoreError::Corrupt {
            key: key.to_string(),
            detail: e.to_string(),
        })?;
        self.doc.put(ROOT, key, raw)?;
        Ok(())
    }

    fn origin_at(&self, key: &str) -> Result<Option<Origin>, StoreError> {
        Ok(self.read_at::<Value>(key)?.map(|e| e.origin))
    }

    fn import_at<T: Serialize>(
        &mut self,
        key: &str,
        value: &T,
        origin: Origin,
    ) -> Result<bool, StoreError> {
        match self.origin_at(key)? {
            Some(Origin::User) => Ok(false),
            Some(_) if origin == Origin::Discovered => Ok(false),
            _ => {
                self.write_at(key, value, origin)?;
                Ok(true)
            }
        }
    }

    fn collection<T: DeserializeOwned>(
        &self,
        prefix: &str,
    ) -> Result<BTreeMap<String, Entry<T>>, StoreError> {
        let keys: Vec<String> = self.doc.keys(ROOT).collect();
        let mut out = BTreeMap::new();
        for key in keys {
            let Some(name) = key.strip_prefix(prefix) else {
                continue;
            };
            if let Some(entry) = self.read_at::<T>(&key)? {
                out.insert(name.to_string(), entry);
            }
        }
        Ok(out)
    }

    fn flag(&mut self, key: &str) -> Result<bool, StoreError> {
        if self.flagged(key) {
            return Ok(false);
        }
        self.doc.put(ROOT, key, "1")?;
        Ok(true)
    }

    fn flagged(&self, key: &str) -> bool {
        self.doc.get(ROOT, key).ok().flatten().is_some()
    }

    pub fn set_disk_intent(
        &mut self,
        node: &str,
        disk_id: &str,
        intent: DiskIntent,
    ) -> Result<(), StoreError> {
        self.write_at(&disk_key(node, disk_id), &intent, Origin::User)
    }

    pub fn observe_disk(&mut self, node: &str, disk_id: &str) -> Result<bool, StoreError> {
        self.import_at(
            &disk_key(node, disk_id),
            &DiskIntent::Off,
            Origin::Discovered,
        )
    }

    pub fn import_claim(
        &mut self,
        record_key: &str,
        intent: DiskIntent,
        origin: Origin,
    ) -> Result<bool, StoreError> {
        self.import_at(&format!("{DISK_CLAIM_PREFIX}{record_key}"), &intent, origin)
    }

    pub fn disk_claims(&self) -> Result<BTreeMap<String, Entry<DiskIntent>>, StoreError> {
        self.collection(DISK_CLAIM_PREFIX)
    }

    pub fn desired_records(&self) -> Result<HashMap<String, String>, StoreError> {
        Ok(fold_hardware_claims(self.disk_claims()?)
            .into_iter()
            .map(|(name, entry)| {
                let setting = if entry.value == DiskIntent::On {
                    "ON"
                } else {
                    "OFF"
                };
                (name, setting.to_string())
            })
            .collect())
    }

    pub fn mark_disks_seeded(&mut self) -> Result<bool, StoreError> {
        self.flag(DISKS_SEEDED_KEY)
    }

    pub fn disks_seeded(&self) -> bool {
        self.flagged(DISKS_SEEDED_KEY)
    }

    pub fn set_storage_policy(&mut self, policy: &StoragePolicy) -> Result<(), StoreError> {
        self.write_at(STORAGE_POLICY_KEY, policy, Origin::User)
    }

    pub fn import_storage_policy(&mut self, policy: &StoragePolicy) -> Result<bool, StoreError> {
        self.import_at(STORAGE_POLICY_KEY, policy, Origin::User)
    }

    pub fn storage_policy(&self) -> Result<Option<StoragePolicy>, StoreError> {
        Ok(self
            .read_at::<StoragePolicy>(STORAGE_POLICY_KEY)?
            .map(|e| e.value))
    }

    pub fn mark_policy_seeded(&mut self) -> Result<bool, StoreError> {
        self.flag(POLICY_SEEDED_KEY)
    }

    pub fn policy_seeded(&self) -> bool {
        self.flagged(POLICY_SEEDED_KEY)
    }

    pub fn machines(&self) -> Result<BTreeMap<String, Entry<MachineState>>, StoreError> {
        self.collection(MACHINE_PREFIX)
    }

    pub fn machines_seeded(&self) -> bool {
        self.flagged(MACHINES_SEEDED_KEY)
    }

    pub fn debug_json(&self) -> Result<Value, StoreError> {
        Ok(serde_json::json!({
            "disk_claims": self.disk_claims()?,
            "machines": self.machines()?,
            "storage_policy": self.read_at::<StoragePolicy>(STORAGE_POLICY_KEY)?,
            "seeded": {
                "disks": self.disks_seeded(),
                "machines": self.machines_seeded(),
                "storage_policy": self.policy_seeded(),
            }
        }))
    }

}

fn disk_key(node: &str, disk_id: &str) -> String {
    format!(
        "{DISK_CLAIM_PREFIX}{}",
        crate::disks_reconciler::record_key(node, disk_id)
    )
}

fn hardware_id_behind_node(name: &str) -> Option<&str> {
    if crate::disks_reconciler::is_globally_unique_id(name) {
        return None;
    }
    let (_, id) = name.split_once("--")?;
    crate::disks_reconciler::is_globally_unique_id(id).then_some(id)
}

fn fold_hardware_claims<T>(claims: BTreeMap<String, Entry<T>>) -> BTreeMap<String, Entry<T>> {
    let mut grouped: BTreeMap<String, Vec<Entry<T>>> = BTreeMap::new();
    for (name, entry) in claims {
        let key = hardware_id_behind_node(&name)
            .map(str::to_string)
            .unwrap_or(name);
        grouped.entry(key).or_default().push(entry);
    }
    grouped
        .into_iter()
        .filter_map(|(key, candidates)| resolve(candidates).map(|e| (key, e)))
        .collect()
}

pub fn shared() -> &'static Mutex<Store> {
    static SHARED: OnceLock<Mutex<Store>> = OnceLock::new();
    SHARED.get_or_init(|| {
        let node = crate::system::hostname();
        let path = default_path();
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

pub fn locked() -> std::sync::MutexGuard<'static, Store> {
    shared()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn default_path() -> PathBuf {
    std::env::var("YOLAB_STORE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/yolab/store-v2.automerge"))
}

fn debug_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".to_string());
    path.with_file_name(format!("{stem}-debug.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn intent(store: &Store, node: &str, disk_id: &str) -> Option<DiskIntent> {
        store
            .disk_claims()
            .unwrap()
            .get(&format!("{node}--{disk_id}"))
            .map(|e| e.value.clone())
    }

    #[test]
    fn a_lone_node_accepts_writes_with_no_peers() {
        let mut only = Store::new("node1");
        only.set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        assert_eq!(intent(&only, "node1", "wwn-a"), Some(DiskIntent::On));
    }

    #[test]
    fn a_merge_reports_a_change_only_when_the_other_side_knew_something_new() {
        let mut a = Store::new("node1");
        a.set_disk_intent("node1", "wwn-a", DiskIntent::On).unwrap();
        let mut echo = Store::load("node1", &a.save()).unwrap();
        assert!(
            !a.merge(&mut echo).unwrap(),
            "its own document back is no news"
        );

        let mut b = Store::new("node2");
        b.set_disk_intent("node2", "wwn-b", DiskIntent::On).unwrap();
        assert!(a.merge(&mut b).unwrap());
        assert!(
            !a.merge(&mut b).unwrap(),
            "the same news twice is news once"
        );
    }

    #[test]
    fn partitioned_writes_converge_once_both_sides_merge() {
        let mut a = Store::new("node1");
        let mut b = Store::new("node2");
        a.set_disk_intent("node1", "wwn-a", DiskIntent::On).unwrap();
        b.set_disk_intent("node2", "wwn-b", DiskIntent::On).unwrap();

        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        assert_eq!(a.disk_claims().unwrap(), b.disk_claims().unwrap());
        assert_eq!(a.disk_claims().unwrap().len(), 2);
    }

    #[test]
    fn a_discovered_default_never_clobbers_a_user_choice_it_never_saw() {
        let mut chooser = Store::new("node1");
        chooser
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();

        let mut stale = Store::new("node2");
        assert!(stale.observe_disk("node1", "wwn-a").unwrap());

        chooser.merge(&mut stale).unwrap();
        stale.merge(&mut chooser).unwrap();

        assert_eq!(intent(&chooser, "node1", "wwn-a"), Some(DiskIntent::On));
        assert_eq!(intent(&stale, "node1", "wwn-a"), Some(DiskIntent::On));
    }

    #[test]
    fn discovery_does_not_overwrite_an_entry_that_already_exists() {
        let mut store = Store::new("node1");
        store
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        assert!(!store.observe_disk("node1", "wwn-a").unwrap());
        assert_eq!(intent(&store, "node1", "wwn-a"), Some(DiskIntent::On));
    }

    #[test]
    fn a_forgotten_disk_is_not_resurrected_by_a_peer_that_still_sees_it() {
        let mut owner = Store::new("node1");
        owner
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();

        let mut peer = Store::new("node2");
        peer.merge(&mut owner).unwrap();

        owner
            .set_disk_intent("node1", "wwn-a", DiskIntent::Forgotten)
            .unwrap();
        assert!(!peer.observe_disk("node1", "wwn-a").unwrap());

        owner.merge(&mut peer).unwrap();
        peer.merge(&mut owner).unwrap();

        assert_eq!(
            intent(&owner, "node1", "wwn-a"),
            Some(DiskIntent::Forgotten)
        );
        assert_eq!(intent(&peer, "node1", "wwn-a"), Some(DiskIntent::Forgotten));
    }

    #[test]
    fn a_saved_document_reloads_with_the_same_intent() {
        let mut before = Store::new("node1");
        before
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        before
            .set_disk_intent("node2", "wwn-b", DiskIntent::Off)
            .unwrap();
        let bytes = before.save();

        let after = Store::load("node1", &bytes).unwrap();
        assert_eq!(after.disk_claims().unwrap(), before.disk_claims().unwrap());
    }

    fn raw_entry(store: &mut Store, node: &str, disk_id: &str, body: &str) {
        store.doc.put(ROOT, disk_key(node, disk_id), body).unwrap();
    }

    #[test]
    fn an_intent_written_by_a_newer_version_survives_a_round_trip_unchanged() {
        let mut store = Store::new("node1");
        raw_entry(
            &mut store,
            "node1",
            "wwn-a",
            r#"{"v":"draining","o":"user","t":"0000000000001-00000-node9"}"#,
        );

        assert_eq!(
            intent(&store, "node1", "wwn-a"),
            Some(DiskIntent::Unknown("draining".to_string()))
        );

        let bytes = store.save();
        let reopened = Store::load("node1", &bytes).unwrap();
        assert_eq!(
            intent(&reopened, "node1", "wwn-a"),
            Some(DiskIntent::Unknown("draining".to_string()))
        );
    }

    #[test]
    fn an_intent_written_by_a_newer_version_does_not_blind_the_whole_listing() {
        let mut store = Store::new("node1");
        store
            .set_disk_intent("node1", "wwn-known", DiskIntent::On)
            .unwrap();
        raw_entry(
            &mut store,
            "node1",
            "wwn-future",
            r#"{"v":"draining","o":"user","t":"0000000000001-00000-node9"}"#,
        );

        let claims = store.disk_claims().unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(
            claims.get("node1--wwn-known").map(|e| e.value.clone()),
            Some(DiskIntent::On)
        );
    }

    #[test]
    fn an_absent_file_opens_as_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open("node1", &dir.path().join("store.automerge")).unwrap();
        assert!(store.disk_claims().unwrap().is_empty());
    }

    #[test]
    fn a_persisted_store_reopens_with_the_same_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");

        let mut before = Store::open("node1", &path).unwrap();
        before
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        before.persist(&path).unwrap();

        let after = Store::open("node1", &path).unwrap();
        assert_eq!(after.disk_claims().unwrap(), before.disk_claims().unwrap());
    }

    #[test]
    fn persisting_leaves_a_json_projection_beside_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");

        let mut store = Store::open("node1", &path).unwrap();
        store
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        store.persist(&path).unwrap();

        let projection = dir.path().join("store-debug.json");
        let text = std::fs::read_to_string(&projection).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["disk_claims"]["node1--wwn-a"]["v"], "on");
    }

    #[test]
    fn the_json_projection_does_not_share_a_temporary_path_with_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");
        assert_ne!(
            debug_path(&path).with_extension("tmp"),
            path.with_extension("tmp")
        );
    }

    #[test]
    fn a_corrupt_entry_is_reported_rather_than_silently_skipped() {
        let mut store = Store::new("node1");
        store
            .doc
            .put(ROOT, disk_key("node1", "wwn-a"), "not json")
            .unwrap();
        assert!(matches!(
            store.disk_claims(),
            Err(StoreError::Corrupt { .. })
        ));
    }

    proptest! {
        #[test]
        fn merge_order_does_not_change_the_resolved_view(
            writes in prop::collection::vec((0u8..4, any::<bool>(), any::<bool>()), 1..24)
        ) {
            let mut a = Store::new("node1");
            let mut b = Store::new("node2");

            for (disk, on_a, by_user) in &writes {
                let id = format!("wwn-{disk}");
                let target = if *on_a { &mut a } else { &mut b };
                if *by_user {
                    target.set_disk_intent("node1", &id, DiskIntent::On).unwrap();
                } else {
                    target.observe_disk("node1", &id).unwrap();
                }
            }

            let a_bytes = a.save();
            let b_bytes = b.save();

            let mut forward = Store::load("node1", &a_bytes).unwrap();
            let mut forward_peer = Store::load("node2", &b_bytes).unwrap();
            forward.merge(&mut forward_peer).unwrap();

            let mut reverse = Store::load("node2", &b_bytes).unwrap();
            let mut reverse_peer = Store::load("node1", &a_bytes).unwrap();
            reverse.merge(&mut reverse_peer).unwrap();

            prop_assert_eq!(forward.disk_claims().unwrap(), reverse.disk_claims().unwrap());
        }
    }

    fn policy(size: u32) -> StoragePolicy {
        StoragePolicy {
            size,
            failure_domain: "host".to_string(),
        }
    }

    #[test]
    fn a_storage_policy_round_trips() {
        let mut store = Store::new("node1");
        store.set_storage_policy(&policy(3)).unwrap();
        assert_eq!(store.storage_policy().unwrap(), Some(policy(3)));
    }

    #[test]
    fn taking_in_a_storage_policy_does_not_undo_one_chosen_since() {
        let mut store = Store::new("node1");
        store.set_storage_policy(&policy(3)).unwrap();
        assert!(!store.import_storage_policy(&policy(2)).unwrap());
        assert_eq!(store.storage_policy().unwrap(), Some(policy(3)));
    }

    #[test]
    fn a_storage_policy_is_taken_in_when_the_store_has_none() {
        let mut store = Store::new("node1");
        assert!(store.import_storage_policy(&policy(2)).unwrap());
        assert_eq!(store.storage_policy().unwrap(), Some(policy(2)));
    }

    fn put_machine(store: &mut Store, name: &str, state: &str) {
        store
            .doc
            .put(
                ROOT,
                format!("{MACHINE_PREFIX}{name}"),
                format!(r#"{{"v":"{state}","o":"user","t":"0000000000001-00000-node9"}}"#),
            )
            .unwrap();
    }

    #[test]
    fn a_machine_state_from_a_newer_version_is_kept_rather_than_guessed_at() {
        let mut store = Store::new("node1");
        put_machine(&mut store, "node2", "quarantined");
        assert_eq!(
            store.machines().unwrap()["node2"].value,
            MachineState::Unknown("quarantined".to_string())
        );
    }

    #[test]
    fn the_kinds_of_record_do_not_bleed_into_each_other() {
        let mut store = Store::new("node1");
        store
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        store.set_storage_policy(&policy(2)).unwrap();
        store.mark_disks_seeded().unwrap();

        assert_eq!(store.disk_claims().unwrap().len(), 1);
        assert!(store.machines().unwrap().is_empty());
        assert_eq!(store.storage_policy().unwrap(), Some(policy(2)));
        assert!(store.disks_seeded());
        assert!(!store.machines_seeded());
    }

    #[test]
    fn every_kind_of_record_survives_a_save_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");

        let mut before = Store::open("node1", &path).unwrap();
        before
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        put_machine(&mut before, "node2", "member");
        before.set_storage_policy(&policy(3)).unwrap();
        before.persist(&path).unwrap();

        let after = Store::open("node1", &path).unwrap();
        assert_eq!(after.disk_claims().unwrap(), before.disk_claims().unwrap());
        assert_eq!(after.machines().unwrap(), before.machines().unwrap());
        assert_eq!(after.storage_policy().unwrap(), Some(policy(3)));
    }

    #[test]
    fn a_node_with_no_file_at_all_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Store::open("node1", &dir.path().join("store-v2.automerge")).unwrap();
        assert!(opened.disk_claims().unwrap().is_empty());
    }

    #[test]
    fn two_machines_that_each_chose_something_different_keep_both_choices() {
        let mut a = Store::new("node1");
        let mut b = Store::new("node2");
        a.set_storage_policy(&policy(3)).unwrap();
        b.set_disk_intent("node3", "wwn-c", DiskIntent::Off)
            .unwrap();

        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        assert_eq!(a.storage_policy().unwrap(), Some(policy(3)));
        assert_eq!(
            a.disk_claims().unwrap()["node3--wwn-c"].value,
            DiskIntent::Off
        );
        assert_eq!(b.storage_policy().unwrap(), Some(policy(3)));
    }

    fn legacy_write(store: &mut Store, name: &str, value: DiskIntent) {
        store
            .write_at(&format!("{DISK_CLAIM_PREFIX}{name}"), &value, Origin::User)
            .unwrap();
    }

    #[test]
    fn a_hardware_disk_choice_is_stored_under_the_key_the_reconciler_reads() {
        let mut s = Store::new("node3");
        s.set_disk_intent("node2", "serial-wwn-0x50014ee214caf529", DiskIntent::Off)
            .unwrap();
        let records = s.desired_records().unwrap();
        let key = crate::disks_reconciler::record_key("node2", "serial-wwn-0x50014ee214caf529");
        assert_eq!(records.get(&key).map(String::as_str), Some("OFF"));
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn switching_off_a_hardware_disk_overrides_the_earlier_on() {
        let mut s = Store::new("node3");
        s.set_disk_intent("node2", "serial-wwn-x", DiskIntent::On)
            .unwrap();
        s.set_disk_intent("node2", "serial-wwn-x", DiskIntent::Off)
            .unwrap();
        assert_eq!(
            s.desired_records()
                .unwrap()
                .get("serial-wwn-x")
                .map(String::as_str),
            Some("OFF")
        );
    }

    #[test]
    fn the_newer_node_scoped_off_found_on_node3_beats_the_older_bare_on() {
        let mut s = Store::new("node3");
        legacy_write(&mut s, "serial-wwn-0x50014ee214caf529", DiskIntent::On);
        legacy_write(
            &mut s,
            "node2--serial-wwn-0x50014ee214caf529",
            DiskIntent::Off,
        );
        let records = s.desired_records().unwrap();
        assert_eq!(
            records
                .get("serial-wwn-0x50014ee214caf529")
                .map(String::as_str),
            Some("OFF")
        );
        assert!(
            !records.contains_key("node2--serial-wwn-0x50014ee214caf529"),
            "the node-scoped copy is folded in, not reported as a second disk"
        );
    }

    #[test]
    fn a_newer_bare_choice_beats_an_older_node_scoped_one() {
        let mut s = Store::new("node3");
        legacy_write(&mut s, "node2--serial-wwn-x", DiskIntent::Off);
        legacy_write(&mut s, "serial-wwn-x", DiskIntent::On);
        assert_eq!(
            s.desired_records()
                .unwrap()
                .get("serial-wwn-x")
                .map(String::as_str),
            Some("ON")
        );
    }

    #[test]
    fn a_hardware_id_containing_dashes_is_folded_whole() {
        assert_eq!(
            hardware_id_behind_node("node2--serial-ata-wdc--wd10"),
            Some("serial-ata-wdc--wd10")
        );
        assert_eq!(hardware_id_behind_node("serial-ata-wdc--wd10"), None);
        assert_eq!(hardware_id_behind_node("node1--dev-sdb"), None);
        assert_eq!(hardware_id_behind_node("node1--system"), None);
    }

    #[test]
    fn a_node_scoped_disk_without_a_hardware_id_keeps_its_node() {
        let mut s = Store::new("node1");
        s.set_disk_intent("node1", "dev-sdb", DiskIntent::Off)
            .unwrap();
        assert_eq!(
            s.desired_records()
                .unwrap()
                .get("node1--dev-sdb")
                .map(String::as_str),
            Some("OFF")
        );
    }
}
