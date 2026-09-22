
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);

pub const STALE_AFTER: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claim {
    pub owner: String,
    pub heartbeat: String,
}

impl Claim {
    pub fn mine(now: DateTime<Utc>) -> Self {
        Self {
            owner: crate::system::hostname(),
            heartbeat: now.to_rfc3339(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Driving,
    Remote,
    Abandoned,
}

impl Liveness {
    pub fn is_live(self) -> bool {
        matches!(self, Liveness::Driving | Liveness::Remote)
    }
}

pub fn liveness(claim: &Claim, me: &str, in_flight_here: bool, silent: Duration) -> Liveness {
    if in_flight_here {
        return Liveness::Driving;
    }
    if claim.owner == me {
        return Liveness::Abandoned;
    }
    if silent < STALE_AFTER {
        Liveness::Remote
    } else {
        Liveness::Abandoned
    }
}

const FORGET_AFTER: Duration = Duration::from_secs(24 * 3600);

type Seen = HashMap<String, (String, Instant, Instant)>;

pub fn observed_silence(id: &str, heartbeat: &str, now: Instant) -> Duration {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    silence_in(&mut seen, id, heartbeat, now)
}

fn silence_in(seen: &mut Seen, id: &str, heartbeat: &str, now: Instant) -> Duration {
    seen.retain(|_, (_, _, last_seen)| now.saturating_duration_since(*last_seen) < FORGET_AFTER);
    match seen.get_mut(id) {
        Some((beat, since, last_seen)) if beat == heartbeat => {
            *last_seen = now;
            now.saturating_duration_since(*since)
        }
        _ => {
            seen.insert(id.to_string(), (heartbeat.to_string(), now, now));
            Duration::ZERO
        }
    }
}

pub struct InFlight {
    ids: Mutex<Vec<String>>,
}

impl InFlight {
    pub const fn new() -> Self {
        Self {
            ids: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn claim(&'static self, id: &str) -> InFlightGuard {
        self.lock().push(id.to_string());
        InFlightGuard {
            set: self,
            id: id.to_string(),
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.lock().iter().any(|i| i == id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids = self.lock().clone();
        ids.sort();
        ids.dedup();
        ids
    }
}

pub struct InFlightGuard {
    set: &'static InFlight,
    id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut ids = self.set.lock();
        if let Some(pos) = ids.iter().position(|i| i == &self.id) {
            ids.swap_remove(pos);
        }
    }
}

pub trait Claimed: serde::Serialize + serde::de::DeserializeOwned + Send + Sync {
    fn id(&self) -> &str;
    fn is_running(&self) -> bool;
    fn claim(&self) -> &Claim;
    fn claim_mut(&mut self) -> &mut Claim;

    fn liveness(&self, me: &str, in_flight: &InFlight) -> Liveness {
        let claim = self.claim();
        let silent = observed_silence(self.id(), &claim.heartbeat, Instant::now());
        liveness(claim, me, in_flight.contains(self.id()), silent)
    }
}

pub fn spawn_heartbeat<T: Claimed + 'static>(
    store: crate::records::Store,
    in_flight: &'static InFlight,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT_EVERY).await;
            let ids = in_flight.ids();
            if ids.is_empty() {
                continue;
            }
            let me = crate::system::hostname();
            let now = Utc::now();
            let result = store
                .update(&crate::host::RealHost, |sets: &mut Vec<T>| {
                    beat(sets, &ids, &me, now);
                })
                .await;
            if let Err(e) = result {
                tracing::warn!(
                    "heartbeat for {}/{}: {e} — other nodes will treat these as abandoned after {}s",
                    store.namespace,
                    store.name,
                    STALE_AFTER.as_secs()
                );
            }
        }
    });
}

pub fn beat<T: Claimed>(sets: &mut [T], ids: &[String], me: &str, now: DateTime<Utc>) {
    for s in sets.iter_mut() {
        if s.is_running() && ids.iter().any(|i| i == s.id()) {
            let c = s.claim_mut();
            c.owner = me.to_string();
            c.heartbeat = now.to_rfc3339();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    const NOW: &str = "2026-09-14T12:00:00Z";
    const QUIET: Duration = Duration::ZERO;
    const SILENT: Duration = Duration::from_secs(300);

    fn claim(owner: &str) -> Claim {
        Claim {
            owner: owner.into(),
            heartbeat: "2026-09-14T11:59:30Z".into(),
        }
    }

    #[test]
    fn a_restore_driven_by_another_node_is_live_while_it_heartbeats() {
        let seen = liveness(&claim("node1"), "node2", false, Duration::from_secs(30));
        assert_eq!(seen, Liveness::Remote);
    }

    #[test]
    fn another_nodes_claim_is_abandoned_once_this_node_has_watched_it_go_quiet() {
        assert_eq!(
            liveness(&claim("node1"), "node2", false, SILENT),
            Liveness::Abandoned
        );
        let just_under = STALE_AFTER - Duration::from_secs(1);
        assert_eq!(
            liveness(&claim("node1"), "node2", false, just_under),
            Liveness::Remote
        );
    }

    #[test]
    fn our_own_claim_without_our_process_is_abandoned_immediately() {
        assert_eq!(
            liveness(&claim("node1"), "node1", false, QUIET),
            Liveness::Abandoned
        );
        assert_eq!(
            liveness(&claim("node1"), "node1", true, SILENT),
            Liveness::Driving
        );
    }

    #[test]
    fn silence_is_timed_from_the_last_change_this_process_saw() {
        let mut seen = Seen::new();
        let t0 = Instant::now();
        let s = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(silence_in(&mut seen, "rs-1", "h1", t0), QUIET);
        assert_eq!(
            silence_in(&mut seen, "rs-1", "h1", s(90)),
            Duration::from_secs(90)
        );
        assert_eq!(silence_in(&mut seen, "rs-1", "h2", s(100)), QUIET);
        assert_eq!(
            silence_in(&mut seen, "rs-1", "h2", s(130)),
            Duration::from_secs(30)
        );
        assert_eq!(silence_in(&mut seen, "rs-2", "h1", s(130)), QUIET);
    }

    #[test]
    fn ids_unseen_for_a_day_are_forgotten() {
        let mut seen = Seen::new();
        let t0 = Instant::now();
        silence_in(&mut seen, "old", "h", t0);
        let later = t0 + FORGET_AFTER + Duration::from_secs(1);
        silence_in(&mut seen, "new", "h", later);
        assert!(!seen.contains_key("old"));
        assert!(seen.contains_key("new"));
    }

    #[test]
    fn a_doubly_claimed_id_stays_in_flight_until_both_guards_drop() {
        static SET: InFlight = InFlight::new();
        let first = SET.claim("rs-1");
        let second = SET.claim("rs-1");
        let _other = SET.claim("rs-2");
        assert_eq!(SET.ids(), vec!["rs-1".to_string(), "rs-2".to_string()]);
        drop(first);
        assert!(SET.contains("rs-1"));
        drop(second);
        assert!(!SET.contains("rs-1"));
        assert!(SET.contains("rs-2"));
    }

    #[test]
    fn a_claim_is_released_even_when_the_driver_panics() {
        static SET: InFlight = InFlight::new();
        let r = std::panic::catch_unwind(|| {
            let _g = SET.claim("bk-1");
            assert!(SET.contains("bk-1"));
            panic!("driver died");
        });
        assert!(r.is_err());
        assert!(!SET.contains("bk-1"));
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Rec {
        id: String,
        state: String,
        #[serde(flatten)]
        claim: Claim,
    }

    impl Claimed for Rec {
        fn id(&self) -> &str {
            &self.id
        }
        fn is_running(&self) -> bool {
            self.state == "running"
        }
        fn claim(&self) -> &Claim {
            &self.claim
        }
        fn claim_mut(&mut self) -> &mut Claim {
            &mut self.claim
        }
    }

    fn rec(id: &str, state: &str, claim: Claim) -> Rec {
        Rec {
            id: id.into(),
            state: state.into(),
            claim,
        }
    }

    #[test]
    fn a_heartbeat_only_touches_running_records_this_process_drives() {
        let mut sets = vec![
            rec("a", "running", claim("node1")),
            rec("b", "running", claim("node2")),
            rec("c", "succeeded", claim("node1")),
        ];
        beat(&mut sets, &["a".into(), "c".into()], "node1", at(NOW));
        assert_eq!(sets[0].claim.heartbeat, at(NOW).to_rfc3339());
        assert_eq!(
            sets[1].claim,
            claim("node2"),
            "a record this process does not drive is not ours to stamp"
        );
        assert_eq!(
            sets[2].claim,
            claim("node1"),
            "a finished record needs no heartbeat"
        );
    }

    #[test]
    fn a_claim_is_part_of_every_record_and_required() {
        let r: Rec =
            serde_json::from_str(r#"{"id":"x","state":"running","owner":"n1","heartbeat":"h"}"#)
                .unwrap();
        assert_eq!(r.claim.owner, "n1");
        let unclaimed = serde_json::from_str::<Rec>(r#"{"id":"x","state":"running"}"#);
        assert!(
            unclaimed.is_err(),
            "a record without its claim does not parse"
        );
    }

    #[test]
    fn a_new_claim_names_this_node_and_the_time() {
        let c = Claim::mine(at(NOW));
        assert_eq!(c.owner, crate::system::hostname());
        assert_eq!(c.heartbeat, at(NOW).to_rfc3339());
    }
}
