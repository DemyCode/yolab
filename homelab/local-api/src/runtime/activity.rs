use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

fn cache() -> &'static Mutex<HashMap<Key, (Instant, Answer)>> {
    static C: std::sync::OnceLock<Mutex<HashMap<Key, (Instant, Answer)>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn cached(key: Key, ask: impl std::future::Future<Output = Answer>) -> Answer {
    {
        let c = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, a)) = c.get(&key) {
            if at.elapsed() < CACHE_FOR {
                return *a;
            }
        }
    }
    let a = ask.await;
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (Instant::now(), a));
    a
}

async fn kube_api_ready() -> Answer {
    let ok = crate::exec::checked(
        "kubectl",
        &["get", "--raw", "/readyz", "--request-timeout=10s"],
        Duration::from_secs(15),
    )
    .await
    .is_ok();
    Some(ok)
}

async fn ceph_ready() -> Answer {
    Some(
        crate::ceph_cli::ceph(&["--connect-timeout", "10", "health"])
            .await
            .is_ok(),
    )
}

pub async fn is_met(r: Requirement) -> bool {
    cached(Key::Req(r), async {
        match r {
            Requirement::KubeApi => kube_api_ready().await,
            Requirement::Ceph => ceph_ready().await,
        }
    })
    .await
        == Some(true)
}

pub async fn unmet(requires: &[Requirement]) -> Option<Requirement> {
    for r in requires {
        let answer = cached(Key::Req(*r), async {
            match r {
                Requirement::KubeApi => kube_api_ready().await,
                Requirement::Ceph => ceph_ready().await,
            }
        })
        .await;
        if answer != Some(true) {
            return Some(*r);
        }
    }
    None
}

pub async fn gate(activities: &[Activity]) -> Gate {
    for a in activities {
        let answer = cached(Key::Act(*a), async {
            let r = match a {
                Activity::Restore => crate::routers::restore::running_anywhere().await,
            };
            r.map_err(|e| tracing::debug!("activity {a:?}: {e:#}")).ok()
        })
        .await;
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
}
