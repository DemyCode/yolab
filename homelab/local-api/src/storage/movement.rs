use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use serde::Serialize;
use serde_json::Value;

use crate::host::{Host, RealHost};
use crate::storage::settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Settled,
    Moving,
    Rebuilding,
    Unavailable,
    NoRoom,
    Restarting,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskRef {
    pub node: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Movement {
    pub state: State,
    pub draining: Vec<DiskRef>,
    pub waiting_for: Vec<String>,
    pub remaining_bytes: u64,
    pub moved_bytes: u64,
    pub to_move_bytes: u64,
    pub progress: Option<f64>,
    pub eta_secs: Option<u64>,
    pub inactive_pgs: u64,
    pub total_pgs: u64,
}

impl Movement {
    fn unknown() -> Self {
        Movement {
            state: State::Unknown,
            draining: Vec::new(),
            waiting_for: Vec::new(),
            remaining_bytes: 0,
            moved_bytes: 0,
            to_move_bytes: 0,
            progress: None,
            eta_secs: None,
            inactive_pgs: 0,
            total_pgs: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub remaining_bytes: u64,
    pub inactive_pgs: u64,
    pub total_pgs: u64,
    pub degraded: bool,
    pub misplaced: bool,
    pub no_room: bool,
    pub noout: bool,
    pub osds_down: bool,
}

fn ratio(pgmap: &Value, key: &str) -> f64 {
    pgmap[key].as_f64().unwrap_or(0.0).clamp(0.0, 1.0)
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
    let has = |part: &str| {
        states
            .iter()
            .any(|(name, _)| name.split('+').any(|p| p == part))
    };
    let inactive_pgs = states
        .iter()
        .filter(|(name, _)| !name.split('+').any(|p| p == "active"))
        .map(|(_, n)| n)
        .sum();
    let degraded = pgmap["degraded_objects"].as_u64().unwrap_or(0) > 0;
    let misplaced = pgmap["misplaced_objects"].as_u64().unwrap_or(0) > 0;
    let remaining = (ratio(pgmap, "misplaced_ratio") + ratio(pgmap, "degraded_ratio")).min(1.0);
    let data_bytes = pgmap["data_bytes"].as_u64().unwrap_or(0);
    let osdmap = &status["osdmap"];
    let num_osds = osdmap["num_osds"].as_u64().unwrap_or(0);
    let num_up = osdmap["num_up_osds"].as_u64().unwrap_or(num_osds);
    Snapshot {
        remaining_bytes: (data_bytes as f64 * remaining).round() as u64,
        inactive_pgs,
        total_pgs: pgmap["num_pgs"].as_u64().unwrap_or(0),
        degraded,
        misplaced,
        no_room: has("backfill_toofull") || has("recovery_toofull"),
        noout: osd_flags.split(',').any(|f| f == "noout"),
        osds_down: num_up < num_osds,
    }
}

pub fn state_of(s: &Snapshot) -> State {
    if s.inactive_pgs > 0 {
        State::Unavailable
    } else if s.no_room {
        State::NoRoom
    } else if s.noout && s.osds_down {
        State::Restarting
    } else if s.degraded {
        State::Rebuilding
    } else if s.misplaced || s.remaining_bytes > 0 {
        State::Moving
    } else {
        State::Settled
    }
}

const WINDOW_MS: u64 = 15 * 60 * 1000;
const MIN_SPAN_MS: u64 = 60 * 1000;

#[derive(Debug, Default)]
pub struct Tracker {
    peak: u64,
    samples: VecDeque<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
pub struct Estimate {
    pub to_move: u64,
    pub moved: u64,
    pub progress: Option<f64>,
    pub eta_secs: Option<u64>,
}

impl Tracker {
    pub fn record(&mut self, now_ms: u64, remaining: u64) -> Estimate {
        if remaining == 0 {
            self.peak = 0;
            self.samples.clear();
            return Estimate {
                to_move: 0,
                moved: 0,
                progress: None,
                eta_secs: None,
            };
        }
        self.peak = self.peak.max(remaining);
        self.samples.push_back((now_ms, remaining));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now_ms.saturating_sub(*t) > WINDOW_MS)
        {
            self.samples.pop_front();
        }
        let moved = self.peak - remaining;
        Estimate {
            to_move: self.peak,
            moved,
            progress: Some(moved as f64 / self.peak as f64),
            eta_secs: self.eta(now_ms, remaining),
        }
    }

    fn eta(&self, now_ms: u64, remaining: u64) -> Option<u64> {
        let &(t0, r0) = self.samples.front()?;
        let span = now_ms.saturating_sub(t0);
        if span < MIN_SPAN_MS || r0 <= remaining {
            return None;
        }
        let rate_per_ms = (r0 - remaining) as f64 / span as f64;
        Some((remaining as f64 / rate_per_ms / 1000.0).round() as u64)
    }
}

pub fn draining_disks(published: &std::collections::BTreeMap<String, String>) -> Vec<DiskRef> {
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

fn tracker() -> &'static Mutex<Tracker> {
    static T: std::sync::OnceLock<Mutex<Tracker>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(Tracker::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn assess_via<H: Host>(host: &H) -> Movement {
    let Ok(status) = host.ceph_json(&["status"]).await else {
        return Movement::unknown();
    };
    let flags = host
        .ceph_json(&["osd", "dump"])
        .await
        .ok()
        .and_then(|d| d["flags"].as_str().map(str::to_string))
        .unwrap_or_default();
    let snap = snapshot(&status, &flags);
    let state = state_of(&snap);
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
    let estimate = tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record(now_ms(), snap.remaining_bytes);
    Movement {
        state,
        draining,
        waiting_for,
        remaining_bytes: snap.remaining_bytes,
        moved_bytes: estimate.moved,
        to_move_bytes: estimate.to_move,
        progress: estimate.progress,
        eta_secs: estimate.eta_secs,
        inactive_pgs: snap.inactive_pgs,
        total_pgs: snap.total_pgs,
    }
}

pub async fn handler() -> Json<Movement> {
    Json(assess_via(&RealHost).await)
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
                "misplaced_ratio": 0.7991078840283119,
                "recovering_bytes_per_sec": 358717
            },
            "osdmap": {"num_osds": 3, "num_up_osds": 3, "num_in_osds": 2}
        })
    }

    #[test]
    fn the_drain_seen_on_node3_reads_as_moving_with_everything_usable() {
        let s = snapshot(&live_node3_status(), "sortbitwise,recovery_deletes");
        assert_eq!(state_of(&s), State::Moving);
        assert_eq!(s.inactive_pgs, 0);
        assert_eq!(s.total_pgs, 81);
        assert!(s.remaining_bytes > 96_000_000_000 && s.remaining_bytes < 97_000_000_000);
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
            degraded: true,
            misplaced: true,
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
        ], "misplaced_objects": 10, "misplaced_ratio": 0.1, "data_bytes": 1000}});
        assert_eq!(state_of(&snapshot(&status, "")), State::NoRoom);
    }

    #[test]
    fn a_machine_restarting_under_noout_is_not_called_rebuilding() {
        let status = json!({
            "pgmap": {"pgs_by_state": [{"state_name": "active+undersized+degraded", "count": 9}],
                      "degraded_objects": 100, "degraded_ratio": 0.5, "data_bytes": 1000},
            "osdmap": {"num_osds": 3, "num_up_osds": 2}
        });
        assert_eq!(
            state_of(&snapshot(&status, "noout,sortbitwise")),
            State::Restarting
        );
        assert_eq!(
            state_of(&snapshot(&status, "sortbitwise")),
            State::Rebuilding
        );
    }

    #[test]
    fn a_clean_cluster_is_settled() {
        let status = json!({"pgmap": {"pgs_by_state": [{"state_name": "active+clean", "count": 81}],
                                      "data_bytes": 1000}});
        assert_eq!(state_of(&snapshot(&status, "")), State::Settled);
    }

    #[test]
    fn progress_is_measured_from_the_peak_of_this_movement() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        let e = t.record(30_000, 750);
        assert_eq!(e.to_move, 1000);
        assert_eq!(e.moved, 250);
        assert_eq!(e.progress, Some(0.25));
    }

    #[test]
    fn no_time_is_promised_before_a_minute_of_samples() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.record(59_000, 900).eta_secs, None);
        assert!(t.record(60_000, 900).eta_secs.is_some());
    }

    #[test]
    fn the_time_left_follows_the_observed_rate() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        let e = t.record(100_000, 500);
        assert_eq!(e.eta_secs, Some(100));
    }

    #[test]
    fn a_stalled_movement_gets_no_time_left() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.record(120_000, 1000).eta_secs, None);
    }

    #[test]
    fn finishing_starts_the_next_movement_from_zero() {
        let mut t = Tracker::default();
        t.record(0, 1000);
        assert_eq!(t.record(10_000, 0).progress, None);
        let e = t.record(20_000, 300);
        assert_eq!(e.to_move, 300);
        assert_eq!(e.moved, 0);
    }

    #[test]
    fn samples_older_than_the_window_stop_steering_the_rate() {
        let mut t = Tracker::default();
        t.record(0, 10_000);
        t.record(WINDOW_MS - 60_000, 2_000);
        let e = t.record(WINDOW_MS + 60_000, 1_000);
        assert_eq!(e.eta_secs, Some(120));
    }

    #[test]
    fn the_disk_being_drained_is_named_by_its_model_and_machine() {
        let published = std::collections::BTreeMap::from([
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
                json!({"disks": {"system": {"model": "System disk", "phase": "active"}}}).to_string(),
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
