use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use super::Requirement;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Activity {
    Restore,
}

impl Activity {
    fn describe(self) -> &'static str {
        match self {
            Activity::Restore => "an app restore is running",
        }
    }

    fn unknown(self) -> &'static str {
        match self {
            Activity::Restore => "cannot tell whether an app restore is running",
        }
    }
}

pub enum Gate {
    Clear,
    Paused(String),
}

const CACHE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Req(Requirement),
    Act(Activity),
}

type Answer = Option<bool>;

#[derive(Default)]
struct Cache(Mutex<HashMap<Key, (Instant, Answer)>>);

impl Cache {
    fn global() -> &'static Cache {
        static C: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
        C.get_or_init(Cache::default)
    }

    async fn get_or_ask(&self, key: Key, ask: impl Future<Output = Answer>) -> Answer {
        {
            let c = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, a)) = c.get(&key) {
                if at.elapsed() < CACHE_FOR {
                    return *a;
                }
            }
        }
        let a = ask.await;
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, (Instant::now(), a));
        a
    }
}

async fn kube_api_ready() -> Answer {
    let Ok(client) = crate::k8s::client().await else {
        return Some(false);
    };
    Some(crate::k8s::ready(&client).await)
}

async fn ceph_ready() -> Answer {
    Some(ceph_answers(&crate::host::RealHost).await)
}

async fn ceph_answers<H: crate::host::Host>(host: &H) -> bool {
    host.ceph(&["--connect-timeout", "10", "health"])
        .await
        .is_ok()
}

async fn ask_requirement(r: Requirement) -> Answer {
    match r {
        Requirement::KubeApi => kube_api_ready().await,
        Requirement::Ceph => ceph_ready().await,
    }
}

async fn ask_activity(a: Activity) -> Answer {
    let r = match a {
        Activity::Restore => match crate::k8s::client().await {
            Ok(client) => crate::routers::restore::running_anywhere(&client).await,
            Err(e) => Err(e),
        },
    };
    r.map_err(|e| tracing::debug!("activity {a:?}: {e:#}")).ok()
}

pub async fn is_met(r: Requirement) -> bool {
    Cache::global()
        .get_or_ask(Key::Req(r), ask_requirement(r))
        .await
        == Some(true)
}

pub async fn unmet(requires: &[Requirement]) -> Option<Requirement> {
    first_unmet(Cache::global(), requires, ask_requirement).await
}

async fn first_unmet<F, Fut>(cache: &Cache, requires: &[Requirement], ask: F) -> Option<Requirement>
where
    F: Fn(Requirement) -> Fut,
    Fut: Future<Output = Answer>,
{
    for r in requires {
        if cache.get_or_ask(Key::Req(*r), ask(*r)).await != Some(true) {
            return Some(*r);
        }
    }
    None
}

pub async fn gate(activities: &[Activity]) -> Gate {
    gate_in(Cache::global(), activities, ask_activity).await
}

async fn gate_in<F, Fut>(cache: &Cache, activities: &[Activity], ask: F) -> Gate
where
    F: Fn(Activity) -> Fut,
    Fut: Future<Output = Answer>,
{
    for a in activities {
        let answer = cache.get_or_ask(Key::Act(*a), ask(*a)).await;
        match decide(*a, answer) {
            Gate::Clear => {}
            paused => return paused,
        }
    }
    Gate::Clear
}

fn decide(a: Activity, answer: Answer) -> Gate {
    match answer {
        Some(false) => Gate::Clear,
        Some(true) => Gate::Paused(a.describe().to_string()),
        None => Gate::Paused(a.unknown().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_knowing_whether_a_restore_runs_pauses_like_one_running() {
        assert!(matches!(
            decide(Activity::Restore, Some(false)),
            Gate::Clear
        ));
        match decide(Activity::Restore, Some(true)) {
            Gate::Paused(why) => assert!(why.contains("restore is running")),
            Gate::Clear => panic!("a running restore must pause"),
        }
        match decide(Activity::Restore, None) {
            Gate::Paused(why) => assert!(why.contains("cannot tell")),
            Gate::Clear => panic!("an unknown restore state must pause"),
        }
    }

    #[tokio::test]
    async fn nothing_declared_means_nothing_asked() {
        assert!(unmet(&[]).await.is_none());
        assert!(matches!(gate(&[]).await, Gate::Clear));
    }

    #[tokio::test]
    async fn the_api_server_is_ready_only_when_readyz_says_so() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, kube) = crate::k8s::testing::api_server().await;
        Mock::given(method("GET"))
            .and(path("/readyz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/readyz"))
            .respond_with(ResponseTemplate::new(500).set_body_string("[-]etcd failed"))
            .mount(&server)
            .await;
        assert!(crate::k8s::ready(&kube).await);
        assert!(!crate::k8s::ready(&kube).await);
        assert!(!crate::k8s::ready(&crate::k8s::testing::unreachable()).await);
    }

    #[tokio::test]
    async fn ceph_is_ready_only_when_it_answers() {
        use crate::host::fake::FakeHost;
        assert!(
            ceph_answers(&FakeHost::new().ok("ceph --connect-timeout 10 health", "HEALTH_OK"))
                .await
        );
        assert!(!ceph_answers(&FakeHost::new().fail("ceph --connect-timeout", "timed out")).await);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Asked(AtomicUsize);

    impl Asked {
        fn new() -> Self {
            Asked(AtomicUsize::new(0))
        }
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
        async fn answer(&self, a: Answer) -> Answer {
            self.0.fetch_add(1, Ordering::SeqCst);
            a
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_is_reused_for_five_seconds_then_asked_again() {
        let cache = Cache::default();
        let asked = Asked::new();
        let ask = |_: Requirement| asked.answer(Some(true));
        assert!(first_unmet(&cache, &[Requirement::Ceph], ask)
            .await
            .is_none());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(first_unmet(&cache, &[Requirement::Ceph], ask)
            .await
            .is_none());
        assert_eq!(asked.count(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(first_unmet(&cache, &[Requirement::Ceph], ask)
            .await
            .is_none());
        assert_eq!(asked.count(), 2);
    }

    #[tokio::test]
    async fn requirements_are_cached_apart_from_each_other() {
        let cache = Cache::default();
        let asked = Asked::new();
        let ask = |r: Requirement| asked.answer(Some(r == Requirement::KubeApi));
        assert_eq!(
            first_unmet(&cache, &[Requirement::KubeApi, Requirement::Ceph], ask).await,
            Some(Requirement::Ceph)
        );
        assert_eq!(asked.count(), 2);
    }

    #[tokio::test]
    async fn the_first_unmet_requirement_stops_the_asking() {
        let cache = Cache::default();
        let asked = Asked::new();
        let ask = |_: Requirement| asked.answer(Some(false));
        assert_eq!(
            first_unmet(&cache, &[Requirement::KubeApi, Requirement::Ceph], ask).await,
            Some(Requirement::KubeApi)
        );
        assert_eq!(asked.count(), 1);
    }

    #[tokio::test]
    async fn a_requirement_nobody_could_answer_is_unmet() {
        let cache = Cache::default();
        let asked = Asked::new();
        assert_eq!(
            first_unmet(&cache, &[Requirement::Ceph], |_| asked.answer(None)).await,
            Some(Requirement::Ceph)
        );
    }

    #[tokio::test]
    async fn a_running_or_unknowable_restore_pauses_and_a_finished_one_clears() {
        let asked = Asked::new();
        let running = gate_in(&Cache::default(), &[Activity::Restore], |_| {
            asked.answer(Some(true))
        })
        .await;
        assert!(matches!(running, Gate::Paused(why) if why.contains("is running")));
        let unknown = gate_in(&Cache::default(), &[Activity::Restore], |_| {
            asked.answer(None)
        })
        .await;
        assert!(matches!(unknown, Gate::Paused(why) if why.contains("cannot tell")));
        let done = gate_in(&Cache::default(), &[Activity::Restore], |_| {
            asked.answer(Some(false))
        })
        .await;
        assert!(matches!(done, Gate::Clear));
    }

    #[tokio::test(start_paused = true)]
    async fn a_restore_that_just_started_is_seen_once_the_cached_answer_expires() {
        let cache = Cache::default();
        let asked = Asked::new();
        let before = gate_in(&cache, &[Activity::Restore], |_| asked.answer(Some(false))).await;
        assert!(matches!(before, Gate::Clear));
        let cached = gate_in(&cache, &[Activity::Restore], |_| asked.answer(Some(true))).await;
        assert!(matches!(cached, Gate::Clear));
        tokio::time::advance(CACHE_FOR).await;
        let after = gate_in(&cache, &[Activity::Restore], |_| asked.answer(Some(true))).await;
        assert!(matches!(after, Gate::Paused(_)));
    }
}
