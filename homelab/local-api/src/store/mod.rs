#![allow(dead_code)]

pub mod entry;
pub mod hlc;

use std::collections::BTreeMap;
use std::fmt;

use automerge::transaction::Transactable;
use automerge::{AutoCommit, AutomergeError, ReadDoc, ROOT};
use serde::{Deserialize, Serialize};

use entry::{resolve, Entry, Origin};
use hlc::Clock;

const DISK_CLAIM_PREFIX: &str = "disk_claim:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskIntent {
    On,
    Off,
    Forgotten,
}

#[derive(Debug)]
pub enum StoreError {
    Doc(AutomergeError),
    Corrupt { key: String, detail: String },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Doc(e) => write!(f, "{e}"),
            StoreError::Corrupt { key, detail } => {
                write!(f, "{key}: stored entry is unreadable: {detail}")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Doc(e) => Some(e),
            StoreError::Corrupt { .. } => None,
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

    pub fn save(&mut self) -> Vec<u8> {
        self.doc.save()
    }

    pub fn merge(&mut self, other: &mut Store) -> Result<(), StoreError> {
        self.doc.merge(&mut other.doc)?;
        Ok(())
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

    pub fn disk_intent(&self, node: &str, disk_id: &str) -> Result<Option<DiskIntent>, StoreError> {
        Ok(self.read(&disk_key(node, disk_id))?.map(|e| e.value))
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_lone_node_accepts_writes_with_no_peers() {
        let mut only = Store::new("node1");
        only.set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        assert_eq!(
            only.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::On)
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

        assert_eq!(
            chooser.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::On)
        );
        assert_eq!(
            stale.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::On)
        );
    }

    #[test]
    fn discovery_does_not_overwrite_an_entry_that_already_exists() {
        let mut store = Store::new("node1");
        store
            .set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        assert!(!store.observe_disk("node1", "wwn-a").unwrap());
        assert_eq!(
            store.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::On)
        );
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
            owner.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::Forgotten)
        );
        assert_eq!(
            peer.disk_intent("node1", "wwn-a").unwrap(),
            Some(DiskIntent::Forgotten)
        );
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

    #[test]
    fn a_corrupt_entry_is_reported_rather_than_silently_skipped() {
        let mut store = Store::new("node1");
        store
            .doc
            .put(ROOT, disk_key("node1", "wwn-a"), "not json")
            .unwrap();
        assert!(matches!(
            store.disk_intent("node1", "wwn-a"),
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
