//! Who is driving a long-running operation, and whether they still are.
//!
//! THE CROSS-NODE BUG THIS FIXES
//!
//! Backups and restores are recorded in ConfigMaps every node reads, but "is this
//! one still running?" was answered from a `static IN_FLIGHT: Mutex<Vec<String>>`
//! — memory of ONE process. So on a two-node cluster a restore driven by node1
//! was, to node2, "recorded running but not in flight": the definition of
//! crashed. node2's watchdog (which ran on every node) would scale the app back
//! up in the middle of node1 replacing its volumes, and mark the restore failed.
//! The backup scheduler, also on every node, would not see node1's backup as
//! running and start a second one.
//!
//! A record now carries a `Claim`: which node drives it and when that node last
//! said so. The driving process refreshes the heartbeat while it works. Anyone can
//! then tell the three cases apart:
//!
//!   - `Driving` — this process has it in hand.
//!   - `Remote`  — another node claims it and its heartbeat is fresh.
//!   - `Abandoned` — this node claims it but this process does not (local-api
//!     restarted mid-operation), or the claimant's heartbeat has gone stale (the
//!     node died).
//!
//! Only `Abandoned` may be cleaned up.
//!
//! STALE MEANS "THIS PROCESS HAS WATCHED IT NOT CHANGE", NOT "ITS TIMESTAMP IS OLD".
//! The heartbeat is written with the claimant's clock. Reading it against ours
//! made a node whose clock ran ahead see every live restore as abandoned — and
//! after an API outage longer than `STALE_AFTER`, when no heartbeat could land,
//! every node did. The watchdog would then scale an app back up in the middle of
//! its live restore. So staleness is timed on this process's monotonic clock,
//! from the moment it last saw the heartbeat value change (`observed_silence`):
//! skew cannot shorten it, and an outage restarts the count once the records
//! can be read again.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How often a driving process refreshes its claims.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);

/// A claim whose heartbeat is older than this is abandoned. Several missed
/// heartbeats, so a slow API call or a GC pause is never mistaken for death.
pub const STALE_AFTER: Duration = Duration::from_secs(120);

/// Records written before claims existed have no owner. They are left alone for
/// this long after they started — enough to outlast any restore or backup an
/// old-version node could still be driving during a rolling update — and only
/// then treated as abandoned.
pub const LEGACY_GRACE: Duration = Duration::from_secs(3 * 3600);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claim {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

impl Claim {
    pub fn mine(now: DateTime<Utc>) -> Self {
        Self {
            owner: crate::system::hostname(),
            heartbeat: Some(now.to_rfc3339()),
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
    /// Still being worked on by someone.
    pub fn is_live(self) -> bool {
        matches!(self, Liveness::Driving | Liveness::Remote)
    }
}

/// Classifies a `running` record. Pure: `me` is this node, `in_flight_here`
/// whether this process holds the id, `silent` how long this process has watched
/// the claim's heartbeat stay unchanged (see the module header).
pub fn liveness(
    claim: &Claim,
    started_at: Option<&str>,
    me: &str,
    in_flight_here: bool,
    now: DateTime<Utc>,
    silent: Duration,
) -> Liveness {
    if in_flight_here {
        return Liveness::Driving;
    }
    if claim.owner.is_empty() {
        let started = started_at
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        return match started {
            Some(t) if age(now, t) < LEGACY_GRACE => Liveness::Remote,
            _ => Liveness::Abandoned,
        };
    }
    if claim.owner == me {
        // This node's own claim, and this process does not hold it: the process
        // that did is gone.
        return Liveness::Abandoned;
    }
    if silent < STALE_AFTER {
        Liveness::Remote
    } else {
        Liveness::Abandoned
    }
}

/// Forgotten after this long unseen, so the map cannot grow for the life of the
/// process. Far longer than any record stays `running`.
const FORGET_AFTER: Duration = Duration::from_secs(24 * 3600);

type Seen = HashMap<String, (Option<String>, Instant, Instant)>;

/// How long this process has seen `id`'s heartbeat sit at `heartbeat`. Zero the
/// first time it is seen and whenever the value changes.
pub fn observed_silence(id: &str, heartbeat: Option<&str>, now: Instant) -> Duration {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    silence_in(&mut seen, id, heartbeat, now)
}

fn silence_in(seen: &mut Seen, id: &str, heartbeat: Option<&str>, now: Instant) -> Duration {
    seen.retain(|_, (_, _, last_seen)| now.saturating_duration_since(*last_seen) < FORGET_AFTER);
    match seen.get_mut(id) {
        Some((beat, since, last_seen)) if beat.as_deref() == heartbeat => {
            *last_seen = now;
            now.saturating_duration_since(*since)
        }
        _ => {
            seen.insert(id.to_string(), (heartbeat.map(str::to_string), now, now));
            Duration::ZERO
        }
    }
}

fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    (now - then).to_std().unwrap_or(Duration::ZERO)
}

/// The ids one kind of operation has in flight in THIS process. A registry
/// rather than a bare static per module, so each operation kind gets the same
/// guard semantics.
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

    /// Claims `id` for this process until the guard drops — including when the
    /// driving task panics, which the old push/retain pairs did not survive.
    ///
    /// Counted: each guard holds one entry, so a second claim of the same id keeps
    /// it in flight until BOTH guards drop.
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

    /// Each id once, however many guards hold it.
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

/// A record whose liveness is tracked by a `Claim`.
pub trait Claimed: serde::Serialize + serde::de::DeserializeOwned + Send + Sync {
    fn id(&self) -> &str;
    fn started_at(&self) -> &str;
    /// Recorded as still running (not yet succeeded or failed).
    fn is_running(&self) -> bool;
    fn claim(&self) -> &Claim;
    fn claim_mut(&mut self) -> &mut Claim;

    fn liveness(&self, me: &str, in_flight: &InFlight, now: DateTime<Utc>) -> Liveness {
        let silent = observed_silence(self.id(), self.claim().heartbeat.as_deref(), Instant::now());
        liveness(
            self.claim(),
            Some(self.started_at()),
            me,
            in_flight.contains(self.id()),
            now,
            silent,
        )
    }
}

/// Refreshes this node's claims on every record this process is driving, every
/// `HEARTBEAT_EVERY`, for as long as the process lives. One task per record
/// kind; a single compare-and-swap covers all of its ids.
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

/// Pure half of the heartbeat: stamp `now` on the running records in `ids`,
/// claiming them for `me`.
pub fn beat<T: Claimed>(sets: &mut [T], ids: &[String], me: &str, now: DateTime<Utc>) {
    for s in sets.iter_mut() {
        if s.is_running() && ids.iter().any(|i| i == s.id()) {
            let c = s.claim_mut();
            c.owner = me.to_string();
            c.heartbeat = Some(now.to_rfc3339());
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

    fn claim(owner: &str, beat: Option<&str>) -> Claim {
        Claim {
            owner: owner.into(),
            heartbeat: beat.map(str::to_string),
        }
    }

    const QUIET: Duration = Duration::ZERO;
    const SILENT: Duration = Duration::from_secs(300);

    #[test]
    fn a_restore_driven_by_another_node_is_not_crashed_while_it_heartbeats() {
        // The exact bug: node2 looking at node1's live restore.
        let c = claim("node1", Some("2026-09-14T11:59:30Z"));
        let seen = liveness(&c, None, "node2", false, at(NOW), Duration::from_secs(30));
        assert_eq!(seen, Liveness::Remote);
    }

    #[test]
    fn a_dead_node_claim_is_abandoned_once_this_node_has_watched_it_go_quiet() {
        let c = claim("node1", Some("2026-09-14T11:55:00Z"));
        let seen = liveness(&c, None, "node2", false, at(NOW), SILENT);
        assert_eq!(seen, Liveness::Abandoned);
    }

    /// node2's clock runs an hour ahead, or the API was down for an hour: the
    /// heartbeat TIMESTAMP looks ancient, but this process has only just seen it.
    #[test]
    fn an_old_looking_heartbeat_is_not_stale_until_it_has_been_watched_that_long() {
        let c = claim("node1", Some("2026-09-14T11:00:00Z"));
        let seen = liveness(&c, None, "node2", false, at(NOW), QUIET);
        assert_eq!(seen, Liveness::Remote);
    }

    #[test]
    fn our_own_claim_without_our_process_is_abandoned_immediately() {
        // local-api restarted mid-restore on this very node.
        let c = claim("node1", Some("2026-09-14T11:59:59Z"));
        let restarted = liveness(&c, None, "node1", false, at(NOW), QUIET);
        assert_eq!(restarted, Liveness::Abandoned);
        let driving = liveness(&c, None, "node1", true, at(NOW), SILENT);
        assert_eq!(driving, Liveness::Driving);
    }

    #[test]
    fn a_claim_without_a_heartbeat_goes_stale_like_any_other() {
        let c = claim("node1", None);
        let fresh = liveness(&c, None, "node2", false, at(NOW), QUIET);
        assert_eq!(fresh, Liveness::Remote);
        let stale = liveness(&c, None, "node2", false, at(NOW), SILENT);
        assert_eq!(stale, Liveness::Abandoned);
    }

    #[test]
    fn a_legacy_record_is_left_alone_during_a_rolling_update() {
        let c = Claim::default();
        let recent = liveness(
            &c,
            Some("2026-09-14T11:00:00Z"),
            "node2",
            false,
            at(NOW),
            QUIET,
        );
        assert_eq!(recent, Liveness::Remote);
        let old = liveness(
            &c,
            Some("2026-09-14T06:00:00Z"),
            "node2",
            false,
            at(NOW),
            QUIET,
        );
        assert_eq!(old, Liveness::Abandoned);
        let undated = liveness(&c, None, "node2", false, at(NOW), QUIET);
        assert_eq!(undated, Liveness::Abandoned);
    }

    #[test]
    fn silence_is_timed_from_the_last_change_this_process_saw() {
        let mut seen = Seen::new();
        let t0 = Instant::now();
        let s = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(silence_in(&mut seen, "rs-1", Some("h1"), t0), QUIET);
        assert_eq!(
            silence_in(&mut seen, "rs-1", Some("h1"), s(90)),
            Duration::from_secs(90)
        );
        // A new heartbeat value restarts the count.
        assert_eq!(silence_in(&mut seen, "rs-1", Some("h2"), s(100)), QUIET);
        assert_eq!(
            silence_in(&mut seen, "rs-1", Some("h2"), s(130)),
            Duration::from_secs(30)
        );
        // Ids are timed independently.
        assert_eq!(silence_in(&mut seen, "rs-2", None, s(130)), QUIET);
    }

    #[test]
    fn ids_unseen_for_a_day_are_forgotten() {
        let mut seen = Seen::new();
        let t0 = Instant::now();
        silence_in(&mut seen, "old", Some("h"), t0);
        silence_in(
            &mut seen,
            "new",
            Some("h"),
            t0 + FORGET_AFTER + Duration::from_secs(1),
        );
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
        started_at: String,
        state: String,
        #[serde(flatten)]
        claim: Claim,
    }

    impl Claimed for Rec {
        fn id(&self) -> &str {
            &self.id
        }
        fn started_at(&self) -> &str {
            &self.started_at
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

    #[test]
    fn a_heartbeat_only_touches_running_records_this_process_drives() {
        let mut sets = vec![
            Rec {
                id: "a".into(),
                started_at: NOW.into(),
                state: "running".into(),
                claim: Claim::default(),
            },
            Rec {
                id: "b".into(),
                started_at: NOW.into(),
                state: "running".into(),
                claim: claim("node2", Some("2026-09-14T11:59:59Z")),
            },
            Rec {
                id: "c".into(),
                started_at: NOW.into(),
                state: "succeeded".into(),
                claim: Claim::default(),
            },
        ];
        beat(&mut sets, &["a".into(), "c".into()], "node1", at(NOW));
        assert_eq!(sets[0].claim.owner, "node1");
        assert_eq!(
            sets[0].claim.heartbeat.as_deref(),
            Some(at(NOW).to_rfc3339().as_str())
        );
        assert_eq!(
            sets[1].claim.owner, "node2",
            "another node's record is not ours to stamp"
        );
        assert!(
            sets[2].claim.owner.is_empty(),
            "a finished record needs no heartbeat"
        );
    }

    #[test]
    fn a_flattened_claim_round_trips_inside_a_record() {
        let r: Rec = serde_json::from_str(
            r#"{"id":"x","started_at":"t","state":"running","owner":"n1","heartbeat":"h"}"#,
        )
        .unwrap();
        assert_eq!(r.claim.owner, "n1");
        let legacy: Rec =
            serde_json::from_str(r#"{"id":"x","started_at":"t","state":"running"}"#).unwrap();
        assert_eq!(legacy.claim, Claim::default());
    }

    #[test]
    fn claims_serialize_compactly_and_legacy_records_parse() {
        let v: Claim = serde_json::from_str("{}").unwrap();
        assert_eq!(v, Claim::default());
        assert_eq!(serde_json::to_string(&Claim::default()).unwrap(), "{}");
    }
}
