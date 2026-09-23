pub mod activity;
pub mod leader;
pub mod lock;
pub mod resource;
pub mod status;
pub mod watch;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

pub use activity::Activity;
use status::Registry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Node,
    Cluster,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    KubeApi,
    Ceph,
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Requirement::KubeApi => "the Kubernetes API",
            Requirement::Ceph => "the Ceph cluster",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    Done,
    Idle(String),
    RequeueAfter(Duration),
}

pub struct Ctx {
    pub node: String,
}

pub trait Controller: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn scope(&self) -> Scope;
    fn interval(&self) -> Duration;
    fn requires(&self) -> &'static [Requirement] {
        &[]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::ZERO
    }
    fn reconcile(&self, ctx: &Ctx) -> impl Future<Output = anyhow::Result<Tick>> + Send;
}

const MIN_GAP: Duration = Duration::from_secs(2);

const RECHECK: Duration = Duration::from_secs(15);

fn failure_backoff(consecutive: u32, interval: Duration) -> Duration {
    let base = Duration::from_secs(10).min(interval);
    let grown =
        Duration::from_secs(10).saturating_mul(1u32 << consecutive.saturating_sub(1).min(6));
    grown.clamp(base, interval)
}

struct Shared {
    wakers: std::sync::Mutex<HashMap<&'static str, Arc<Notify>>>,
    registry: Registry,
    graph: resource::Graph,
}

fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| Shared {
        wakers: std::sync::Mutex::new(HashMap::new()),
        registry: Registry::default(),
        graph: resource::Graph::default(),
    })
}

fn waker(name: &'static str) -> Arc<Notify> {
    let mut w = shared()
        .wakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.entry(name)
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone()
}

pub fn wake(name: &str) {
    let w = shared()
        .wakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(n) = w.get(name) {
        n.notify_one();
    }
}

pub fn registry() -> &'static Registry {
    &shared().registry
}

fn uptime() -> Option<Duration> {
    let raw = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = raw.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs))
}

pub fn spawn<C: Controller>(controller: C, leader: leader::Leadership) {
    resource::spawn(resource::ControllerResource(controller), leader);
}

async fn wait_or_wake(notify: &Notify, d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = notify.notified() => {}
    }
}

pub async fn run_once<C: Controller>(controller: &C) -> anyhow::Result<Tick> {
    let node = crate::system::hostname();
    if controller.scope() == Scope::Cluster && !leader::held_by(&node).await? {
        anyhow::bail!(
            "{} is cluster-scoped and {node} does not hold the cluster lease — run it on the leader",
            controller.name()
        );
    }
    if let Some(missing) = activity::unmet(controller.requires()).await {
        anyhow::bail!("not running {}: waiting for {missing}", controller.name());
    }
    if let activity::Gate::Paused(why) = activity::gate(controller.pauses_during()).await {
        anyhow::bail!("not running {}: {why}", controller.name());
    }
    controller.reconcile(&Ctx { node }).await
}

#[cfg(test)]
mod tests {
    use super::status::Phase;
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn failure_backoff_grows_but_never_exceeds_the_interval() {
        let interval = Duration::from_secs(120);
        assert_eq!(failure_backoff(1, interval), Duration::from_secs(10));
        assert_eq!(failure_backoff(2, interval), Duration::from_secs(20));
        assert_eq!(failure_backoff(4, interval), Duration::from_secs(80));
        assert_eq!(failure_backoff(10, interval), interval);
        assert_eq!(
            failure_backoff(5, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    struct Flaky {
        runs: Arc<AtomicU32>,
    }

    impl Controller for Flaky {
        fn name(&self) -> &'static str {
            "test-flaky"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            let n = self.runs.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => panic!("first tick panics"),
                1 => anyhow::bail!("second tick fails"),
                _ => Ok(Tick::Done),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_or_failing_tick_is_recorded_and_the_controller_keeps_running() {
        let runs = Arc::new(AtomicU32::new(0));
        spawn(
            Flaky { runs: runs.clone() },
            leader::Leadership::fixed_for_tests(true),
        );
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if registry()
                .get("test-flaky")
                .is_some_and(|s| s.last_ok_at.is_some())
            {
                break;
            }
        }
        assert!(runs.load(Ordering::SeqCst) >= 3);
        let s = registry().get("test-flaky").expect("registered");
        assert_eq!(s.panics, 1);
        assert!(s.last_ok_at.is_some());
        assert_eq!(s.consecutive_failures, 0);
    }

    struct Clustered {
        runs: Arc<AtomicU32>,
    }

    impl Controller for Clustered {
        fn name(&self) -> &'static str {
            "test-clustered"
        }
        fn scope(&self) -> Scope {
            Scope::Cluster
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cluster_controller_never_runs_on_a_standby() {
        let runs = Arc::new(AtomicU32::new(0));
        spawn(
            Clustered { runs: runs.clone() },
            leader::Leadership::fixed_for_tests(false),
        );
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            registry().get("test-clustered").map(|s| s.phase),
            Some(Phase::Standby)
        );
    }

    struct Woken {
        runs: Arc<AtomicU32>,
    }

    impl Controller for Woken {
        fn name(&self) -> &'static str {
            "test-woken"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_runs_a_controller_before_its_interval() {
        let runs = Arc::new(AtomicU32::new(0));
        spawn(
            Woken { runs: runs.clone() },
            leader::Leadership::fixed_for_tests(true),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        wake("test-woken");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    struct Impatient {
        runs: Arc<AtomicU32>,
    }

    impl Controller for Impatient {
        fn name(&self) -> &'static str {
            "test-impatient"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            match self.runs.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(Tick::RequeueAfter(Duration::from_secs(10))),
                _ => Ok(Tick::Idle("nothing left to do".into())),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_requeue_runs_sooner_and_an_idle_reason_is_shown() {
        let runs = Arc::new(AtomicU32::new(0));
        spawn(
            Impatient { runs: runs.clone() },
            leader::Leadership::fixed_for_tests(true),
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "requeued long before the hour"
        );
        let s = registry().get("test-impatient").expect("registered");
        assert_eq!(s.last_note.as_deref(), Some("nothing left to do"));
        assert_eq!(s.phase, Phase::Idle);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "an idle tick waits the interval"
        );
    }
}
