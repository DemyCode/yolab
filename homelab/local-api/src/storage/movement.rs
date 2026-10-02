use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::Host;
use crate::storage::settings;

pub const RECORD_KEY: &str = "yolab/movement";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Settled,
    Working,
    Unavailable,
    NoRoom,
    Restarting,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Move,
    AddCopies,
    Rebuild,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskRef {
    pub node: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Job {
    pub kind: JobKind,
    pub remaining_bytes: u64,
    pub to_move_bytes: u64,
    pub moved_bytes: u64,
    pub progress: Option<f64>,
    pub eta_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Movement {
    pub state: State,
    pub jobs: Vec<Job>,
    pub draining: Vec<DiskRef>,
    pub waiting_for: Vec<String>,
    pub copies: Option<u32>,
    pub eta_secs: Option<u64>,
    pub inactive_pgs: u64,
    pub total_pgs: u64,
}

impl Movement {
    fn unknown() -> Self {
        Movement {
            state: State::Unknown,
            jobs: Vec::new(),
            draining: Vec::new(),
            waiting_for: Vec::new(),
            copies: None,
            eta_secs: None,
            inactive_pgs: 0,
            total_pgs: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub misplaced_bytes: u64,
    pub degraded_bytes: u64,
    pub inactive_pgs: u64,
    pub total_pgs: u64,
    pub no_room: bool,
    pub noout: bool,
    pub osds_down: bool,
}

fn bytes_of(objects: u64, data_bytes: u64, num_objects: u64) -> u64 {
    if num_objects == 0 {
        return 0;
    }
    (objects as f64 * data_bytes as f64 / num_objects as f64).round() as u64
}

pub fn snapshot(status: &Value, osd_flags: &str) -> Snapshot {
    let pgmap = &status["pgmap"];
    let states: Vec<(&str, u64)> = pgmap["pgs_by_state"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| Some((s["state_name"].as_str()?, s["count"].as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    let has = |part: &str| states.iter().any(|(name, _)| name.split('+').any(|p| p == part));
    let inactive_pgs = states
        .iter()
        .filter(|(name, _)| !name.split('+').any(|p| p == "active"))
        .map(|(_, n)| n)
        .sum();
    let data_bytes = pgmap["data_bytes"].as_u64().unwrap_or(0);
    let num_objects = pgmap["num_objects"].as_u64().unwrap_or(0);
    let osdmap = &status["osdmap"];
    let num_osds = osdmap["num_osds"].as_u64().unwrap_or(0);
    let num_up = osdmap["num_up_osds"].as_u64().unwrap_or(num_osds);
    Snapshot {
        misplaced_bytes: bytes_of(
            pgmap["misplaced_objects"].as_u64().unwrap_or(0),
            data_bytes,
            num_objects,
        ),
        degraded_bytes: bytes_of(
            pgmap["degraded_objects"].as_u64().unwrap_or(0),
            data_bytes,
            num_objects,
        ),
        inactive_pgs,
        total_pgs: pgmap["num_pgs"].as_u64().unwrap_or(0),
        no_room: has("backfill_toofull") || has("recovery_toofull"),
        noout: osd_flags.split(',').any(|f| f == "noout"),
        osds_down: num_up < num_osds,
    }
}

pub fn remaining_by_job(s: &Snapshot) -> BTreeMap<JobKind, u64> {
    let mut jobs = BTreeMap::new();
    if s.misplaced_bytes > 0 {
        jobs.insert(JobKind::Move, s.misplaced_bytes);
    }
    if s.degraded_bytes > 0 {
        let kind = if s.osds_down {
            JobKind::Rebuild
        } else {
            JobKind::AddCopies
        };
        jobs.insert(kind, s.degraded_bytes);
    }
    jobs
}

pub fn state_of(s: &Snapshot) -> State {
    if s.inactive_pgs > 0 {
        State::Unavailable
    } else if s.no_room {
        State::NoRoom
    } else if s.noout && s.osds_down {
        State::Restarting
    } else if s.misplaced_bytes > 0 || s.degraded_bytes > 0 {
        State::Working
    } else {
        State::Settled
    }
}

const WINDOW_MS: u64 = 15 * 60 * 1000;
const MIN_SPAN_MS: u64 = 60 * 1000;
const MAX_SAMPLES: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracker {
    pub peak: u64,
    pub samples: VecDeque<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
pub struct Estimate {
    pub to_move: u64,
    pub moved: u64,
    pub progress: Option<f64>,
    pub eta_secs: Option<u64>,
}

impl Tracker {
    pub fn record(&mut self, now_ms: u64, remaining: u64) {
        self.peak = self.peak.max(remaining);
        self.samples.push_back((now_ms, remaining));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now_ms.saturating_sub(*t) > WINDOW_MS)
            || self.samples.len() > MAX_SAMPLES
        {
            self.samples.pop_front();
        }
    }

    pub fn estimate(&self, now_ms: u64, remaining: u64) -> Estimate {
        let peak = self.peak.max(remaining);
        let moved = peak - remaining;
        Estimate {
            to_move: peak,
            moved,
            progress: (peak > 0).then(|| moved as f64 / peak as f64),
            eta_secs: self.eta(now_ms, remaining),
        }
    }

    fn eta(&self, now_ms: u64, remaining: u64) -> Option<u64> {
        let &(t0, r0) = self
            .samples
            .iter()
            .find(|(t, _)| now_ms.saturating_sub(*t) <= WINDOW_MS)?;
        let span = now_ms.saturating_sub(t0);
        if span < MIN_SPAN_MS || r0 <= remaining {
            return None;
        }
        let rate_per_ms = (r0 - remaining) as f64 / span as f64;
        Some((remaining as f64 / rate_per_ms / 1000.0).round() as u64)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub jobs: BTreeMap<JobKind, Tracker>,
}

impl Record {
    pub fn advance(&mut self, now_ms: u64, remaining: &BTreeMap<JobKind, u64>) {
        self.jobs.retain(|kind, _| remaining.contains_key(kind));
        for (&kind, &left) in remaining {
            self.jobs.entry(kind).or_default().record(now_ms, left);
        }
    }

    pub fn jobs(&self, now_ms: u64, remaining: &BTreeMap<JobKind, u64>) -> Vec<Job> {
        remaining
            .iter()
            .map(|(&kind, &left)| {
                let e = match self.jobs.get(&kind) {
                    Some(t) => t.estimate(now_ms, left),
                    None => Estimate {
                        to_move: left,
                        moved: 0,
                        progress: None,
                        eta_secs: None,
                    },
                };
                Job {
                    kind,
                    remaining_bytes: left,
                    to_move_bytes: e.to_move,
                    moved_bytes: e.moved,
                    progress: e.progress,
                    eta_secs: e.eta_secs,
                }
            })
            .collect()
    }
}

pub fn overall_eta(jobs: &[Job]) -> Option<u64> {
    if jobs.is_empty() {
        return None;
    }
    jobs.iter()
        .map(|j| j.eta_secs)
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
}

pub fn draining_disks(published: &BTreeMap<String, String>) -> Vec<DiskRef> {
    let mut out: Vec<DiskRef> = published
        .iter()
        .flat_map(|(node, raw)| {
            let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            payload["disks"]
                .as_object()
                .map(|disks| {
                    disks
                        .iter()
                        .filter(|(_, d)| {
                            matches!(d["phase"].as_str(), Some("draining") | Some("removing"))
                        })
                        .map(|(id, d)| DiskRef {
                            node: node.clone(),
                            name: d["model"]
                                .as_str()
                                .filter(|m| !m.is_empty())
                                .unwrap_or(id)
                                .to_string(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    out.sort_by(|a, b| (&a.node, &a.name).cmp(&(&b.node, &b.name)));
    out
}

pub fn hosts_with_down_osds(tree: &Value) -> Vec<String> {
    let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
    let down: std::collections::HashSet<i64> = nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd") && n["status"].as_str() == Some("down"))
        .filter_map(|n| n["id"].as_i64())
        .collect();
    let mut hosts: Vec<String> = nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("host"))
        .filter(|h| {
            h["children"]
                .as_array()
                .is_some_and(|c| c.iter().filter_map(Value::as_i64).any(|id| down.contains(&id)))
        })
        .filter_map(|h| h["name"].as_str().map(str::to_string))
        .collect();
    hosts.sort();
    hosts
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn read_snapshot<H: Host>(host: &H) -> Option<Snapshot> {
    let status = host.ceph_json(&["status"]).await.ok()?;
    let flags = host
        .ceph_json(&["osd", "dump"])
        .await
        .ok()
        .and_then(|d| d["flags"].as_str().map(str::to_string))
        .unwrap_or_default();
    Some(snapshot(&status, &flags))
}

async fn read_record<H: Host>(host: &H) -> Record {
    settings::get(host, RECORD_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub async fn sample<H: Host>(host: &H) {
    let Some(snap) = read_snapshot(host).await else {
        return;
    };
    let remaining = remaining_by_job(&snap);
    let mut record = read_record(host).await;
    if remaining.is_empty() && record.jobs.is_empty() {
        return;
    }
    record.advance(now_ms(), &remaining);
    match serde_json::to_string(&record) {
        Ok(raw) => {
            if let Err(e) = settings::set(host, RECORD_KEY, &raw).await {
                tracing::debug!("movement: could not save the progress record ({e})");
            }
        }
        Err(e) => tracing::warn!("movement: could not encode the progress record ({e})"),
    }
}

pub async fn assess_via<H: Host>(host: &H) -> Movement {
    let Some(snap) = read_snapshot(host).await else {
        return Movement::unknown();
    };
    let remaining = remaining_by_job(&snap);
    let jobs = read_record(host).await.jobs(now_ms(), &remaining);
    let waiting_for = if snap.osds_down {
        host.ceph_json(&["osd", "tree"])
            .await
            .map(|t| hosts_with_down_osds(&t))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let draining = settings::dump(host, settings::DISK_STATUS)
        .await
        .map(|p| draining_disks(&p))
        .unwrap_or_default();
    let copies = match crate::topology::read_policy_from(host).await {
        Some(crate::topology::PolicyState::Chosen(p)) => Some(p.size),
        _ => None,
    };
    Movement {
        state: state_of(&snap),
        eta_secs: overall_eta(&jobs),
        jobs,
        draining,
        waiting_for,
        copies,
        inactive_pgs: snap.inactive_pgs,
        total_pgs: snap.total_pgs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_node3_status() -> Value {
        json!({
            "pgmap": {
                "pgs_by_state": [
                    {"state_name": "active+remapped+backfill_wait", "count": 52},
                    {"state_name": "active+clean", "count": 24},
                    {"state_name": "active+remapped+backfilling", "count": 5}
                ],
                "num_pgs": 81,
                "num_objects": 590282,
                "data_bytes": 121233767008u64,
                "misplaced_objects": 471699,
                "misplaced_total": 590282,
                "misplaced_ratio": 0.7991078840283119
            },
            "osdmap": {"num_osds": 3, "num_up_osds": 3, "num_in_osds": 2}
        })
    }

    fn drain_and_second_copy() -> Value {
        json!({
            "pgmap": {
                "pgs_by_state": [
                    {"state_name": "active+undersized+degraded+remapped+backfilling", "count": 40},
                    {"state_name": "active+clean", "count": 41}
                ],
                "num_pgs": 81,
                "num_objects": 1000,
                "data_bytes": 100_000u64,
                "misplaced_objects": 400,
                "misplaced_total": 2000,
                "misplaced_ratio": 0.2,
                "degraded_objects": 900,
                "degraded_total": 2000,
                "degraded_ratio": 0.45
            },
            "osdmap": {"num_osds": 3, "num_up_osds": 3}
        })
    }

    fn left(pairs: &[(JobKind, u64)]) -> BTreeMap<JobKind, u64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn the_drain_seen_on_node3_is_one_move_job_with_everything_usable() {
        let s = snapshot(&live_node3_status(), "sortbitwise,recovery_deletes");
        assert_eq!(state_of(&s), State::Working);
        assert_eq!(s.inactive_pgs, 0);
        let jobs = remaining_by_job(&s);
        assert_eq!(jobs.keys().copied().collect::<Vec<_>>(), vec![JobKind::Move]);
        let mv = jobs[&JobKind::Move];
        assert!(mv > 96_000_000_000 && mv < 97_000_000_000);
    }

    #[test]
    fn a_drain_and_a_raised_copy_count_are_two_separate_jobs() {
        let jobs = remaining_by_job(&snapshot(&drain_and_second_copy(), ""));
        assert_eq!(jobs.get(&JobKind::Move), Some(&40_000));
        assert_eq!(jobs.get(&JobKind::AddCopies), Some(&90_000));
        assert!(!jobs.contains_key(&JobKind::Rebuild));
    }

    #[test]
    fn bytes_come_from_object_counts_not_from_ratios_over_copies() {
        let s = snapshot(&drain_and_second_copy(), "");
        assert_eq!(
            s.misplaced_bytes, 40_000,
            "400 of 1000 objects is 40% of the data, although the ratio over copies says 20%"
        );
    }

    #[test]
    fn missing_copies_while_a_disk_is_down_are_a_rebuild() {
        let mut status = drain_and_second_copy();
        status["osdmap"]["num_up_osds"] = json!(2);
        let jobs = remaining_by_job(&snapshot(&status, ""));
        assert!(jobs.contains_key(&JobKind::Rebuild));
        assert!(!jobs.contains_key(&JobKind::AddCopies));
    }

    #[test]
    fn a_pg_that_is_not_active_makes_files_unavailable() {
        let status = json!({"pgmap": {"pgs_by_state": [
            {"state_name": "active+clean", "count": 70},
            {"state_name": "down", "count": 3},
            {"state_name": "peering", "count": 2}
        ]}});
        let s = snapshot(&status, "");
        assert_eq!(s.inactive_pgs, 5);
        assert_eq!(state_of(&s), State::Unavailable);
    }

    #[test]
    fn unavailable_outranks_every_other_state() {
        let s = Snapshot {
            inactive_pgs: 1,
            misplaced_bytes: 1,
            degraded_bytes: 1,
            no_room: true,
            noout: true,
            osds_down: true,
            ..Default::default()
        };
        assert_eq!(state_of(&s), State::Unavailable);
    }

    #[test]
    fn a_full_backfill_target_is_reported_as_no_room() {
        let status = json!({"pgmap": {"pgs_by_state": [
            {"state_name": "active+remapped+backfill_toofull", "count": 4}
        ], "misplaced_objects": 10, "num_objects": 100, "data_bytes": 1000}});
        assert_eq!(state_of(&snapshot(&status, "")), State::NoRoom);
    }

    #[test]
    fn a_machine_restarting_under_noout_is_not_called_working() {
        let status = json!({
            "pgmap": {"pgs_by_state": [{"state_name": "active+undersized+degraded", "count": 9}],
                      "degraded_objects": 100, "num_objects": 200, "data_bytes": 1000},
            "osdmap": {"num_osds": 3, "num_up_osds": 2}
        });
        assert_eq!(
            state_of(&snapshot(&status, "noout,sortbitwise")),
            State::Restarting
        );
        assert_eq!(state_of(&snapshot(&status, "sortbitwise")), State::Working);
    }

    #[test]
    fn a_clean_cluster_is_settled_with_no_jobs() {
        let status = json!({"pgmap": {"pgs_by_state": [{"state_name": "active+clean", "count": 81}],
                                      "num_objects": 10, "data_bytes": 1000}});
        let s = snapshot(&status, "");
        assert_eq!(state_of(&s), State::Settled);
        assert!(remaining_by_job(&s).is_empty());
    }

    #[test]
    fn progress_is_measured_from_the_peak_recorded_when_the_job_began() {
        let mut r = Record::default();
        r.advance(0, &left(&[(JobKind::Move, 1000)]));
        let jobs = r.jobs(30_000, &left(&[(JobKind::Move, 750)]));
        assert_eq!(jobs[0].to_move_bytes, 1000);
        assert_eq!(jobs[0].moved_bytes, 250);
        assert_eq!(jobs[0].progress, Some(0.25));
    }

    #[test]
    fn a_second_job_starting_does_not_reset_the_first() {
        let mut r = Record::default();
        r.advance(0, &left(&[(JobKind::Move, 1000)]));
        r.advance(
            120_000,
            &left(&[(JobKind::Move, 800), (JobKind::AddCopies, 5000)]),
        );
        let jobs = r.jobs(
            180_000,
            &left(&[(JobKind::Move, 700), (JobKind::AddCopies, 4900)]),
        );
        let mv = jobs.iter().find(|j| j.kind == JobKind::Move).unwrap();
        assert_eq!(mv.to_move_bytes, 1000);
        assert_eq!(mv.moved_bytes, 300);
        assert_eq!(mv.eta_secs, Some(420));
    }

    #[test]
    fn a_finished_job_is_forgotten_and_starts_from_zero_next_time() {
        let mut r = Record::default();
        r.advance(0, &left(&[(JobKind::Move, 1000)]));
        r.advance(60_000, &left(&[]));
        assert!(r.jobs.is_empty());
        r.advance(120_000, &left(&[(JobKind::Move, 300)]));
        let jobs = r.jobs(120_000, &left(&[(JobKind::Move, 300)]));
        assert_eq!(jobs[0].to_move_bytes, 300);
        assert_eq!(jobs[0].moved_bytes, 0);
    }

    #[test]
    fn a_job_nobody_recorded_yet_has_no_progress_or_time() {
        let jobs = Record::default().jobs(0, &left(&[(JobKind::AddCopies, 500)]));
        assert_eq!(jobs[0].progress, None);
        assert_eq!(jobs[0].eta_secs, None);
        assert_eq!(jobs[0].to_move_bytes, 500);
    }

    #[test]
    fn no_time_is_promised_before_a_minute_of_samples() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.estimate(59_000, 900).eta_secs, None);
        assert!(t.estimate(60_000, 900).eta_secs.is_some());
    }

    #[test]
    fn the_time_left_follows_the_observed_rate() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.estimate(100_000, 500).eta_secs, Some(100));
    }

    #[test]
    fn a_stalled_job_gets_no_time_left() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.estimate(120_000, 1000).eta_secs, None);
    }

    #[test]
    fn samples_older_than_the_window_stop_steering_the_rate() {
        let mut t = Tracker::default();
        t.record(0, 10_000);
        t.record(WINDOW_MS - 60_000, 2_000);
        assert_eq!(t.estimate(WINDOW_MS + 60_000, 1_000).eta_secs, Some(120));
    }

    #[test]
    fn the_record_stays_small() {
        let mut t = Tracker::default();
        for i in 0..100 {
            t.record(i * 1000, 10_000 - i);
        }
        assert!(t.samples.len() <= MAX_SAMPLES);
    }

    #[test]
    fn the_record_survives_a_round_trip_through_the_config_store() {
        let mut r = Record::default();
        r.advance(5, &left(&[(JobKind::Move, 10), (JobKind::AddCopies, 20)]));
        let raw = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&raw).unwrap(), r);
    }

    #[test]
    fn the_whole_reorganisation_takes_as_long_as_its_slowest_job() {
        let job = |eta| Job {
            kind: JobKind::Move,
            remaining_bytes: 1,
            to_move_bytes: 1,
            moved_bytes: 0,
            progress: None,
            eta_secs: eta,
        };
        assert_eq!(overall_eta(&[job(Some(60)), job(Some(600))]), Some(600));
        assert_eq!(overall_eta(&[job(Some(60)), job(None)]), None);
        assert_eq!(overall_eta(&[]), None);
    }

    #[test]
    fn the_disk_being_drained_is_named_by_its_model_and_machine() {
        let published = BTreeMap::from([
            (
                "node2".to_string(),
                json!({"disks": {
                    "serial-wwn-0x50014ee214caf529": {"model": "easystore 2647", "phase": "draining"},
                    "system": {"model": "System disk", "phase": "active"}
                }})
                .to_string(),
            ),
            (
                "node3".to_string(),
                json!({"disks": {"system": {"model": "System disk", "phase": "active"}}})
                    .to_string(),
            ),
        ]);
        assert_eq!(
            draining_disks(&published),
            vec![DiskRef {
                node: "node2".into(),
                name: "easystore 2647".into()
            }]
        );
    }

    #[test]
    fn a_machine_is_named_when_one_of_its_disks_is_down() {
        let tree = json!({"nodes": [
            {"id": -3, "type": "host", "name": "node3", "children": [0]},
            {"id": -5, "type": "host", "name": "node2", "children": [1, 2]},
            {"id": 0, "type": "osd", "status": "up"},
            {"id": 1, "type": "osd", "status": "down"},
            {"id": 2, "type": "osd", "status": "up"}
        ]});
        assert_eq!(hosts_with_down_osds(&tree), vec!["node2".to_string()]);
    }
}
