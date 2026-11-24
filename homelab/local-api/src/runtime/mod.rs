pub mod activity;
pub mod fleet;
pub mod leader;
pub mod lock;
pub mod resource;
pub mod status;
pub mod watch;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

pub use activity::Activity;
pub use resource::{spawn, Resource};
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

pub const KUBE_API: &str = "kube-api";
pub const CEPH: &str = "ceph";

impl Requirement {
    pub const ALL: [Requirement; 2] = [Requirement::KubeApi, Requirement::Ceph];

    pub fn from_resource_name(name: &str) -> Option<Requirement> {
        Requirement::ALL
            .into_iter()
            .find(|r| r.resource_name() == name)
    }

    pub fn resource_name(self) -> &'static str {
        match self {
            Requirement::KubeApi => KUBE_API,
            Requirement::Ceph => CEPH,
        }
    }
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
    NotYet(String),
    RequeueAfter(Duration),
}

pub struct Ctx {
    pub node: String,
    pub tools: crate::tools::Tools,
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
    let raw = std::fs::read_to_string(crate::storage::on_machine("/proc/uptime")).ok()?;
    let secs: f64 = raw.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs))
}

async fn wait_or_wake(notify: &Notify, d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = notify.notified() => {}
    }
}

pub async fn run_once<R: Resource>(resource: &R) -> anyhow::Result<Tick> {
    run_once_with(crate::tools::Tools::real(), resource).await
}

async fn run_once_with<R: Resource>(
    tools: crate::tools::Tools,
    resource: &R,
) -> anyhow::Result<Tick> {
    let node = resource::preflight(
        &tools,
        resource.name(),
        resource.scope(),
        resource.depends_on(),
        resource.pauses_during(),
    )
    .await?;
    resource.converge(&Ctx { node, tools }).await
}

#[cfg(test)]
mod tests {
    use super::status::Phase;
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn on_machine(path: &str, body: &str) {
        let p = crate::storage::on_machine(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn the_uptime_is_read_from_the_kernel_and_unknown_when_unreadable() {
        assert_eq!(uptime(), None);
        on_machine("/proc/uptime", "");
        assert_eq!(uptime(), None);
        on_machine("/proc/uptime", "soon 1.0\n");
        assert_eq!(uptime(), None);
        on_machine("/proc/uptime", "12.5 3.0\n");
        assert_eq!(uptime(), Some(Duration::from_millis(12_500)));
    }

    #[test]
    fn a_requirement_is_found_by_its_resource_name_and_named_for_people() {
        assert_eq!(
            Requirement::from_resource_name(KUBE_API),
            Some(Requirement::KubeApi)
        );
        assert_eq!(
            Requirement::from_resource_name(CEPH),
            Some(Requirement::Ceph)
        );
        assert_eq!(Requirement::from_resource_name("disks"), None);
        assert_eq!(Requirement::KubeApi.to_string(), "the Kubernetes API");
        assert_eq!(Requirement::Ceph.to_string(), "the Ceph cluster");
    }

    #[tokio::test]
    async fn a_machine_controller_run_once_converges_at_once() {
        let runs = Arc::new(AtomicU32::new(0));
        let tick = run_once(&Woken { runs: runs.clone() }).await.unwrap();
        assert_eq!(tick, Tick::Done);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    fn lease_held_by(holder: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": leader::LEASE_NAME, "namespace": leader::LEASE_NS },
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": 30,
                "renewTime": chrono::Utc::now().to_rfc3339(),
            },
        })
    }

    async fn tools_with_lease(holder: &str) -> (wiremock::MockServer, crate::tools::Tools) {
        let (server, kube) = crate::k8s::testing::api_server().await;
        crate::k8s::testing::serve(
            &server,
            "/apis/coordination.k8s.io/v1/namespaces/kube-system/leases/yolab-leader",
            200,
            lease_held_by(holder),
        )
        .await;
        let tools = crate::tools::Tools {
            kube: crate::k8s::Kube::with(kube),
            ..crate::tools::Tools::unreachable()
        };
        (server, tools)
    }

    #[tokio::test]
    async fn a_cluster_controller_run_once_runs_only_on_the_leader() {
        on_machine("/etc/hostname", "node1\n");
        let runs = Arc::new(AtomicU32::new(0));
        let (_server, tools) = tools_with_lease("node1").await;
        let tick = run_once_with(tools, &Clustered { runs: runs.clone() })
            .await
            .unwrap();
        assert_eq!(tick, Tick::Done);

        let (_server, tools) = tools_with_lease("node2").await;
        let err = run_once_with(tools, &Clustered { runs: runs.clone() })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "test-clustered is cluster-scoped and node1 does not hold the cluster lease — run it on the leader"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        assert!(
            run_once_with(crate::tools::Tools::unreachable(), &Clustered { runs })
                .await
                .is_err()
        );
    }

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

    impl Resource for Flaky {
        fn name(&self) -> &'static str {
            "test-flaky"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
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
            crate::tools::Tools::unreachable(),
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

    impl Resource for Clustered {
        fn name(&self) -> &'static str {
            "test-clustered"
        }
        fn scope(&self) -> Scope {
            Scope::Cluster
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
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
            crate::tools::Tools::unreachable(),
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

    impl Resource for Woken {
        fn name(&self) -> &'static str {
            "test-woken"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
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
            crate::tools::Tools::unreachable(),
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

    impl Resource for Impatient {
        fn name(&self) -> &'static str {
            "test-impatient"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
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
            crate::tools::Tools::unreachable(),
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
