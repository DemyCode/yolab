use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::Host;
use crate::storage::settings;

pub const RECORD_KEY: &str = "yolab/movement";

const UNAVAILABLE_AFTER_MS: u64 = 45 * 1000;
const CLONE_SCALE: u64 = 10_000;

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
    Clone,
    FreeSpace,
    Repair,
}

impl JobKind {
    fn unit(self) -> Unit {
        match self {
            JobKind::Move | JobKind::AddCopies | JobKind::Rebuild => Unit::Bytes,
            JobKind::Clone | JobKind::FreeSpace | JobKind::Repair => Unit::Percent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Bytes,
    Percent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveReason {
    Draining,
    Filling,
    Resizing,
    Balancing,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskRef {
    pub node: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Job {
    pub kind: JobKind,
    pub unit: Unit,
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
    pub move_reason: Option<MoveReason>,
    pub draining: Vec<DiskRef>,
    pub filling: Vec<DiskRef>,
    pub waiting_for: Vec<String>,
    pub copies: Option<u32>,
    pub clones: u32,
    pub repairing: bool,
    pub eta_secs: Option<u64>,
    pub inactive_pgs: u64,
    pub total_pgs: u64,
}

impl Movement {
    fn unknown() -> Self {
        Movement {
            state: State::Unknown,
            jobs: Vec::new(),
            move_reason: None,
            draining: Vec::new(),
            filling: Vec::new(),
            waiting_for: Vec::new(),
            copies: None,
            clones: 0,
            repairing: false,
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
    pub snaptrim_pgs: u64,
    pub inconsistent_pgs: u64,
    pub repairing: bool,
    pub no_room: bool,
    pub noout: bool,
    pub osds_down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clones {
    pub count: u32,
    pub progress: f64,
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
    let count_with = |parts: &[&str]| -> u64 {
        states
            .iter()
            .filter(|(name, _)| name.split('+').any(|p| parts.contains(&p)))
            .map(|(_, n)| n)
            .sum()
    };
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
        snaptrim_pgs: count_with(&["snaptrim", "snaptrim_wait"]),
        inconsistent_pgs: count_with(&["inconsistent"]),
        repairing: count_with(&["repair"]) > 0,
        no_room: count_with(&["backfill_toofull", "recovery_toofull"]) > 0,
        noout: osd_flags.split(',').any(|f| f == "noout"),
        osds_down: num_up < num_osds,
    }
}

fn clone_event(e: &Value) -> bool {
    e["id"].as_str() == Some("mgr-vol-ongoing-clones")
        || e["refs"]
            .as_array()
            .is_some_and(|r| r.iter().any(|x| x.as_str() == Some("clone")))
            && e["message"]
                .as_str()
                .is_some_and(|m| m.contains("ongoing clones"))
}

pub fn clones(progress: &Value) -> Option<Clones> {
    let event = progress["events"]
        .as_array()?
        .iter()
        .find(|e| clone_event(e))?;
    let message = event["message"].as_str().unwrap_or("");
    let count = message
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    let fraction = event["progress"].as_f64().or_else(|| {
        let pct = regex::Regex::new(r"average progress is ([0-9.]+)%")
            .ok()?
            .captures(message)?
            .get(1)?
            .as_str()
            .parse::<f64>()
            .ok()?;
        Some(pct / 100.0)
    })?;
    Some(Clones {
        count,
        progress: fraction.clamp(0.0, 1.0),
    })
}

pub fn remaining_by_job(s: &Snapshot, c: Option<Clones>) -> BTreeMap<JobKind, u64> {
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
    if let Some(c) = c.filter(|c| c.progress < 1.0) {
        let left = ((1.0 - c.progress) * CLONE_SCALE as f64).round() as u64;
        jobs.insert(JobKind::Clone, left.max(1));
    }
    if s.snaptrim_pgs > 0 {
        jobs.insert(JobKind::FreeSpace, s.snaptrim_pgs);
    }
    if s.inconsistent_pgs > 0 {
        jobs.insert(JobKind::Repair, s.inconsistent_pgs);
    }
    jobs
}

pub fn state_of(s: &Snapshot, unavailable_for_long: bool, has_jobs: bool) -> State {
    if s.inactive_pgs > 0 && unavailable_for_long {
        State::Unavailable
    } else if s.no_room {
        State::NoRoom
    } else if s.noout && s.osds_down {
        State::Restarting
    } else if has_jobs {
        State::Working
    } else {
        State::Settled
    }
}

pub fn filling_osds(df: &Value) -> Vec<i64> {
    let avg = df["summary"]["average_utilization"].as_f64().unwrap_or(0.0);
    if avg <= 0.0 {
        return Vec::new();
    }
    df["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| n["reweight"].as_f64().unwrap_or(0.0) > 0.5)
                .filter(|n| n["crush_weight"].as_f64().unwrap_or(0.0) > 0.0)
                .filter(|n| n["utilization"].as_f64().unwrap_or(avg) < avg * 0.5)
                .filter_map(|n| n["id"].as_i64())
                .collect()
        })
        .unwrap_or_default()
}

pub fn pools_resizing(pools: &Value) -> bool {
    pools.as_array().is_some_and(|pools| {
        pools.iter().any(|p| {
            p["pg_num"] != p["pg_num_target"]
                || p["pg_placement_num"] != p["pg_placement_num_target"]
        })
    })
}

pub fn move_reason(draining: &[DiskRef], filling: &[DiskRef], resizing: bool) -> MoveReason {
    if !draining.is_empty() {
        MoveReason::Draining
    } else if !filling.is_empty() {
        MoveReason::Filling
    } else if resizing {
        MoveReason::Resizing
    } else {
        MoveReason::Balancing
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
    #[serde(default)]
    pub inactive_since: Option<u64>,
}

impl Record {
    pub fn advance(&mut self, now_ms: u64, remaining: &BTreeMap<JobKind, u64>, inactive: bool) {
        self.jobs.retain(|kind, _| remaining.contains_key(kind));
        for (&kind, &left) in remaining {
            self.jobs.entry(kind).or_default().record(now_ms, left);
        }
        self.inactive_since = if inactive {
            Some(self.inactive_since.unwrap_or(now_ms))
        } else {
            None
        };
    }

    pub fn unavailable_for_long(&self, now_ms: u64, inactive: bool) -> bool {
        inactive
            && self
                .inactive_since
                .is_some_and(|t| now_ms.saturating_sub(t) >= UNAVAILABLE_AFTER_MS)
    }

    pub fn jobs(&self, now_ms: u64, remaining: &BTreeMap<JobKind, u64>) -> Vec<Job> {
        remaining
            .iter()
            .map(|(&kind, &left)| {
                let tracked = self.jobs.get(&kind);
                let mut e = match tracked {
                    Some(t) => t.estimate(now_ms, left),
                    None => Estimate {
                        to_move: left,
                        moved: 0,
                        progress: None,
                        eta_secs: None,
                    },
                };
                if kind == JobKind::Clone {
                    e.progress = Some(1.0 - left as f64 / CLONE_SCALE as f64);
                }
                let bytes = kind.unit() == Unit::Bytes;
                Job {
                    kind,
                    unit: kind.unit(),
                    remaining_bytes: if bytes { left } else { 0 },
                    to_move_bytes: if bytes { e.to_move } else { 0 },
                    moved_bytes: if bytes { e.moved } else { 0 },
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

fn published_disks(published: &BTreeMap<String, String>) -> Vec<(String, String, Value)> {
    published
        .iter()
        .flat_map(|(node, raw)| {
            let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            payload["disks"]
                .as_object()
                .map(|disks| {
                    disks
                        .iter()
                        .map(|(id, d)| (node.clone(), id.clone(), d.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

fn disk_ref(node: &str, id: &str, d: &Value) -> DiskRef {
    DiskRef {
        node: node.to_string(),
        name: d["model"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or(id)
            .to_string(),
    }
}

fn sorted(mut out: Vec<DiskRef>) -> Vec<DiskRef> {
    out.sort_by(|a, b| (&a.node, &a.name).cmp(&(&b.node, &b.name)));
    out
}

pub fn draining_disks(published: &BTreeMap<String, String>) -> Vec<DiskRef> {
    sorted(
        published_disks(published)
            .into_iter()
            .filter(|(_, _, d)| matches!(d["phase"].as_str(), Some("draining") | Some("removing")))
            .map(|(node, id, d)| disk_ref(&node, &id, &d))
            .collect(),
    )
}

pub fn disks_of_osds(published: &BTreeMap<String, String>, osds: &[i64]) -> Vec<DiskRef> {
    let wanted: HashSet<i64> = osds.iter().copied().collect();
    sorted(
        published_disks(published)
            .into_iter()
            .filter(|(_, _, d)| d["osd_id"].as_i64().is_some_and(|id| wanted.contains(&id)))
            .map(|(node, id, d)| disk_ref(&node, &id, &d))
            .collect(),
    )
}

pub fn hosts_with_down_osds(tree: &Value) -> Vec<String> {
    let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
    let down: HashSet<i64> = nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd") && n["status"].as_str() == Some("down"))
        .filter_map(|n| n["id"].as_i64())
        .collect();
    let mut hosts: Vec<String> = nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("host"))
        .filter(|h| {
            h["children"].as_array().is_some_and(|c| {
                c.iter()
                    .filter_map(Value::as_i64)
                    .any(|id| down.contains(&id))
            })
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

struct Reading {
    snap: Snapshot,
    clones: Option<Clones>,
}

async fn read<H: Host>(host: &H) -> Option<Reading> {
    let status = host.ceph_json(&["status"]).await.ok()?;
    let flags = host
        .ceph_json(&["osd", "dump"])
        .await
        .ok()
        .and_then(|d| d["flags"].as_str().map(str::to_string))
        .unwrap_or_default();
    let clones = host
        .ceph_json(&["progress", "json"])
        .await
        .ok()
        .and_then(|p| clones(&p));
    Some(Reading {
        snap: snapshot(&status, &flags),
        clones,
    })
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
    let Some(r) = read(host).await else {
        return;
    };
    let remaining = remaining_by_job(&r.snap, r.clones);
    let inactive = r.snap.inactive_pgs > 0;
    let mut record = read_record(host).await;
    if remaining.is_empty() && !inactive && record == Record::default() {
        return;
    }
    record.advance(now_ms(), &remaining, inactive);
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
    let Some(r) = read(host).await else {
        return Movement::unknown();
    };
    let snap = r.snap;
    let now = now_ms();
    let remaining = remaining_by_job(&snap, r.clones);
    let record = read_record(host).await;
    let jobs = record.jobs(now, &remaining);
    let waiting_for = if snap.osds_down {
        host.ceph_json(&["osd", "tree"])
            .await
            .map(|t| hosts_with_down_osds(&t))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let published = settings::dump(host, settings::DISK_STATUS)
        .await
        .unwrap_or_default();
    let draining = draining_disks(&published);
    let (filling, reason) = if remaining.contains_key(&JobKind::Move) {
        let filling = if draining.is_empty() {
            let osds = host
                .ceph_json(&["osd", "df"])
                .await
                .map(|df| filling_osds(&df))
                .unwrap_or_default();
            disks_of_osds(&published, &osds)
        } else {
            Vec::new()
        };
        let resizing = draining.is_empty()
            && filling.is_empty()
            && host
                .ceph_json(&["osd", "pool", "ls", "detail"])
                .await
                .is_ok_and(|p| pools_resizing(&p));
        let reason = move_reason(&draining, &filling, resizing);
        (filling, Some(reason))
    } else {
        (Vec::new(), None)
    };
    let copies = match crate::topology::read_policy_from(host).await {
        Some(crate::topology::PolicyState::Chosen(p)) => Some(p.size),
        _ => None,
    };
    let inactive = snap.inactive_pgs > 0;
    Movement {
        state: state_of(
            &snap,
            record.unavailable_for_long(now, inactive),
            !jobs.is_empty(),
        ),
        eta_secs: overall_eta(&jobs),
        jobs,
        move_reason: reason,
        draining,
        filling,
        waiting_for,
        copies,
        clones: r.clones.map(|c| c.count).unwrap_or(0),
        repairing: snap.repairing,
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

    fn pgs(states: &[(&str, u64)]) -> Value {
        json!({"pgmap": {"pgs_by_state": states
            .iter()
            .map(|(s, n)| json!({"state_name": s, "count": n}))
            .collect::<Vec<_>>()}})
    }

    #[test]
    fn the_drain_seen_on_node3_is_one_move_job_with_everything_usable() {
        let s = snapshot(&live_node3_status(), "sortbitwise,recovery_deletes");
        let jobs = remaining_by_job(&s, None);
        assert_eq!(state_of(&s, false, !jobs.is_empty()), State::Working);
        assert_eq!(
            jobs.keys().copied().collect::<Vec<_>>(),
            vec![JobKind::Move]
        );
        let mv = jobs[&JobKind::Move];
        assert!(mv > 96_000_000_000 && mv < 97_000_000_000);
    }

    #[test]
    fn a_drain_and_a_raised_copy_count_are_two_separate_jobs() {
        let jobs = remaining_by_job(&snapshot(&drain_and_second_copy(), ""), None);
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
        let jobs = remaining_by_job(&snapshot(&status, ""), None);
        assert!(jobs.contains_key(&JobKind::Rebuild));
        assert!(!jobs.contains_key(&JobKind::AddCopies));
    }

    #[test]
    fn a_short_peering_blip_is_not_called_unavailable() {
        let s = snapshot(&pgs(&[("active+clean", 70), ("peering", 11)]), "");
        assert_eq!(s.inactive_pgs, 11);
        let mut r = Record::default();
        r.advance(0, &left(&[]), true);
        assert!(!r.unavailable_for_long(30_000, true));
        assert_eq!(
            state_of(&s, r.unavailable_for_long(30_000, true), false),
            State::Settled
        );
    }

    #[test]
    fn files_out_of_reach_for_a_while_are_unavailable() {
        let s = snapshot(
            &pgs(&[("active+clean", 70), ("down", 3), ("peering", 2)]),
            "",
        );
        let mut r = Record::default();
        r.advance(0, &left(&[]), true);
        r.advance(60_000, &left(&[]), true);
        assert!(r.unavailable_for_long(60_000, true));
        assert_eq!(state_of(&s, true, false), State::Unavailable);
    }

    #[test]
    fn coming_back_resets_the_unavailable_clock() {
        let mut r = Record::default();
        r.advance(0, &left(&[]), true);
        r.advance(60_000, &left(&[]), false);
        assert_eq!(r.inactive_since, None);
        r.advance(70_000, &left(&[]), true);
        assert!(!r.unavailable_for_long(90_000, true));
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
        assert_eq!(state_of(&s, true, true), State::Unavailable);
    }

    #[test]
    fn a_full_backfill_target_is_reported_as_no_room() {
        let s = snapshot(&pgs(&[("active+remapped+backfill_toofull", 4)]), "");
        assert_eq!(state_of(&s, false, true), State::NoRoom);
    }

    #[test]
    fn a_machine_restarting_under_noout_is_not_called_working() {
        let status = json!({
            "pgmap": {"pgs_by_state": [{"state_name": "active+undersized+degraded", "count": 9}],
                      "degraded_objects": 100, "num_objects": 200, "data_bytes": 1000},
            "osdmap": {"num_osds": 3, "num_up_osds": 2}
        });
        assert_eq!(
            state_of(&snapshot(&status, "noout,sortbitwise"), false, true),
            State::Restarting
        );
        assert_eq!(
            state_of(&snapshot(&status, "sortbitwise"), false, true),
            State::Working
        );
    }

    #[test]
    fn a_clean_cluster_is_settled_with_no_jobs() {
        let s = snapshot(&pgs(&[("active+clean", 81)]), "");
        let jobs = remaining_by_job(&s, None);
        assert!(jobs.is_empty());
        assert_eq!(state_of(&s, false, false), State::Settled);
    }

    #[test]
    fn deleting_snapshots_is_a_free_space_job() {
        let s = snapshot(
            &pgs(&[
                ("active+clean", 70),
                ("active+clean+snaptrim", 3),
                ("active+clean+snaptrim_wait", 8),
            ]),
            "",
        );
        assert_eq!(
            remaining_by_job(&s, None).get(&JobKind::FreeSpace),
            Some(&11)
        );
    }

    #[test]
    fn damaged_copies_are_a_repair_job_and_say_whether_repair_runs() {
        let found = snapshot(&pgs(&[("active+clean+inconsistent", 2)]), "");
        assert_eq!(
            remaining_by_job(&found, None).get(&JobKind::Repair),
            Some(&2)
        );
        assert!(!found.repairing);
        let fixing = snapshot(
            &pgs(&[("active+clean+scrubbing+deep+inconsistent+repair", 2)]),
            "",
        );
        assert!(fixing.repairing);
    }

    #[test]
    fn routine_scrubbing_is_not_a_job() {
        let s = snapshot(
            &pgs(&[
                ("active+clean+scrubbing", 3),
                ("active+clean+scrubbing+deep", 1),
                ("active+clean", 77),
            ]),
            "",
        );
        assert!(remaining_by_job(&s, None).is_empty());
    }

    #[test]
    fn an_app_copy_reads_its_progress_from_the_ceph_progress_event() {
        let progress = json!({
            "events": [{"id": "mgr-vol-ongoing-clones", "message": "2 ongoing clones - average progress is 66.555%", "refs": ["mds", "clone"]}],
            "completed": []
        });
        let c = clones(&progress).unwrap();
        assert_eq!(c.count, 2);
        assert!((c.progress - 0.66555).abs() < 1e-9);
        let jobs = Record::default().jobs(0, &remaining_by_job(&Snapshot::default(), Some(c)));
        assert_eq!(jobs[0].kind, JobKind::Clone);
        assert_eq!(jobs[0].unit, Unit::Percent);
        assert!((jobs[0].progress.unwrap() - 0.6656).abs() < 1e-3);
    }

    #[test]
    fn a_finished_clone_in_the_completed_list_is_not_a_job() {
        let progress = json!({
            "events": [],
            "completed": [{"id": "mgr-vol-ongoing-clones", "message": "1 ongoing clones - average progress is 100.0%", "refs": ["mds", "clone"]}]
        });
        assert_eq!(clones(&progress), None);
    }

    #[test]
    fn an_explicit_progress_field_wins_over_the_message() {
        let progress = json!({"events": [{"id": "mgr-vol-ongoing-clones", "message": "1 ongoing clones - average progress is 10%", "progress": 0.25}]});
        assert_eq!(clones(&progress).unwrap().progress, 0.25);
    }

    #[test]
    fn a_much_emptier_disk_is_the_one_being_filled() {
        let df = json!({
            "nodes": [
                {"id": 0, "crush_weight": 0.19, "reweight": 1.0, "utilization": 14.8},
                {"id": 1, "crush_weight": 0.17, "reweight": 1.0, "utilization": 14.4},
                {"id": 3, "crush_weight": 0.9, "reweight": 1.0, "utilization": 1.2},
                {"id": 2, "crush_weight": 0.9, "reweight": 0.0, "utilization": 0.0}
            ],
            "summary": {"average_utilization": 10.0}
        });
        assert_eq!(filling_osds(&df), vec![3]);
    }

    #[test]
    fn a_pool_changing_its_group_count_is_resizing() {
        assert!(pools_resizing(
            &json!([{"pg_num": 32, "pg_num_target": 64, "pg_placement_num": 32, "pg_placement_num_target": 64}])
        ));
        assert!(!pools_resizing(
            &json!([{"pg_num": 32, "pg_num_target": 32, "pg_placement_num": 32, "pg_placement_num_target": 32}])
        ));
    }

    #[test]
    fn the_reason_for_moving_prefers_what_the_person_did() {
        let d = vec![DiskRef {
            node: "node2".into(),
            name: "easystore".into(),
        }];
        assert_eq!(move_reason(&d, &d, true), MoveReason::Draining);
        assert_eq!(move_reason(&[], &d, true), MoveReason::Filling);
        assert_eq!(move_reason(&[], &[], true), MoveReason::Resizing);
        assert_eq!(move_reason(&[], &[], false), MoveReason::Balancing);
    }

    #[test]
    fn progress_is_measured_from_the_peak_recorded_when_the_job_began() {
        let mut r = Record::default();
        r.advance(0, &left(&[(JobKind::Move, 1000)]), false);
        let jobs = r.jobs(30_000, &left(&[(JobKind::Move, 750)]));
        assert_eq!(jobs[0].to_move_bytes, 1000);
        assert_eq!(jobs[0].moved_bytes, 250);
        assert_eq!(jobs[0].progress, Some(0.25));
    }

    #[test]
    fn a_second_job_starting_does_not_reset_the_first() {
        let mut r = Record::default();
        r.advance(0, &left(&[(JobKind::Move, 1000)]), false);
        r.advance(
            120_000,
            &left(&[(JobKind::Move, 800), (JobKind::AddCopies, 5000)]),
            false,
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
        r.advance(0, &left(&[(JobKind::Move, 1000)]), false);
        r.advance(60_000, &left(&[]), false);
        assert!(r.jobs.is_empty());
        r.advance(120_000, &left(&[(JobKind::Move, 300)]), false);
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
    fn percent_jobs_never_report_bytes() {
        let jobs = Record::default().jobs(0, &left(&[(JobKind::FreeSpace, 11)]));
        assert_eq!(jobs[0].unit, Unit::Percent);
        assert_eq!(jobs[0].to_move_bytes, 0);
        assert_eq!(jobs[0].remaining_bytes, 0);
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
        r.advance(5, &left(&[(JobKind::Move, 10), (JobKind::Clone, 20)]), true);
        let raw = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&raw).unwrap(), r);
    }

    #[test]
    fn a_record_written_before_the_unavailable_clock_still_loads() {
        let old = r#"{"jobs":{"move":{"peak":10,"samples":[[0,10]]}}}"#;
        let r: Record = serde_json::from_str(old).unwrap();
        assert_eq!(r.inactive_since, None);
        assert_eq!(r.jobs[&JobKind::Move].peak, 10);
    }

    #[test]
    fn the_whole_reorganisation_takes_as_long_as_its_slowest_job() {
        let job = |eta| Job {
            kind: JobKind::Move,
            unit: Unit::Bytes,
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
    fn disks_are_named_by_their_model_and_machine() {
        let published = BTreeMap::from([
            (
                "node2".to_string(),
                json!({"disks": {
                    "serial-wwn-0x50014ee214caf529": {"model": "easystore 2647", "phase": "draining", "osd_id": 2},
                    "system": {"model": "System disk", "phase": "active", "osd_id": 1}
                }})
                .to_string(),
            ),
            (
                "node3".to_string(),
                json!({"disks": {"system": {"model": "System disk", "phase": "active", "osd_id": 0}}})
                    .to_string(),
            ),
        ]);
        let easystore = DiskRef {
            node: "node2".into(),
            name: "easystore 2647".into(),
        };
        assert_eq!(draining_disks(&published), vec![easystore]);
        assert_eq!(
            disks_of_osds(&published, &[0]),
            vec![DiskRef {
                node: "node3".into(),
                name: "System disk".into()
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
