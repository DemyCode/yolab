use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::status::Phase;
use super::{activity, leader, Controller, Ctx, Requirement, Scope, Tick};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum State {
    Ready,
    Unchecked,
    NotYet(String),
    Failed(String),
}

impl State {
    pub fn settled(&self) -> bool {
        matches!(self, State::Ready)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            State::Ready | State::Unchecked => None,
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

    fn requires(&self) -> &'static [Requirement] {
        &[]
    }

    fn pauses_during(&self) -> &'static [activity::Activity] {
        &[]
    }

    fn not_before_uptime(&self) -> Duration {
        Duration::ZERO
    }

    fn check(&self, ctx: &Ctx) -> impl Future<Output = State> + Send;

    fn converge(&self, ctx: &Ctx) -> impl Future<Output = anyhow::Result<Tick>> + Send;
}

pub struct ControllerResource<C> {
    inner: C,
    deps: Vec<&'static str>,
}

impl<C: Controller> ControllerResource<C> {
    pub fn new(inner: C) -> Self {
        let deps = inner.requires().iter().map(|r| r.resource_name()).collect();
        Self { inner, deps }
    }
}

impl<C: Controller> Resource for ControllerResource<C> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn depends_on(&self) -> &[&'static str] {
        &self.deps
    }

    fn scope(&self) -> Scope {
        self.inner.scope()
    }

    fn interval(&self) -> Duration {
        self.inner.interval()
    }

    fn pauses_during(&self) -> &'static [activity::Activity] {
        self.inner.pauses_during()
    }

    fn not_before_uptime(&self) -> Duration {
        self.inner.not_before_uptime()
    }

    async fn check(&self, _ctx: &Ctx) -> State {
        State::Unchecked
    }

    async fn converge(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        self.inner.reconcile(ctx).await
    }
}

pub struct Observed(pub Requirement);

impl Resource for Observed {
    fn name(&self) -> &'static str {
        self.0.resource_name()
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    async fn check(&self, _ctx: &Ctx) -> State {
        if activity::is_met(self.0).await {
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

pub fn spawn<R: Resource>(resource: R, leader: leader::Leadership) {
    let resource = Arc::new(resource);
    let name = resource.name();
    record_edges(name, resource.depends_on().to_vec());
    let notify = super::waker(name);
    super::registry().register(name, resource.scope(), resource.interval());
    tokio::spawn(async move {
        run(resource, notify, leader).await;
    });
}

async fn run<R: Resource>(resource: Arc<R>, notify: Arc<Notify>, leader: leader::Leadership) {
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
            let since = t.elapsed();
            if since < super::MIN_GAP {
                tokio::time::sleep(super::MIN_GAP - since).await;
            }
        }

        let ctx = Ctx { node: node.clone() };
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
            reg.set_phase(name, Phase::Standby);
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        if let Some(missing) = activity::unmet(resource.requires()).await {
            reg.set_phase(name, Phase::Waiting(format!("waiting for {missing}")));
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        if let activity::Gate::Paused(why) = activity::gate(resource.pauses_during()).await {
            reg.set_phase(name, Phase::Paused(why));
            super::wait_or_wake(&notify, super::RECHECK).await;
            continue;
        }

        last_start = Some(Instant::now());
        reg.started(name);
        let r = resource.clone();
        let ctx = Ctx { node: node.clone() };
        let outcome = tokio::spawn(async move { r.converge(&ctx).await }).await;

        let next = match outcome {
            Ok(Ok(tick)) => {
                let (note, next) = match tick {
                    Tick::Done => (None, interval),
                    Tick::Idle(why) => (Some(why), interval),
                    Tick::RequeueAfter(d) => (None, d.min(interval)),
                };
                reg.succeeded(name, note);
                if unchecked {
                    becomes_ready!();
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
            ("containerd-store", &["images-rbd"]),
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
        let g = edges(&[("containerd-store", &["images-rbd"])]);
        let found = problems_in(&g);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("containerd-store depends on 'images-rbd'"),
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
        );
        spawn(
            Probe {
                name: "test-edge-dependent",
                deps: &["test-edge-dependency"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: dep_converges.clone(),
            },
            leader,
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
        match phase {
            Phase::Waiting(why) => assert!(
                why.contains("test-edge-dependency"),
                "the wait does not name the dependency: {why}"
            ),
            other => panic!("expected Waiting, got {other:?}"),
        }

        up.store(true, Ordering::SeqCst);
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if dep_converges.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert!(
            dep_converges.load(Ordering::SeqCst) > 0,
            "the dependency became ready and the dependent was never released"
        );
    }

    struct Plain {
        converges: Arc<AtomicU32>,
    }

    impl Controller for Plain {
        fn name(&self) -> &'static str {
            "test-adapted-controller"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            self.converges.fetch_add(1, Ordering::SeqCst);
            Ok(Tick::Done)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_adapted_controller_keeps_running_and_holds_its_readiness_steady() {
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            ControllerResource::new(Plain {
                converges: converges.clone(),
            }),
            leader::Leadership::fixed_for_tests(true),
        );

        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if state_of("test-adapted-controller") == Some(State::Ready) {
                break;
            }
        }
        assert_eq!(
            state_of("test-adapted-controller"),
            Some(State::Ready),
            "a controller that converged cleanly never became ready"
        );

        let before = converges.load(Ordering::SeqCst);
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                state_of("test-adapted-controller"),
                Some(State::Ready),
                "readiness flapped between passes"
            );
        }
        assert!(
            converges.load(Ordering::SeqCst) > before,
            "the adapted controller stopped running once it was ready"
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
        );
        spawn(
            Probe {
                name: "test-chain-middle",
                deps: &["test-chain-root"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader.clone(),
        );
        spawn(
            Probe {
                name: "test-chain-leaf",
                deps: &["test-chain-middle"],
                ready: Arc::new(AtomicBool::new(false)),
                converges: Arc::new(AtomicU32::new(0)),
            },
            leader,
        );

        tokio::time::sleep(Duration::from_secs(60)).await;

        match state_of("test-chain-middle") {
            Some(State::NotYet(why)) => assert!(why.contains("test-chain-root"), "{why}"),
            other => panic!("expected NotYet naming its dependency, got {other:?}"),
        }

        let leaf = super::super::registry()
            .get("test-chain-leaf")
            .expect("registered")
            .phase;
        match leaf {
            Phase::Waiting(why) => {
                assert!(why.contains("test-chain-middle"), "{why}");
                assert!(
                    why.contains("test-chain-root"),
                    "the leaf is told which name it waits on but not what is actually \
                     wrong underneath it: {why}"
                );
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
    }

    struct NeedsCeph;

    impl Controller for NeedsCeph {
        fn name(&self) -> &'static str {
            "test-needs-ceph"
        }
        fn scope(&self) -> Scope {
            Scope::Node
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn requires(&self) -> &'static [Requirement] {
            &[Requirement::Ceph]
        }
        async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
            Ok(Tick::Done)
        }
    }

    #[test]
    fn a_controllers_requirements_become_edges_to_observed_resources() {
        let r = ControllerResource::new(NeedsCeph);
        assert_eq!(r.depends_on(), &["ceph"]);
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
}
