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

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const SUCCEEDED: &str = "succeeded";
pub const FAILED: &str = "failed";

pub trait Claimed: serde::Serialize + serde::de::DeserializeOwned + Send + Sync {
    fn id(&self) -> &str;
    fn is_running(&self) -> bool;
    fn claim(&self) -> &Claim;
    fn claim_mut(&mut self) -> &mut Claim;
}

pub fn upsert<T: Claimed>(records: &mut Vec<T>, record: T, keep: usize) {
    records.retain(|r| r.id() != record.id());
    records.insert(0, record);
    records.truncate(keep);
}

pub fn silent_for<T: Claimed>(record: &T) -> Duration {
    observed_silence(record.id(), &record.claim().heartbeat, Instant::now())
}

pub fn is_driven(record: &impl Claimed, silent: Duration) -> bool {
    record.is_running() && silent < STALE_AFTER
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Busy(String),
    Run(String),
    Nothing,
}

pub fn next<T: Claimed>(
    records: &[T],
    is_queued: impl Fn(&T) -> bool,
    silent: impl Fn(&T) -> Duration,
) -> Next {
    if let Some(live) = records.iter().find(|r| is_driven(*r, silent(r))) {
        return Next::Busy(live.id().to_string());
    }
    records
        .iter()
        .rev()
        .find(|r| r.is_running())
        .or_else(|| records.iter().rev().find(|r| is_queued(r)))
        .map_or(Next::Nothing, |r| Next::Run(r.id().to_string()))
}

pub fn take_in<T: Claimed + Clone>(
    records: &mut [T],
    id: &str,
    takeable: impl Fn(&T) -> bool,
    start: impl Fn(&mut T),
    claim: &Claim,
) -> Option<T> {
    let record = records.iter_mut().find(|r| r.id() == id)?;
    if !takeable(record) {
        return None;
    }
    start(record);
    *record.claim_mut() = claim.clone();
    Some(record.clone())
}

pub async fn take<T: Claimed + Clone>(
    store: crate::records::Store,
    client: &kube::Client,
    id: &str,
    takeable: impl Fn(&T) -> bool,
    start: impl Fn(&mut T),
) -> anyhow::Result<Option<T>> {
    let claim = Claim::mine(Utc::now());
    let taken = store
        .update(client, |records: &mut Vec<T>| {
            take_in(records, id, &takeable, &start, &claim)
        })
        .await?;
    Ok(taken)
}

pub async fn drive<T: Claimed, F: std::future::Future>(
    store: crate::records::Store,
    client: &kube::Client,
    id: &str,
    work: F,
) -> F::Output {
    tokio::select! {
        biased;
        out = work => out,
        never = beat_forever::<T>(store, client, id) => match never {},
    }
}

async fn beat_forever<T: Claimed>(
    store: crate::records::Store,
    client: &kube::Client,
    id: &str,
) -> std::convert::Infallible {
    let me = crate::system::hostname();
    let ids = [id.to_string()];
    loop {
        tokio::time::sleep(HEARTBEAT_EVERY).await;
        let written = store
            .update(client, |records: &mut Vec<T>| {
                beat(records, &ids, &me, Utc::now())
            })
            .await;
        if let Err(e) = written {
            tracing::warn!(
                "{id}: its heartbeat was not written: {e} — another machine takes it over after {}s",
                STALE_AFTER.as_secs()
            );
        }
    }
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

    const BEATS: crate::records::Store = crate::records::Store {
        name: "yolab-beat-test",
        namespace: "kube-system",
        key: "sets",
    };

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Job {
        id: String,
        state: String,
        #[serde(default)]
        attempts: u32,
        #[serde(flatten)]
        claim: Claim,
    }

    impl Claimed for Job {
        fn id(&self) -> &str {
            &self.id
        }
        fn is_running(&self) -> bool {
            self.state == RUNNING
        }
        fn claim(&self) -> &Claim {
            &self.claim
        }
        fn claim_mut(&mut self) -> &mut Claim {
            &mut self.claim
        }
    }

    fn job(id: &str, state: &str) -> Job {
        Job {
            id: id.into(),
            state: state.into(),
            attempts: 0,
            claim: claim("node1"),
        }
    }

    fn queued(j: &Job) -> bool {
        j.state == QUEUED
    }

    #[test]
    fn a_job_that_still_heartbeats_keeps_everything_else_waiting() {
        let jobs = [job("new", QUEUED), job("live", RUNNING)];
        assert_eq!(next(&jobs, queued, |_| QUIET), Next::Busy("live".into()));
    }

    #[test]
    fn a_job_whose_machine_went_quiet_is_run_again_before_anything_queued() {
        let jobs = [
            job("newer", QUEUED),
            job("stopped", RUNNING),
            job("older", QUEUED),
        ];
        assert_eq!(
            next(&jobs, queued, |_| SILENT),
            Next::Run("stopped".into())
        );
    }

    #[test]
    fn queued_jobs_run_oldest_first() {
        let jobs = [
            job("newest", QUEUED),
            job("done", SUCCEEDED),
            job("oldest", QUEUED),
        ];
        assert_eq!(next(&jobs, queued, |_| QUIET), Next::Run("oldest".into()));
    }

    #[test]
    fn finished_jobs_leave_nothing_to_run() {
        let jobs = [job("a", SUCCEEDED), job("b", FAILED)];
        assert_eq!(next(&jobs, queued, |_| SILENT), Next::Nothing);
    }

    #[test]
    fn taking_a_job_stamps_this_machine_and_applies_its_start() {
        let mut jobs = vec![job("a", QUEUED), job("b", QUEUED)];
        let mine = Claim {
            owner: "node2".into(),
            heartbeat: NOW.into(),
        };
        let taken = take_in(
            &mut jobs,
            "b",
            queued,
            |j| {
                j.state = RUNNING.into();
                j.attempts += 1;
            },
            &mine,
        )
        .expect("a queued job is takeable");
        assert_eq!(taken.claim, mine);
        assert_eq!(taken.attempts, 1);
        assert_eq!(jobs[1].state, RUNNING);
        assert_eq!(jobs[0].state, QUEUED, "only the named job is taken");
    }

    #[test]
    fn a_job_someone_else_took_first_is_not_taken_again() {
        let mut jobs = vec![job("a", RUNNING)];
        let mine = Claim::mine(at(NOW));
        let again = take_in(&mut jobs, "a", queued, |j| j.attempts += 1, &mine);
        assert!(again.is_none());
        assert_eq!(jobs[0].attempts, 0);
        assert_eq!(jobs[0].claim, claim("node1"));
        assert!(take_in(&mut jobs, "gone", queued, |_| {}, &mine).is_none());
    }

    #[tokio::test]
    async fn driving_a_job_returns_what_its_work_returned() {
        let unreachable = crate::k8s::testing::unreachable();
        let out = drive::<Job, _>(BEATS, &unreachable, "a", async { 7 }).await;
        assert_eq!(out, 7);
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
