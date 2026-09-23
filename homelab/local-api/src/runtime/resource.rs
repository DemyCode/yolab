//! yolabd's resource graph.
//!
//! A `Resource` is one thing about this machine that should be true: the mon is
//! in quorum, the images RBD exists, containerd's data-root is on it. Each one
//! knows what it depends on, how to tell whether it is already true, and how to
//! make it true.
//!
//! This exists because the same dependency graph is currently written down three
//! times and agrees with itself only by hand:
//!
//!   * as `After=`/`Wants=` edges between systemd oneshots, evaluated once per
//!     boot, one attempt each;
//!   * as `containerd-store-after-order` in nix/checks.nix, which re-derives
//!     those edges from the evaluated config to police them;
//!   * as `after_boot:` sleeps in storage/controllers.rs — `images-grow` waits
//!     600s after boot, which is not a schedule, it is "wait until
//!     containerd-store has probably mounted" written as a guess about
//!     wall-clock time.
//!
//! Here an edge is an edge. A resource that is not ready stalls exactly its
//! dependents and nothing else, and it says which dependency it is waiting on.
//!
//! ## Why this is not one serial pass over a sorted graph
//!
//! The obvious implementation — sort topologically, walk the list once per tick
//! — would be a regression against the per-controller tasks this replaces: one
//! slow resource would stall every unrelated one behind it. Instead each
//! resource keeps its own task and its own pacing, consults a shared readiness
//! map before doing work, and wakes its dependents when it becomes ready.
//! Ordering falls out of the gating rather than out of a scheduler.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::status::Phase;
use super::{activity, leader, Controller, Ctx, Requirement, Scope, Tick};

/// What a resource's cheap, read-only `check` found.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum State {
    /// Already true. Nothing to do, and dependents may proceed.
    Ready,
    /// This resource has no cheap check, so the only way to find out is to run
    /// `converge`, which is idempotent. Every adapted `Controller` reports this,
    /// which is what makes the adapter behave exactly like the loop it replaces.
    Unchecked,
    /// Not true yet, and that is not an error — a precondition outside this
    /// resource's control has not happened. Carries the reason, verbatim, for
    /// the status surface.
    NotYet(String),
    /// Broken in a way `converge` is expected to address.
    Failed(String),
}

impl State {
    /// Whether dependents are allowed to proceed.
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

/// One thing about this machine that should be true.
///
/// `check` must be cheap and side-effect free: it runs every pass, including
/// when the resource is already ready, and it is what the status surface is
/// built from. `converge` may do work, and is only ever called when `check` did
/// not say `Ready` *and* every dependency is ready.
pub trait Resource: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Names of resources that must be `Ready` before this one's `converge`
    /// runs. Every name must belong to a registered resource; `validate` is
    /// what enforces that.
    fn depends_on(&self) -> &'static [&'static str] {
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

/// Carries an existing `Controller` into the graph unchanged.
///
/// Not a blanket `impl<C: Controller> Resource for C`: coherence rejects that
/// (E0119) because the compiler may not assume a future type will not implement
/// `Controller` too. An explicit newtype also makes migration legible — a
/// resource has finished moving when it implements `Resource` directly and this
/// wrapper is gone from its registration.
pub struct ControllerResource<C>(pub C);

impl<C: Controller> Resource for ControllerResource<C> {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn scope(&self) -> Scope {
        self.0.scope()
    }

    fn interval(&self) -> Duration {
        self.0.interval()
    }

    fn requires(&self) -> &'static [Requirement] {
        self.0.requires()
    }

    fn pauses_during(&self) -> &'static [activity::Activity] {
        self.0.pauses_during()
    }

    fn not_before_uptime(&self) -> Duration {
        self.0.not_before_uptime()
    }

    /// A `Controller` has no cheap check — `reconcile` is all it offers, and it
    /// is idempotent. Reporting `Unchecked` means the supervisor always falls
    /// through to `converge` on the interval, which is what the loop this
    /// replaces did.
    async fn check(&self, _ctx: &Ctx) -> State {
        State::Unchecked
    }

    async fn converge(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        self.0.reconcile(ctx).await
    }
}

// ── the shared graph ──────────────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct Graph {
    edges: std::sync::Mutex<BTreeMap<&'static str, &'static [&'static str]>>,
    states: std::sync::RwLock<BTreeMap<&'static str, State>>,
}

fn graph() -> &'static Graph {
    &super::shared().graph
}

fn lock_edges<T>(f: impl FnOnce(&mut BTreeMap<&'static str, &'static [&'static str]>) -> T) -> T {
    let mut e = graph()
        .edges
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut e)
}

fn record_edges(name: &'static str, deps: &'static [&'static str]) {
    lock_edges(|e| e.insert(name, deps));
}

fn set_state(name: &'static str, state: State) {
    graph()
        .states
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name, state);
}

/// The last `check` result for a resource, or `None` if it has never reported —
/// which is the state of anything whose task has not had its first pass yet.
pub fn state_of(name: &str) -> Option<State> {
    graph()
        .states
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

/// Every resource's last known state, for the status surface.
pub fn states() -> BTreeMap<&'static str, State> {
    graph()
        .states
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The first dependency of `name` that is not ready, if any.
///
/// A dependency that has never reported counts as not ready: at startup every
/// task begins at once, and letting an unreported dependency pass would be a
/// race that usually resolves the right way and occasionally does not.
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

/// Wake everything that lists `name` as a dependency, so a resource that has
/// just become ready releases its dependents immediately rather than leaving
/// them to notice on their next poll.
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

/// Everything wrong with the registered graph: a dependency naming a resource
/// that does not exist, or a cycle. Empty means the graph is sound.
///
/// This is the check that replaces `containerd-store-after-order`: that one had
/// to re-derive the edges out of an evaluated NixOS config to police them, and
/// this one reads the graph itself.
pub fn problems() -> Vec<String> {
    problems_in(&lock_edges(|e| e.clone()))
}

/// The pure half, so it can be tested without touching the process-wide graph.
fn problems_in(edges: &BTreeMap<&'static str, &'static [&'static str]>) -> Vec<String> {
    let mut out = Vec::new();

    for (name, deps) in edges {
        for dep in *deps {
            if !edges.contains_key(dep) {
                out.push(format!(
                    "{name} depends on '{dep}', which is not a registered resource"
                ));
            }
        }
    }

    // Depth-first, tracking the path so a cycle can be named rather than merely
    // reported. `done` keeps this linear rather than exponential on diamonds.
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
    edges: &BTreeMap<&'static str, &'static [&'static str]>,
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
    for dep in edges.get(name).copied().unwrap_or(&[]) {
        if let Some(cycle) = walk(dep, edges, done, path, on_path) {
            return Some(cycle);
        }
    }
    on_path.remove(name);
    path.pop();
    done.insert(name);
    None
}

// ── the supervisor ────────────────────────────────────────────────────────────

pub fn spawn<R: Resource>(resource: R, leader: leader::Leadership) {
    let resource = Arc::new(resource);
    let name = resource.name();
    record_edges(name, resource.depends_on());
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

    // Only on the transition, so a resource that is simply fine does not wake
    // its dependents every interval forever.
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

        // The cheap half. Runs every pass, including when nothing needs doing,
        // and is the only thing the status surface is built from.
        let state = resource.check(&ctx).await;

        // `Unchecked` is the absence of information, not a state, so it must not
        // overwrite what a previous `converge` established. Overwriting it was a
        // flap: an adapted `Controller` would report Ready right after
        // converging and Unchecked on the very next pass, so anything depending
        // on it would see readiness blink on and off every interval.
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
            // The resource's own state, not just its phase: something blocked on
            // a blocked dependency has to carry a reason of its own, so that ITS
            // dependents are told what is actually wrong rather than a bare name.
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
                // With no cheap check, a successful idempotent converge is the
                // only evidence this resource is satisfied — and it is what
                // dependents have to go on.
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
    ) -> BTreeMap<&'static str, &'static [&'static str]> {
        pairs.iter().copied().collect()
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
        // Two paths to the same ancestor is the shape a naive depth-first walk
        // reports as a cycle, and it is a perfectly ordinary graph.
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
        fn depends_on(&self) -> &'static [&'static str] {
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
                // never ready on its own, so it always wants to converge —
                // anything stopping it is the dependency gate, not its check
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
        // The regression this guards: `check` reporting `Unchecked` used to
        // overwrite the state a successful `converge` had just set, so an
        // adapted controller blinked Ready/Unchecked every interval and nothing
        // could depend on one without flapping.
        let converges = Arc::new(AtomicU32::new(0));
        spawn(
            ControllerResource(Plain {
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

        // It must still run on its interval — being "ready" must not switch off
        // an adapted controller, which has no cheap check to re-derive it from.
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
}
