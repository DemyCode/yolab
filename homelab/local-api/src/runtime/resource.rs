use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::status::Phase;
use super::{activity, leader, Ctx, Requirement, Scope, Tick};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum State {
    Ready,
    Unchecked,
    NotYet(String),
    Standby,
    Failed(String),
}

impl State {
    pub fn settled(&self) -> bool {
        matches!(self, State::Ready)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            State::Ready | State::Unchecked | State::Standby => None,
            State::NotYet(why) | State::Failed(why) => Some(why),
        }
    }
}

pub trait Resource: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn depends_on(&self) -> &[&'static str] {
        &[]
    }

    fn scope(&self) -> Scope {
        Scope::Node
    }

    fn interval(&self) -> Duration;

    fn pauses_during(&self) -> &'static [activity::Activity] {
        &[]
    }

    fn not_before_uptime(&self) -> Duration {
        Duration::ZERO
    }

    fn check(&self, _ctx: &Ctx) -> impl Future<Output = State> + Send {
        async { State::Unchecked }
    }

    fn converge(&self, ctx: &Ctx) -> impl Future<Output = anyhow::Result<Tick>> + Send;
}

pub(crate) async fn preflight(
    tools: &crate::tools::Tools,
    name: &str,
    scope: Scope,
    deps: &[&'static str],
    pauses: &[activity::Activity],
) -> anyhow::Result<String> {
    let node = crate::system::hostname();
    if scope == Scope::Cluster && !leader::held_by(&tools.kube().await?, &node).await? {
        anyhow::bail!(
            "{name} is cluster-scoped and {node} does not hold the cluster lease — run it on the leader"
        );
    }
    let requires: Vec<Requirement> = deps
        .iter()
        .copied()
        .filter_map(Requirement::from_resource_name)
        .collect();
    if let Some(r) = activity::unmet(tools, &requires).await {
        anyhow::bail!("not running {name}: waiting for {r}");
    }
    if let activity::Gate::Paused(why) = activity::gate(tools, pauses).await {
        anyhow::bail!("not running {name}: {why}");
    }
    Ok(node)
}

pub struct Observed(pub Requirement);

impl Resource for Observed {
    fn name(&self) -> &'static str {
        self.0.resource_name()
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    async fn check(&self, ctx: &Ctx) -> State {
        if activity::is_met(&ctx.tools, self.0).await {
            State::Ready
        } else {
            State::NotYet(format!("{} is not answering", self.0))
        }
    }

    async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
        Ok(Tick::Idle(format!("{} is observed, not managed", self.0)))
    }
}

#[derive(Default)]
pub(super) struct Graph {
    edges: std::sync::Mutex<BTreeMap<&'static str, Vec<&'static str>>>,
    states: std::sync::RwLock<BTreeMap<&'static str, State>>,
}

fn graph() -> &'static Graph {
    &super::shared().graph
}

fn lock_edges<T>(f: impl FnOnce(&mut BTreeMap<&'static str, Vec<&'static str>>) -> T) -> T {
    let mut e = graph()
        .edges
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut e)
}

fn record_edges(name: &'static str, deps: Vec<&'static str>) {
    lock_edges(|e| e.insert(name, deps));
}

fn set_state(name: &'static str, state: State) {
    graph()
        .states
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name, state);
}

pub fn state_of(name: &str) -> Option<State> {
    graph()
        .states
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

pub fn states() -> BTreeMap<&'static str, State> {
    graph()
        .states
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn first_unready_dep(deps: &[&'static str]) -> Option<String> {
    for dep in deps {
        match state_of(dep) {
            Some(s) if s.settled() => {}
            Some(s) => {
                return Some(match s.reason() {
                    Some(why) => format!("{dep} ({why})"),
                    None => (*dep).to_string(),
                })
            }
            None => return Some(format!("{dep} (has not reported yet)")),
        }
    }
    None
}

fn wake_dependents(name: &'static str) {
    let dependents: Vec<&'static str> = lock_edges(|e| {
        e.iter()
            .filter(|(_, deps)| deps.contains(&name))
            .map(|(n, _)| *n)
            .collect()
    });
    for d in dependents {
        super::wake(d);
    }
}

pub fn problems() -> Vec<String> {
    problems_in(&lock_edges(|e| e.clone()))
}

fn problems_in(edges: &BTreeMap<&'static str, Vec<&'static str>>) -> Vec<String> {
    let mut out = Vec::new();

    for (name, deps) in edges {
        for dep in deps {
            if !edges.contains_key(dep) {
                out.push(format!(
                    "{name} depends on '{dep}', which is not a registered resource"
                ));
            }
        }
    }

    let mut done: BTreeSet<&'static str> = BTreeSet::new();
    for start in edges.keys() {
        let mut path: Vec<&'static str> = Vec::new();
        let mut on_path: BTreeSet<&'static str> = BTreeSet::new();
        if let Some(cycle) = walk(start, edges, &mut done, &mut path, &mut on_path) {
            out.push(format!("dependency cycle: {}", cycle.join(" -> ")));
        }
    }

    out.sort();
    out.dedup();
    out
}

fn walk(
    name: &'static str,
    edges: &BTreeMap<&'static str, Vec<&'static str>>,
    done: &mut BTreeSet<&'static str>,
    path: &mut Vec<&'static str>,
    on_path: &mut BTreeSet<&'static str>,
) -> Option<Vec<&'static str>> {
    if done.contains(name) {
        return None;
    }
    if on_path.contains(name) {
        let from = path.iter().position(|n| *n == name).unwrap_or(0);
        let mut cycle: Vec<&'static str> = path[from..].to_vec();
        cycle.push(name);
        return Some(cycle);
    }
    path.push(name);
    on_path.insert(name);
    for dep in edges.get(name).map(Vec::as_slice).unwrap_or(&[]) {
        if let Some(cycle) = walk(dep, edges, done, path, on_path) {
            return Some(cycle);
        }
    }
    on_path.remove(name);
    path.pop();
    done.insert(name);
    None
}

pub fn spawn<R: Resource>(resource: R, leader: leader::Leadership, tools: crate::tools::Tools) {
    let resource = Arc::new(resource);
    let name = resource.name();
    record_edges(name, resource.depends_on().to_vec());
    let notify = super::waker(name);
    super::registry().register(name, resource.scope(), resource.interval());
    tokio::spawn(run(resource, notify, leader, tools));
}

async fn run<R: Resource>(
    resource: Arc<R>,
    notify: Arc<Notify>,
    leader: leader::Leadership,
    tools: crate::tools::Tools,
) {
    let name = resource.name();
    let reg = super::registry();
    let node = crate::system::hostname();

    let min_uptime = resource.not_before_uptime();
    if let Some(up) = super::uptime() {
        if up < min_uptime {
            reg.set_phase(
                name,
                Phase::Waiting(format!("starts {}s after boot", min_uptime.as_secs())),
            );
            tokio::time::sleep(min_uptime - up).await;
        }
    }

    let mut last_start: Option<Instant> = None;
    let mut was_settled = false;
    let mut last_pause: Option<String> = None;

    macro_rules! becomes_ready {
        () => {{
            set_state(name, State::Ready);
            if !was_settled {
                wake_dependents(name);
            }
            was_settled = true;
        }};
    }

    loop {
        if let Some(t) = last_start {
            tokio::time::sleep(super::MIN_GAP.saturating_sub(t.elapsed())).await;
        }

        let ctx = Ctx {
            node: node.clone(),
            tools: tools.clone(),
        };
        let interval = resource.interval();

        let state = resource.check(&ctx).await;

        let unchecked = matches!(state, State::Unchecked);
        if !unchecked {
            set_state(name, state.clone());
            if state.settled() {
                becomes_ready!();
                reg.succeeded(name, None);
                super::status::publish_snapshot();
                super::wait_or_wake(&notify, interval).await;
                continue;
            }
            was_settled = false;
        }

        if let Some(dep) = first_unready_dep(resource.depends_on()) {
            let why = format!("waiting on {dep}");
            set_state(name, State::NotYet(why.clone()));
            was_settled = false;
            reg.set_phase(name, Phase::Waiting(why));
            super::status::publish_snapshot();
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        if resource.scope() == Scope::Cluster && !leader.is_leader() {
            set_state(name, State::Standby);
            was_settled = false;
            reg.set_phase(name, Phase::Standby);
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        if let activity::Gate::Paused(why) = activity::gate(&tools, resource.pauses_during()).await
        {
            if last_pause.as_deref() != Some(why.as_str()) {
                tracing::warn!("resource {name}: paused, {why}");
                last_pause = Some(why.clone());
            }
            set_state(name, State::NotYet(why.clone()));
            was_settled = false;
            reg.set_phase(name, Phase::Paused(why));
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        last_pause = None;
        last_start = Some(Instant::now());
        reg.started(name);
        let r = resource.clone();
        let ctx = Ctx {
            node: node.clone(),
            tools: tools.clone(),
        };
        let outcome = tokio::spawn(async move { r.converge(&ctx).await }).await;

        let next = match outcome {
            Ok(Ok(tick)) => {
                let (note, next, ready) = match tick {
                    Tick::NotYet(why) => {
                        set_state(name, State::NotYet(why.clone()));
                        (Some(why), interval, false)
                    }
                    Tick::Done => (None, interval, unchecked),
                    Tick::Idle(why) => (Some(why), interval, unchecked),
                    Tick::RequeueAfter(d) => (None, d.min(interval), unchecked),
                };
                reg.succeeded(name, note);
                if ready {
                    becomes_ready!();
                } else {
                    was_settled = false;
                }
                next
            }
            Ok(Err(e)) => {
                let why = format!("{e:#}");
                set_state(name, State::Failed(why.clone()));
                was_settled = false;
                let n = reg.failed(name, why);
                tracing::warn!("resource {name}: {e:#}");
                super::failure_backoff(n, interval)
            }
            Err(join) => {
                set_state(name, State::Failed(format!("panicked: {join}")));
                was_settled = false;
                let n = reg.panicked(name, join.to_string());
                tracing::error!("resource {name}: converge panicked ({join}) — continuing");
                super::failure_backoff(n, interval)
            }
        };
        super::status::publish_snapshot();
        super::wait_or_wake(&notify, next).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    fn edges(
        pairs: &[(&'static str, &'static [&'static str])],
    ) -> BTreeMap<&'static str, Vec<&'static str>> {
        pairs.iter().map(|(n, d)| (*n, d.to_vec())).collect()
    }

    #[test]
    fn a_sound_graph_has_nothing_wrong_with_it() {
        let g = edges(&[
            ("mon", &[]),
            ("system-osd", &["mon"]),
            ("images-rbd", &["system-osd"]),
            ("image-store", &["images-rbd"]),
        ]);
        assert_eq!(problems_in(&g), Vec::<String>::new());
    }

    #[test]
    fn a_diamond_is_not_a_cycle() {
        let g = edges(&[
            ("mon", &[]),
            ("left", &["mon"]),
            ("right", &["mon"]),
            ("join", &["left", "right"]),
        ]);
        assert_eq!(problems_in(&g), Vec::<String>::new());
    }

    #[test]
    fn a_dependency_on_something_unregistered_is_reported() {
        let g = edges(&[("image-store", &["images-rbd"])]);
        let found = problems_in(&g);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("image-store depends on 'images-rbd'"),
            "{found:?}"
        );
    }

    #[test]
    fn a_cycle_is_reported_and_named() {
        let g = edges(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]);
        let found = problems_in(&g);
        assert!(
            found.iter().any(|p| p.starts_with("dependency cycle:")),
            "{found:?}"
        );
        let cycle = found
            .iter()
            .find(|p| p.starts_with("dependency cycle:"))
            .unwrap();
        for name in ["a", "b", "c"] {
            assert!(cycle.contains(name), "{cycle}");
        }
    }

    #[test]
    fn a_resource_that_depends_on_itself_is_a_cycle() {
        let g = edges(&[("a", &["a"])]);
        assert!(
            problems_in(&g)
                .iter()
                .any(|p| p.starts_with("dependency cycle:")),
            "a self-edge went unreported"
        );
    }

    struct Probe {
        name: &'static str,
        deps: &'static [&'static str],
        ready: Arc<AtomicBool>,
        converges: Arc<AtomicU32>,
    }

    impl Resource for Probe {
        fn name(&self) -> &'static str {
            self.name
        }
        fn depends_on(&self) -> &[&'static str] {
            self.deps
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn check(&self, _ctx: &Ctx) -> State {
            if self.ready.load(Ordering::SeqCst) {
                State::Ready
            } else {
                State::NotYet("the probe has not been switched on".into())
            }
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.converges.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_that_is_already_ready_is_never_converged() {
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            Probe {
                name: "test-already-ready",
                deps: &[],
                ready: Arc::new(AtomicBool::new(true)),
                converges: converges.clone(),
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            converges.load(Ordering::SeqCst),
            0,
            "a cheap check said Ready, so no work should have been done"
        );
        assert_eq!(state_of("test-already-ready"), Some(State::Ready));
    }

    #[tokio::test(start_paused = true)]
    async fn a_dependent_waits_for_its_dependency_and_says_which_one() {
        let up = Arc::new(AtomicBool::new(false));
        let dep_converges = Arc::new(AtomicU32::new(0));
        let leader = leader::Leadership::fixed_for_tests(true);

        spawn(
            Probe {
                name: "test-edge-dependency",
                deps: &[],
                ready: up.clone(),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader.clone(),
            crate::tools::Tools::unreachable(),
        );
        spawn(
            Probe {
                name: "test-edge-dependent",
                deps: &["test-edge-dependency"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: dep_converges.clone(),
            },
            leader,
            crate::tools::Tools::unreachable(),
        );

        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            dep_converges.load(Ordering::SeqCst),
            0,
            "the dependent converged while its dependency was not ready"
        );
        let phase = super::super::registry()
            .get("test-edge-dependent")
            .expect("registered")
            .phase;
        assert_eq!(
            phase,
            Phase::Waiting(
                "waiting on test-edge-dependency (the probe has not been switched on)".into()
            )
        );

        up.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            dep_converges.load(Ordering::SeqCst) > 0,
            "the dependency became ready and the dependent was never released"
        );
    }

    struct Plain {
        converges: Arc<AtomicU32>,
    }

    impl Resource for Plain {
        fn name(&self) -> &'static str {
            "test-unchecked-resource"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.converges.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test]
    async fn a_resource_that_does_not_check_is_unchecked_not_ready() {
        let plain = Plain {
            converges: Arc::new(AtomicU32::new(0)),
        };
        let ctx = Ctx {
            node: "test-node".into(),
            tools: crate::tools::Tools::unreachable(),
        };
        assert_eq!(plain.check(&ctx).await, State::Unchecked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_without_a_check_keeps_running_and_holds_its_readiness_steady() {
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            Plain {
                converges: converges.clone(),
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            state_of("test-unchecked-resource"),
            Some(State::Ready),
            "a resource that converged cleanly never became ready"
        );

        let before = converges.load(Ordering::SeqCst);
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                state_of("test-unchecked-resource"),
                Some(State::Ready),
                "readiness flapped between passes"
            );
        }
        assert!(
            converges.load(Ordering::SeqCst) > before,
            "the resource stopped running once it was ready"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_blocked_on_a_dependency_reports_that_as_its_own_state() {
        let leader = leader::Leadership::fixed_for_tests(true);
        spawn(
            Probe {
                name: "test-chain-root",
                deps: &[],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader.clone(),
            crate::tools::Tools::unreachable(),
        );
        spawn(
            Probe {
                name: "test-chain-middle",
                deps: &["test-chain-root"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader.clone(),
            crate::tools::Tools::unreachable(),
        );
        spawn(
            Probe {
                name: "test-chain-leaf",
                deps: &["test-chain-middle"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader,
            crate::tools::Tools::unreachable(),
        );

        tokio::time::sleep(Duration::from_secs(60)).await;

        assert_eq!(
            state_of("test-chain-middle"),
            Some(State::NotYet(
                "waiting on test-chain-root (the probe has not been switched on)".into()
            ))
        );

        let leaf = super::super::registry()
            .get("test-chain-leaf")
            .expect("registered")
            .phase;
        assert_eq!(
            leaf,
            Phase::Waiting(
                "waiting on test-chain-middle (waiting on test-chain-root (the probe has not been switched on))"
                    .into()
            )
        );
    }

    #[test]
    fn only_a_resource_that_is_not_yet_or_failed_has_a_reason() {
        assert_eq!(State::Ready.reason(), None);
        assert_eq!(State::Unchecked.reason(), None);
        assert_eq!(State::Standby.reason(), None);
        assert_eq!(State::NotYet("a".into()).reason(), Some("a"));
        assert_eq!(State::Failed("b".into()).reason(), Some("b"));
    }

    #[tokio::test]
    async fn a_resource_run_by_hand_waits_for_what_it_needs_and_what_it_pauses_for() {
        let tools = crate::tools::Tools::unreachable();
        assert_eq!(
            preflight(&tools, "x", Scope::Node, &[crate::runtime::CEPH], &[])
                .await
                .unwrap_err()
                .to_string(),
            "not running x: waiting for the Ceph cluster"
        );
        assert_eq!(
            preflight(
                &tools,
                "x",
                Scope::Node,
                &[],
                &[activity::Activity::Restore]
            )
            .await
            .unwrap_err()
            .to_string(),
            "not running x: cannot tell whether an app restore is running"
        );
        std::fs::create_dir_all(crate::storage::on_machine("/etc")).unwrap();
        std::fs::write(crate::storage::on_machine("/etc/hostname"), "node1\n").unwrap();
        assert_eq!(
            preflight(&tools, "x", Scope::Node, &["not-a-requirement"], &[])
                .await
                .unwrap(),
            "node1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_observed_requirement_is_ready_while_it_answers_and_never_managed() {
        let ceph_answers = crate::tools::Tools {
            host: crate::host::fake::FakeHost::new()
                .ok("ceph --connect-timeout 10 health", "HEALTH_OK"),
            ..crate::tools::Tools::unreachable()
        };
        let ctx = Ctx {
            node: "node1".into(),
            tools: ceph_answers,
        };
        let ceph = Observed(Requirement::Ceph);
        assert_eq!(ceph.interval(), Duration::from_secs(15));
        assert_eq!(ceph.check(&ctx).await, State::Ready);
        let silent = Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools::unreachable(),
        };
        let api = Observed(Requirement::KubeApi);
        assert_eq!(
            api.check(&silent).await,
            State::NotYet("the Kubernetes API is not answering".into())
        );
        assert_eq!(
            api.converge(&silent).await.unwrap(),
            Tick::Idle("the Kubernetes API is observed, not managed".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dependency_on_an_unregistered_resource_is_a_problem_of_the_running_graph() {
        spawn(
            Probe {
                name: "test-orphan",
                deps: &["test-nobody"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        assert!(problems().contains(
            &"test-orphan depends on 'test-nobody', which is not a registered resource".to_string()
        ));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            state_of("test-orphan"),
            Some(State::NotYet(
                "waiting on test-nobody (has not reported yet)".into()
            ))
        );
    }

    struct Late {
        converges: Arc<AtomicU32>,
    }

    impl Resource for Late {
        fn name(&self) -> &'static str {
            "test-late"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn not_before_uptime(&self) -> Duration {
            Duration::from_secs(60)
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.converges.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_that_starts_late_waits_out_the_boot() {
        let uptime = crate::storage::on_machine("/proc/uptime");
        std::fs::create_dir_all(uptime.parent().unwrap()).unwrap();
        std::fs::write(&uptime, "10.0 1.0\n").unwrap();
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            Late {
                converges: converges.clone(),
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(converges.load(Ordering::SeqCst), 0);
        assert_eq!(
            super::super::registry()
                .get("test-late")
                .expect("registered")
                .phase,
            Phase::Waiting("starts 60s after boot".into())
        );
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(state_of("test-late"), Some(State::Ready));
    }

    struct WaitsForRestores;

    impl Resource for WaitsForRestores {
        fn name(&self) -> &'static str {
            "test-waits-for-restores"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn pauses_during(&self) -> &'static [activity::Activity] {
            &[activity::Activity::Restore]
        }
        async fn check(&self, _ctx: &Ctx) -> State {
            State::Failed("not converged yet".into())
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_pauses_while_it_cannot_tell_whether_a_restore_runs() {
        spawn(
            WaitsForRestores,
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(40)).await;
        let why = "cannot tell whether an app restore is running";
        assert_eq!(
            state_of("test-waits-for-restores"),
            Some(State::NotYet(why.into()))
        );
        assert_eq!(
            super::super::registry()
                .get("test-waits-for-restores")
                .expect("registered")
                .phase,
            Phase::Paused(why.into())
        );
        assert_eq!(
            WaitsForRestores
                .converge(&Ctx {
                    node: "node1".into(),
                    tools: crate::tools::Tools::unreachable(),
                })
                .await
                .unwrap(),
            Tick::Done
        );
    }

    #[test]
    fn every_requirement_names_a_distinct_observed_resource() {
        let names: BTreeSet<&str> = Requirement::ALL
            .iter()
            .map(|r| Observed(*r).name())
            .collect();
        assert_eq!(
            names.len(),
            Requirement::ALL.len(),
            "two requirements share a resource name, so one shadows the other"
        );
        assert!(names.iter().all(|n| !n.is_empty()));
    }

    #[test]
    fn a_controller_waits_on_a_requirement_no_one_registers() {
        let g = edges(&[("test-needs-ceph", &["ceph"])]);
        let found = problems_in(&g);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("'ceph'"), "{found:?}");
    }

    struct NeverDone {
        converges: Arc<AtomicU32>,
    }

    impl Resource for NeverDone {
        fn name(&self) -> &'static str {
            "test-never-done"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn check(&self, _ctx: &Ctx) -> State {
            State::Unchecked
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.converges.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::NotYet("no OSD is up yet".into()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_converge_that_reports_not_yet_never_becomes_ready() {
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            NeverDone {
                converges: converges.clone(),
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(converges.load(Ordering::SeqCst) > 1, "it stopped retrying");
        assert_eq!(
            state_of("test-never-done"),
            Some(State::NotYet("no OSD is up yet".into()))
        );
    }

    #[tokio::test]
    async fn a_cluster_resource_run_by_hand_without_a_cluster_is_an_error() {
        assert_eq!(
            preflight(&crate::tools::Tools::real(), "x", Scope::Cluster, &[], &[])
                .await
                .unwrap_err()
                .to_string(),
            "this test runs without a cluster"
        );
    }

    #[derive(Clone)]
    enum Step {
        Tick(Tick),
        Fail,
        Panic,
    }

    struct Scripted {
        name: &'static str,
        deps: &'static [&'static str],
        scope: Scope,
        pauses: &'static [activity::Activity],
        not_before: Duration,
        checks: Vec<State>,
        steps: Vec<Step>,
        checked: std::sync::atomic::AtomicUsize,
        converges: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn scripted(name: &'static str) -> Scripted {
        Scripted {
            name,
            deps: &[],
            scope: Scope::Node,
            pauses: &[],
            not_before: Duration::ZERO,
            checks: vec![State::Unchecked],
            steps: vec![Step::Tick(Tick::Done)],
            checked: std::sync::atomic::AtomicUsize::new(0),
            converges: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    impl Resource for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }
        fn depends_on(&self) -> &[&'static str] {
            self.deps
        }
        fn scope(&self) -> Scope {
            self.scope
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn pauses_during(&self) -> &'static [activity::Activity] {
            self.pauses
        }
        fn not_before_uptime(&self) -> Duration {
            self.not_before
        }
        async fn check(&self, _ctx: &Ctx) -> State {
            let n = self.checked.fetch_add(1, Ordering::SeqCst);
            self.checks[n.min(self.checks.len() - 1)].clone()
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            let n = self.converges.fetch_add(1, Ordering::SeqCst);
            match self.steps[n.min(self.steps.len() - 1)].clone() {
                Step::Tick(tick) => Ok(tick),
                Step::Fail => anyhow::bail!("the script fails here"),
                Step::Panic => panic!("the script panics here"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_lives_through_every_outcome_a_pass_can_have_and_ends_ready() {
        let walk = Scripted {
            steps: vec![
                Step::Tick(Tick::Done),
                Step::Tick(Tick::Done),
                Step::Tick(Tick::Idle("resting".into())),
                Step::Tick(Tick::RequeueAfter(Duration::from_millis(500))),
                Step::Tick(Tick::NotYet("halfway".into())),
                Step::Fail,
                Step::Panic,
                Step::Tick(Tick::Done),
            ],
            ..scripted("test-scripted-walk")
        };
        let converges = walk.converges.clone();
        spawn(
            walk,
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(converges.load(Ordering::SeqCst) >= 8);
        let s = super::super::registry()
            .get("test-scripted-walk")
            .expect("registered");
        assert_eq!(s.panics, 1);
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(state_of("test-scripted-walk"), Some(State::Ready));
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_turns_ready_stops_the_converging() {
        let settles = Scripted {
            checks: vec![State::NotYet("warming up".into()), State::Ready],
            ..scripted("test-scripted-settles")
        };
        let converges = settles.converges.clone();
        spawn(
            settles,
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(converges.load(Ordering::SeqCst), 1);
        assert_eq!(state_of("test-scripted-settles"), Some(State::Ready));
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_resource_that_starts_after_the_current_uptime_waits_for_the_boot() {
        let uptime = crate::storage::on_machine("/proc/uptime");
        std::fs::create_dir_all(uptime.parent().unwrap()).unwrap();
        std::fs::write(&uptime, "10.0 1.0\n").unwrap();
        let leader = leader::Leadership::fixed_for_tests(true);
        spawn(
            Scripted {
                not_before: Duration::from_secs(60),
                ..scripted("test-scripted-late")
            },
            leader.clone(),
            crate::tools::Tools::unreachable(),
        );
        spawn(
            scripted("test-scripted-booted"),
            leader,
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            super::super::registry()
                .get("test-scripted-late")
                .expect("registered")
                .phase,
            Phase::Waiting("starts 60s after boot".into())
        );
        assert_eq!(state_of("test-scripted-booted"), Some(State::Ready));
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(state_of("test-scripted-late"), Some(State::Ready));
    }

    #[tokio::test(start_paused = true)]
    async fn a_dependent_of_a_standby_resource_waits_on_it_by_name_while_the_leader_runs_its_own() {
        let standby = leader::Leadership::fixed_for_tests(false);
        spawn(
            Scripted {
                scope: Scope::Cluster,
                ..scripted("test-scripted-standby")
            },
            standby.clone(),
            crate::tools::Tools::unreachable(),
        );
        spawn(
            Scripted {
                deps: &["test-scripted-standby"],
                ..scripted("test-scripted-behind-standby")
            },
            standby,
            crate::tools::Tools::unreachable(),
        );
        spawn(
            Scripted {
                scope: Scope::Cluster,
                ..scripted("test-scripted-led")
            },
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(state_of("test-scripted-standby"), Some(State::Standby));
        assert_eq!(
            state_of("test-scripted-behind-standby"),
            Some(State::NotYet("waiting on test-scripted-standby".into()))
        );
        assert_eq!(state_of("test-scripted-led"), Some(State::Ready));
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_paused_pass_after_pass_never_converges() {
        let paused = Scripted {
            pauses: &[activity::Activity::Restore],
            ..scripted("test-scripted-paused")
        };
        let converges = paused.converges.clone();
        spawn(
            paused,
            leader::Leadership::fixed_for_tests(true),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(40)).await;
        let why = "cannot tell whether an app restore is running";
        assert_eq!(converges.load(Ordering::SeqCst), 0);
        assert_eq!(
            state_of("test-scripted-paused"),
            Some(State::NotYet(why.into()))
        );
        assert_eq!(
            super::super::registry()
                .get("test-scripted-paused")
                .expect("registered")
                .phase,
            Phase::Paused(why.into())
        );
    }

    struct ClusterProbe {
        ready: Arc<AtomicBool>,
    }

    impl Resource for ClusterProbe {
        fn name(&self) -> &'static str {
            "test-standby-truth"
        }
        fn depends_on(&self) -> &[&'static str] {
            &["test-standby-dep"]
        }
        fn scope(&self) -> Scope {
            Scope::Cluster
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn check(&self, _ctx: &Ctx) -> State {
            State::Unchecked
        }
        async fn converge(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.ready.store(true, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_on_a_standby_machine_says_so_instead_of_blaming_a_dependency() {
        let converged = Arc::new(AtomicBool::new(false));
        let leader = leader::Leadership::fixed_for_tests(false);

        spawn(
            ClusterProbe {
                ready: converged.clone(),
            },
            leader.clone(),
            crate::tools::Tools::unreachable(),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            state_of("test-standby-truth"),
            Some(State::NotYet(
                "waiting on test-standby-dep (has not reported yet)".into()
            ))
        );

        spawn(
            Probe {
                name: "test-standby-dep",
                deps: &[],
                ready: Arc::new(AtomicBool::new(true)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader,
            crate::tools::Tools::unreachable(),
        );

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            state_of("test-standby-truth"),
            Some(State::Standby),
            "its dependency is ready and this machine is not the leader, so the state \
             must say standby — leaving the old dependency reason behind makes the \
             Storage page blame Ceph for work another machine owns"
        );
        assert!(
            !converged.load(Ordering::SeqCst),
            "cluster-scoped work must not run on a standby machine"
        );
        let leader_ctx = Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools::unreachable(),
        };
        ClusterProbe {
            ready: converged.clone(),
        }
        .converge(&leader_ctx)
        .await
        .unwrap();
        assert!(
            converged.load(Ordering::SeqCst),
            "only the leader converges it"
        );
    }
}
