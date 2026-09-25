pub mod entry;
pub mod hlc;
pub mod sync;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use automerge::transaction::Transactable;
use automerge::{AutoCommit, AutomergeError, ReadDoc, ROOT};
use serde::{Deserialize, Serialize};

use entry::{resolve, Entry, Origin};
use hlc::Clock;

const DISK_CLAIM_PREFIX: &str = "disk_claim:";
const DISKS_SEEDED_KEY: &str = "meta:disks_seeded";

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
        let applied = self.doc.merge(&mut other.doc)?;
        Ok(!applied.is_empty())
    }

    pub fn set_disk_intent(
        &mut self,
        node: &str,
        disk_id: &str,
        intent: DiskIntent,
    ) -> Result<(), StoreError> {
        let key = disk_key(node, disk_id);
        self.write(&key, intent, Origin::User)
    }

    pub fn observe_disk(&mut self, node: &str, disk_id: &str) -> Result<bool, StoreError> {
        let key = disk_key(node, disk_id);
        if self.read(&key)?.is_some() {
            return Ok(false);
        }
        self.write(&key, DiskIntent::Off, Origin::Discovered)?;
        Ok(true)
    }

    pub fn import_claim(
        &mut self,
        record_key: &str,
        intent: DiskIntent,
        origin: Origin,
    ) -> Result<bool, StoreError> {
        let key = format!("{DISK_CLAIM_PREFIX}{record_key}");
        match self.read(&key)? {
            Some(existing) if existing.origin == Origin::User => Ok(false),
            Some(_) if origin == Origin::Discovered => Ok(false),
            _ => {
                self.write(&key, intent, origin)?;
                Ok(true)
            }
        }
    }

    pub fn desired_records(&self) -> Result<HashMap<String, String>, StoreError> {
        Ok(self
            .disk_claims()?
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
        if self.disks_seeded() {
            return Ok(false);
        }
        self.doc.put(ROOT, DISKS_SEEDED_KEY, "1")?;
        Ok(true)
    }

    pub fn disks_seeded(&self) -> bool {
        self.doc
            .get(ROOT, DISKS_SEEDED_KEY)
            .ok()
            .flatten()
            .is_some()
    }

    pub fn disk_claims(&self) -> Result<BTreeMap<String, Entry<DiskIntent>>, StoreError> {
        let keys: Vec<String> = self.doc.keys(ROOT).collect();
        let mut out = BTreeMap::new();
        for key in keys {
            let Some(name) = key.strip_prefix(DISK_CLAIM_PREFIX) else {
                continue;
            };
            if let Some(entry) = self.read(&key)? {
                out.insert(name.to_string(), entry);
            }
        }
        Ok(out)
    }

    pub fn debug_json(&self) -> Result<serde_json::Value, StoreError> {
        Ok(serde_json::json!({ "disk_claims": self.disk_claims()? }))
    }

    fn read(&self, key: &str) -> Result<Option<Entry<DiskIntent>>, StoreError> {
        let mut candidates = Vec::new();
        for (value, _) in self.doc.get_all(ROOT, key)? {
            let Some(raw) = value.as_str() else {
                continue;
            };
            let parsed: Entry<DiskIntent> =
                serde_json::from_str(raw).map_err(|e| StoreError::Corrupt {
                    key: key.to_string(),
                    detail: e.to_string(),
                })?;
            self.clock.observe(&parsed.hlc);
            candidates.push(parsed);
        }
        Ok(resolve(candidates))
    }

    fn write(&mut self, key: &str, value: DiskIntent, origin: Origin) -> Result<(), StoreError> {
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
}

fn disk_key(node: &str, disk_id: &str) -> String {
    format!("{DISK_CLAIM_PREFIX}{node}--{disk_id}")
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
        .unwrap_or_else(|_| PathBuf::from("/var/lib/yolab/store.automerge"))
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
}
