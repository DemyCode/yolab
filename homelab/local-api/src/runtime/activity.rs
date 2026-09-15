//! Requirements (is the thing a tick needs answering?) and activities (is the
//! cluster in the middle of something a tick must not interfere with?).
//!
//! Both used to be asked by each loop for itself, and both were answered
//! optimistically: `restore::is_running()` read the restore records, and when the
//! read failed it saw no records and said "no restore" — so a brief API outage
//! during a restore let the disk reconciler purge OSDs underneath it.
//! The storage recovery check did the same with `.is_ok_and(..)`.
//!
//! Here "cannot tell" is its own answer, and it pauses.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::Requirement;

/// A cluster-wide operation during which some controllers must stand still.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Activity {
    /// An app restore is replacing PVCs and scaling deployments.
    Restore,
    /// FORCE HEAL is removing machines and disks and deleting every pool.
    Heal,
}

impl Activity {
    fn describe(self) -> &'static str {
        match self {
            Activity::Restore => "an app restore is running",
            Activity::Heal => "the cluster is being healed",
        }
    }

    fn unknown(self) -> &'static str {
        match self {
            Activity::Restore => "cannot tell whether an app restore is running",
            Activity::Heal => "cannot tell whether the cluster is being healed",
        }
    }
}

pub enum Gate {
    Clear,
    Paused(String),
}

/// Answers are cached this long, so twenty controllers checking the same thing
/// in the same second cost one kubectl call, not twenty.
const CACHE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Req(Requirement),
    Act(Activity),
}

/// `Some(true)`/`Some(false)` for a known answer, `None` for "could not tell".
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

/// The first requirement that is not answering, if any.
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

/// Whether any of `activities` is running — or cannot be ruled out.
pub async fn gate(activities: &[Activity]) -> Gate {
    for a in activities {
        let answer = cached(Key::Act(*a), async {
            let r = match a {
                Activity::Restore => crate::routers::restore::running_anywhere().await,
                Activity::Heal => crate::heal::heal_running().await,
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
        match decide(Activity::Heal, None) {
            Gate::Paused(why) => assert!(why.contains("cannot tell")),
            Gate::Clear => panic!("an unknown heal state must pause"),
        }
    }

    #[tokio::test]
    async fn nothing_declared_means_nothing_asked() {
        assert!(unmet(&[]).await.is_none());
        assert!(matches!(gate(&[]).await, Gate::Clear));
    }
}
