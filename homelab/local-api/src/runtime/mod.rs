//! The controller runtime: how every background job in local-api runs.
//!
//! WHAT THIS REPLACED
//!
//! Background work used to live in three places that did not know about each
//! other: ten `loop { …; sleep }` functions spawned from main.rs, eleven systemd
//! timers re-running `local-api storage <x>` oneshots, and HTTP handlers that
//! detached long tasks. Each loop hand-wrote its own startup sleep ("let k3s
//! settle"), its own leader check (borrowed from the disk reconciler), and its own
//! "is a restore running?" guard — and they disagreed: the disk reconciler paused
//! for restores but not for storage recovery, cephfs paused for both, the backup
//! scheduler and restore watchdog ran on every node with no leader at all.
//!
//! WHAT A CONTROLLER DECLARES
//!
//! Everything that used to be copy-pasted into each loop is now a declaration the
//! runtime enforces in one place, the way systemd enforces `After=` instead of
//! every daemon sleeping:
//!
//!   - `scope`: `Node` runs on every machine; `Cluster` runs only on the elected
//!     leader (one Lease, owned by the runtime, not by any one controller).
//!   - `requires`: what must be answering before a tick is worth running (the
//!     Kubernetes API, Ceph). Unmet requirements are a WAITING state, not an error
//!     and not a sleep.
//!   - `pauses_during`: cluster-wide activities (a restore, a storage recovery)
//!     during which the controller must not act. When the runtime cannot tell
//!     whether one is running, it pauses — the old guards failed open.
//!   - `not_before_uptime`: `OnBootSec` for jobs that used to be timers.
//!   - `interval`: periodic resync. Anything can also `wake(name)` a controller
//!     early — a disk change from udev, an API call that changed its input.
//!
//! WHAT IT DELIBERATELY DOES NOT DO
//!
//! No dependency graph between controllers, no inferred ordering, no retries
//! inside a tick. A tick is a plain async function that reads the world and acts;
//! the runtime only decides WHEN it runs and records HOW it went. Every
//! controller's status is visible at `GET /api/system/controllers` and in
//! `/run/yolab/controllers.json`, so "is the disk reconciler alive?" is a lookup,
//! not a journal search.

pub mod activity;
pub mod leader;
pub mod lock;
pub mod status;
pub mod watch;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

pub use activity::Activity;
use status::{Phase, Registry};

/// Where a controller runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// On every machine, acting on that machine.
    Node,
    /// On exactly one machine at a time: the Lease holder.
    Cluster,
}

/// Something that must be answering before a tick can do anything useful.
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

/// How a tick went, when it did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Converged, or did what it could. Next run after `interval`.
    Done,
    /// Nothing to do, for a reason worth showing (e.g. "no disks switched on").
    Idle(String),
    /// Run again sooner than `interval`.
    RequeueAfter(Duration),
}

/// Everything a tick is told about the world by the runtime.
pub struct Ctx {
    pub node: String,
    leader: Option<leader::Leadership>,
}

impl Ctx {
    fn new(node: String, scope: Scope, leader: &leader::Leadership) -> Self {
        Self {
            node,
            leader: (scope == Scope::Cluster).then(|| leader.clone()),
        }
    }

    /// Whether this tick may still act. The runtime checks leadership only
    /// BEFORE a tick; a cluster-scoped tick that runs for minutes (a storage
    /// recovery reinstalling every app) must ask again between steps, or a node
    /// that lost the lease halfway keeps acting while the new leader starts the
    /// same work — two drivers of one destructive operation. Always true for a
    /// node-scoped controller.
    pub fn still_in_charge(&self) -> bool {
        self.leader
            .as_ref()
            .is_none_or(leader::Leadership::is_leader)
    }
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
    /// Machine uptime before the first tick — the old timers' `OnBootSec`.
    /// Measured from boot, not from process start, so a local-api restart on a
    /// long-running machine does not re-wait.
    fn not_before_uptime(&self) -> Duration {
        Duration::ZERO
    }
    fn reconcile(&self, ctx: &Ctx) -> impl Future<Output = anyhow::Result<Tick>> + Send;
}

/// Shortest gap between two ticks of the same controller, however many wakes
/// arrive: a burst of udev events must not become a burst of reconciles.
const MIN_GAP: Duration = Duration::from_secs(2);

/// How long to wait before re-checking an unmet requirement, pause or
/// leadership. Short, because each check is cheap and the state it waits on
/// changes on its own.
const RECHECK: Duration = Duration::from_secs(15);

/// After a tick fails, wait at least this long — growing with consecutive
/// failures — so a controller stuck on a persistent error does not hammer the
/// cluster, but still retries sooner than a long `interval` would.
fn failure_backoff(consecutive: u32, interval: Duration) -> Duration {
    let base = Duration::from_secs(10).min(interval);
    let grown =
        Duration::from_secs(10).saturating_mul(1u32 << consecutive.saturating_sub(1).min(6));
    grown.clamp(base, interval)
}

struct Shared {
    wakers: std::sync::Mutex<HashMap<&'static str, Arc<Notify>>>,
    registry: Registry,
}

fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| Shared {
        wakers: std::sync::Mutex::new(HashMap::new()),
        registry: Registry::default(),
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

/// Ask a controller to run as soon as its gates allow. Cheap and idempotent:
/// many wakes before the next tick collapse into one.
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

/// Starts a controller. It runs for the life of the process; a panic inside a
/// tick is caught, recorded, and the controller carries on after a backoff.
pub fn spawn<C: Controller>(controller: C, leader: leader::Leadership) {
    let controller = Arc::new(controller);
    let name = controller.name();
    let notify = waker(name);
    registry().register(name, controller.scope(), controller.interval());
    tokio::spawn(async move {
        run(controller, notify, leader).await;
    });
}

async fn wait_or_wake(notify: &Notify, d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = notify.notified() => {}
    }
}

async fn run<C: Controller>(controller: Arc<C>, notify: Arc<Notify>, leader: leader::Leadership) {
    let name = controller.name();
    let reg = registry();
    let node = crate::system::hostname();

    // OnBootSec.
    let min_uptime = controller.not_before_uptime();
    if let Some(up) = uptime() {
        if up < min_uptime {
            reg.set_phase(
                name,
                Phase::Waiting(format!("starts {}s after boot", min_uptime.as_secs())),
            );
            tokio::time::sleep(min_uptime - up).await;
        }
    }

    let mut last_start: Option<Instant> = None;
    loop {
        if let Some(t) = last_start {
            let since = t.elapsed();
            if since < MIN_GAP {
                tokio::time::sleep(MIN_GAP - since).await;
            }
        }

        if controller.scope() == Scope::Cluster && !leader.is_leader() {
            reg.set_phase(name, Phase::Standby);
            wait_or_wake(&notify, RECHECK).await;
            continue;
        }

        if let Some(missing) = activity::unmet(controller.requires()).await {
            reg.set_phase(name, Phase::Waiting(format!("waiting for {missing}")));
            wait_or_wake(&notify, RECHECK).await;
            continue;
        }

        if let activity::Gate::Paused(why) = activity::gate(controller.pauses_during()).await {
            reg.set_phase(name, Phase::Paused(why));
            wait_or_wake(&notify, RECHECK).await;
            continue;
        }

        last_start = Some(Instant::now());
        reg.started(name);
        let c = controller.clone();
        let ctx = Ctx::new(node.clone(), controller.scope(), &leader);
        let outcome = tokio::spawn(async move { c.reconcile(&ctx).await }).await;

        let interval = controller.interval();
        let next = match outcome {
            Ok(Ok(Tick::Done)) => {
                reg.succeeded(name, None);
                interval
            }
            Ok(Ok(Tick::Idle(why))) => {
                reg.succeeded(name, Some(why));
                interval
            }
            Ok(Ok(Tick::RequeueAfter(d))) => {
                reg.succeeded(name, None);
                d.min(interval)
            }
            Ok(Err(e)) => {
                let n = reg.failed(name, format!("{e:#}"));
                tracing::warn!("controller {name}: {e:#}");
                failure_backoff(n, interval)
            }
            Err(join) => {
                let n = reg.panicked(name, join.to_string());
                tracing::error!("controller {name}: tick panicked ({join}) — continuing");
                failure_backoff(n, interval)
            }
        };
        status::publish_snapshot();
        wait_or_wake(&notify, next).await;
    }
}

/// Runs a controller's tick once, outside the loop — the `local-api run <name>`
/// subcommand, so a person over SSH can drive exactly what the daemon would.
///
/// Under the same gates the daemon applies, or it would be a way around them:
/// `local-api run disks` during a restore would drain and purge OSDs underneath
/// it, and `local-api run storage-heal` on a standby would be a second writer
/// next to the leader. A refusal says which gate and why.
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
    // The lease was just seen live, and nothing renews it from this process: past
    // the same window the daemon allows itself, a long manual tick stops acting.
    let leader = leader::Leadership::confirmed_now();
    controller
        .reconcile(&Ctx::new(node, controller.scope(), &leader))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn failure_backoff_grows_but_never_exceeds_the_interval() {
        let interval = Duration::from_secs(120);
        assert_eq!(failure_backoff(1, interval), Duration::from_secs(10));
        assert_eq!(failure_backoff(2, interval), Duration::from_secs(20));
        assert_eq!(failure_backoff(4, interval), Duration::from_secs(80));
        assert_eq!(failure_backoff(10, interval), interval);
        // A controller with a short interval is never slowed below it.
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

    #[test]
    fn only_a_cluster_tick_can_lose_the_right_to_act() {
        let lost = leader::Leadership::fixed_for_tests(false);
        let held = leader::Leadership::fixed_for_tests(true);
        assert!(Ctx::new("n1".into(), Scope::Node, &lost).still_in_charge());
        assert!(!Ctx::new("n1".into(), Scope::Cluster, &lost).still_in_charge());
        assert!(Ctx::new("n1".into(), Scope::Cluster, &held).still_in_charge());
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
