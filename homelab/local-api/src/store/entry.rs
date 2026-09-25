use serde::{Deserialize, Serialize};

use super::hlc::Hlc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Discovered,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry<T> {
    #[serde(rename = "v")]
    pub value: T,
    #[serde(rename = "o")]
    pub origin: Origin,
    #[serde(rename = "t")]
    pub hlc: Hlc,
}

pub fn resolve<T>(candidates: Vec<Entry<T>>) -> Option<Entry<T>> {
    candidates
        .into_iter()
        .max_by(|a, b| a.origin.cmp(&b.origin).then_with(|| a.hlc.cmp(&b.hlc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(value: u8, origin: Origin, millis: u64, node: &str) -> Entry<u8> {
        Entry {
            value,
            origin,
            hlc: Hlc {
                millis,
                counter: 0,
                node: node.to_string(),
            },
        }
    }

    #[test]
    fn a_user_write_outranks_a_strictly_later_discovered_write() {
        let user = entry(1, Origin::User, 10, "node1");
        let discovered = entry(2, Origin::Discovered, 9_999, "node2");
        let winner = resolve(vec![discovered, user.clone()]).unwrap();
        assert_eq!(winner, user);
    }

    #[test]
    fn among_writes_of_equal_origin_the_later_timestamp_wins() {
        let older = entry(1, Origin::User, 10, "node1");
        let newer = entry(2, Origin::User, 20, "node1");
        let winner = resolve(vec![newer.clone(), older]).unwrap();
        assert_eq!(winner, newer);
    }

    #[test]
    fn resolution_does_not_depend_on_candidate_order() {
        let a = entry(1, Origin::User, 10, "node1");
        let b = entry(2, Origin::User, 10, "node2");
        let c = entry(3, Origin::Discovered, 99, "node3");
        let forward = resolve(vec![a.clone(), b.clone(), c.clone()]);
        let reverse = resolve(vec![c, b, a]);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn resolving_no_candidates_yields_nothing() {
        assert_eq!(resolve(Vec::<Entry<u8>>::new()), None);
    }
}
