
use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::Scope;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", content = "reason", rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running,
    Idle,
    Waiting(String),
    Paused(String),
    Standby,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControllerStatus {
    pub name: &'static str,
    pub scope: Scope,
    pub interval_secs: u64,
    #[serde(flatten)]
    pub phase: Phase,
    pub runs: u64,
    pub last_started_at: Option<DateTime<Utc>>,
    pub last_finished_at: Option<DateTime<Utc>>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_note: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub panics: u32,
}

#[derive(Default)]
pub struct Registry {
    inner: RwLock<BTreeMap<&'static str, ControllerStatus>>,
}

impl Registry {
    fn with<R>(&self, name: &str, f: impl FnOnce(&mut ControllerStatus) -> R) -> Option<R> {
        let mut map = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get_mut(name).map(f)
    }

    pub fn register(&self, name: &'static str, scope: Scope, interval: Duration) {
        let mut map = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(
            name,
            ControllerStatus {
                name,
                scope,
                interval_secs: interval.as_secs(),
                phase: Phase::Starting,
                runs: 0,
                last_started_at: None,
                last_finished_at: None,
                last_ok_at: None,
                last_note: None,
                last_error: None,
                last_error_at: None,
                consecutive_failures: 0,
                panics: 0,
            },
        );
    }

    pub fn set_phase(&self, name: &str, phase: Phase) {
        self.with(name, |s| s.phase = phase);
    }

    pub fn started(&self, name: &str) {
        self.with(name, |s| {
            s.phase = Phase::Running;
            s.runs += 1;
            s.last_started_at = Some(Utc::now());
        });
    }

    pub fn succeeded(&self, name: &str, note: Option<String>) {
        self.with(name, |s| {
            let now = Utc::now();
            s.phase = Phase::Idle;
            s.last_finished_at = Some(now);
            s.last_ok_at = Some(now);
            s.last_note = note;
            s.consecutive_failures = 0;
        });
    }

    pub fn failed(&self, name: &str, error: String) -> u32 {
        self.with(name, |s| {
            let now = Utc::now();
            s.phase = Phase::Idle;
            s.last_finished_at = Some(now);
            s.last_error = Some(error);
            s.last_error_at = Some(now);
            s.consecutive_failures = s.consecutive_failures.saturating_add(1);
            s.consecutive_failures
        })
        .unwrap_or(1)
    }

    pub fn panicked(&self, name: &str, error: String) -> u32 {
        let n = self.failed(name, format!("panicked: {error}"));
        self.with(name, |s| s.panics = s.panics.saturating_add(1));
        n
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<ControllerStatus> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    pub fn snapshot(&self) -> Vec<ControllerStatus> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

pub const SNAPSHOT_PATH: &str = "/run/yolab/controllers.json";

pub fn publish_snapshot() {
    if cfg!(test) {
        return;
    }
    let snap = serde_json::json!({
        "node": crate::system::hostname(),
        "written_at": Utc::now(),
        "leader": super::leader::current_holder_is_me(),
        "controllers": super::registry().snapshot(),
    });
    let Ok(body) = serde_json::to_vec_pretty(&snap) else {
        return;
    };
    let path = std::path::Path::new(SNAPSHOT_PATH);
    let tmp = path.with_extension("json.tmp");
    let written = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        tracing::debug!("could not write {SNAPSHOT_PATH}: {e}");
    }
}

pub async fn handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "node": crate::system::hostname(),
        "leader": super::leader::current_holder_is_me(),
        "controllers": super::registry().snapshot(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_streak_resets_on_success_and_panics_are_counted() {
        let r = Registry::default();
        r.register("x", Scope::Node, Duration::from_secs(30));
        r.started("x");
        assert_eq!(r.failed("x", "boom".into()), 1);
        assert_eq!(r.panicked("x", "oops".into()), 2);
        let s = r.get("x").unwrap();
        assert_eq!(s.consecutive_failures, 2);
        assert_eq!(s.panics, 1);
        assert_eq!(s.last_error.as_deref(), Some("panicked: oops"));
        r.succeeded("x", Some("nothing to do".into()));
        let s = r.get("x").unwrap();
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.last_note.as_deref(), Some("nothing to do"));
        assert!(s.last_error.is_some());
    }

    #[test]
    fn status_serializes_phase_with_its_reason() {
        let r = Registry::default();
        r.register("y", Scope::Cluster, Duration::from_secs(60));
        r.set_phase("y", Phase::Paused("a restore is running".into()));
        let v = serde_json::to_value(r.get("y").unwrap()).unwrap();
        assert_eq!(v["phase"], "paused");
        assert_eq!(v["reason"], "a restore is running");
        assert_eq!(v["scope"], "cluster");
    }

    #[test]
    fn an_unknown_controller_is_ignored_rather_than_created() {
        let r = Registry::default();
        r.started("ghost");
        assert!(r.get("ghost").is_none());
        assert_eq!(r.failed("ghost", "boom".into()), 1);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn a_phase_without_a_reason_serializes_as_just_the_phase() {
        let r = Registry::default();
        r.register("z", Scope::Node, Duration::from_secs(5));
        let v = serde_json::to_value(r.get("z").unwrap()).unwrap();
        assert_eq!(v["phase"], "starting");
        assert!(v.get("reason").is_none());
        assert_eq!(v["interval_secs"], 5);
    }

    #[tokio::test]
    async fn the_status_endpoint_lists_the_registered_controllers() {
        super::super::registry().register(
            "test-status-endpoint",
            Scope::Node,
            Duration::from_secs(1),
        );
        let axum::Json(body) = handler().await;
        let names: Vec<&str> = body["controllers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(names.contains(&"test-status-endpoint"), "{body}");
        assert!(body["leader"].is_boolean());
    }
}
