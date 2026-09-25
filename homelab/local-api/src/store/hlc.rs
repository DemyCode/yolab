use std::cmp::Ordering;
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hlc {
    pub millis: u64,
    pub counter: u32,
    pub node: String,
}

impl Hlc {
    pub fn encode(&self) -> String {
        format!("{:013}-{:05}-{}", self.millis, self.counter, self.node)
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(3, '-');
        let millis = parts.next()?.parse().ok()?;
        let counter = parts.next()?.parse().ok()?;
        let node = parts.next()?;
        if node.is_empty() {
            return None;
        }
        Some(Hlc {
            millis,
            counter,
            node: node.to_string(),
        })
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        self.millis
            .cmp(&other.millis)
            .then(self.counter.cmp(&other.counter))
            .then_with(|| self.node.cmp(&other.node))
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl Serialize for Hlc {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encode())
    }
}

impl<'de> Deserialize<'de> for Hlc {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Hlc::decode(&raw).ok_or_else(|| D::Error::custom(format!("unreadable timestamp {raw:?}")))
    }
}

pub struct Clock {
    node: String,
    last: Mutex<(u64, u32)>,
}

impl Clock {
    pub fn new(node: impl Into<String>) -> Self {
        Clock {
            node: node.into(),
            last: Mutex::new((0, 0)),
        }
    }

    pub fn now(&self) -> Hlc {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut last = self.last.lock().expect("hlc clock poisoned");
        let (millis, counter) = if wall > last.0 {
            (wall, 0)
        } else {
            (last.0, last.1 + 1)
        };
        *last = (millis, counter);
        Hlc {
            millis,
            counter,
            node: self.node.clone(),
        }
    }

    pub fn observe(&self, remote: &Hlc) {
        let mut last = self.last.lock().expect("hlc clock poisoned");
        if (remote.millis, remote.counter) > *last {
            *last = (remote.millis, remote.counter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64, counter: u32, node: &str) -> Hlc {
        Hlc {
            millis,
            counter,
            node: node.to_string(),
        }
    }

    #[test]
    fn encoded_form_sorts_in_the_same_order_as_the_value() {
        let mut values = [
            at(2, 0, "node1"),
            at(1, 9, "node1"),
            at(1, 10, "node1"),
            at(1, 9, "node2"),
        ];
        values.sort();
        let mut encoded: Vec<String> = values.iter().map(Hlc::encode).collect();
        let sorted_by_value = encoded.clone();
        encoded.sort();
        assert_eq!(encoded, sorted_by_value);
    }

    #[test]
    fn decode_reverses_encode() {
        let value = at(1_758_800_000_000, 7, "node-with-dashes");
        assert_eq!(Hlc::decode(&value.encode()), Some(value));
    }

    #[test]
    fn decode_rejects_malformed_input() {
        assert_eq!(Hlc::decode("not-a-timestamp"), None);
        assert_eq!(Hlc::decode("1-2-"), None);
        assert_eq!(Hlc::decode("1"), None);
    }

    #[test]
    fn successive_reads_strictly_increase_within_the_same_millisecond() {
        let clock = Clock::new("node1");
        let first = clock.now();
        let second = clock.now();
        let third = clock.now();
        assert!(first < second);
        assert!(second < third);
    }

    #[test]
    fn observing_a_remote_timestamp_makes_the_next_local_read_exceed_it() {
        let clock = Clock::new("node1");
        let remote = at(u64::MAX / 2, 41, "node2");
        clock.observe(&remote);
        let next = clock.now();
        assert!(next > remote);
    }

    #[test]
    fn observing_an_older_remote_timestamp_does_not_rewind_the_clock() {
        let clock = Clock::new("node1");
        let ahead = clock.now();
        clock.observe(&at(1, 0, "node2"));
        assert!(clock.now() > ahead);
    }
}
