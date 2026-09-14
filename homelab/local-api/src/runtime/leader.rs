//! One leader for the cluster, owned by the runtime.
//!
//! The Lease used to live inside the disk reconciler, renewed as a side effect of
//! its 30-second tick, and every other single-writer loop asked "am I the disk
//! reconciler's leader?" with an API round-trip per question. So leadership
//! stalled whenever the disk tick did (a slow `ceph-volume`), and two loops that
//! were supposed to be single-writer — the backup scheduler and the restore
//! watchdog — never asked at all.
//!
//! Now a dedicated task renews the Lease every `RENEW_EVERY`, and
//! `Leadership::is_leader()` is a local read: true only while the last successful
//! renewal is younger than `ACT_WITHIN`. `ACT_WITHIN` is deliberately shorter than
//! the Lease duration, so this node stops acting BEFORE another node is allowed to
//! take over — the gap is what keeps two leaders from overlapping (the same rule
//! client-go's leader election uses: renew deadline < lease duration).
//!
//! The Lease NAME and namespace are unchanged from the disk reconciler's, on
//! purpose: during a rolling update an old node and a new node must still be
//! electing the same thing.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::exec::CmdError;

pub const LEASE_NAME: &str = "yolab-disk-reconciler";
pub const LEASE_NS: &str = "rook-ceph";
const LEASE_SECS: i64 = 30;
const RENEW_EVERY: Duration = Duration::from_secs(10);
/// Stop acting this long after the last confirmed renewal. Must stay below
/// `LEASE_SECS`, so this node has stopped before anyone else may start.
const ACT_WITHIN_MS: i64 = 20_000;

static IS_ME: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct Leadership {
    last_renewed_ms: Arc<AtomicI64>,
    fixed: Option<bool>,
}

impl Leadership {
    pub fn is_leader(&self) -> bool {
        if let Some(f) = self.fixed {
            return f;
        }
        let last = self.last_renewed_ms.load(Ordering::SeqCst);
        last > 0 && now_ms() - last < ACT_WITHIN_MS
    }

    #[cfg(test)]
    pub fn fixed_for_tests(leader: bool) -> Self {
        Self {
            last_renewed_ms: Arc::new(AtomicI64::new(0)),
            fixed: Some(leader),
        }
    }
}

/// For status output only — never for a decision (use `Leadership::is_leader`).
pub fn current_holder_is_me() -> bool {
    IS_ME.load(Ordering::SeqCst)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Starts the election loop for `identity` (this node's hostname) and returns
/// the handle controllers consult.
pub fn start(identity: String) -> Leadership {
    let last = Arc::new(AtomicI64::new(0));
    let handle = Leadership {
        last_renewed_ms: last.clone(),
        fixed: None,
    };
    tokio::spawn(async move {
        loop {
            let attempt_started = now_ms();
            match try_acquire(&identity, Utc::now()).await {
                Ok(true) => {
                    if !IS_ME.swap(true, Ordering::SeqCst) {
                        tracing::info!("leader: {identity} now holds the cluster lease");
                    }
                    // The renewal counts from when it was ATTEMPTED: the time
                    // spent waiting on kubectl already ate into the lease.
                    last.store(attempt_started, Ordering::SeqCst);
                }
                Ok(false) => {
                    if IS_ME.swap(false, Ordering::SeqCst) {
                        tracing::warn!("leader: {identity} lost the cluster lease");
                    }
                    last.store(0, Ordering::SeqCst);
                }
                Err(e) => {
                    // Not answered: keep the last confirmed time, so is_leader()
                    // expires on its own after ACT_WITHIN if this persists.
                    tracing::debug!("leader: could not renew the lease: {e}");
                }
            }
            tokio::time::sleep(RENEW_EVERY).await;
        }
    });
    handle
}

/// Whether `identity` holds a live lease right now, read straight from the API —
/// for a one-off process (`local-api run`) that takes part in no election. `Err`
/// when the API did not answer: not knowing is not "yes".
pub async fn held_by(identity: &str) -> Result<bool, CmdError> {
    let lease =
        crate::kubectl::get_opt(&["get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json"])
            .await?;
    Ok(lease.is_some_and(|l| holds_live(&l, identity, Utc::now())))
}

fn holds_live(lease: &Value, identity: &str, now: DateTime<Utc>) -> bool {
    let spec = &lease["spec"];
    if spec["holderIdentity"].as_str() != Some(identity) {
        return false;
    }
    let dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_SECS);
    // Unlike `decide`, an unreadable renewTime is NOT live here: this answer
    // grants permission to act, so it must be proven.
    spec["renewTime"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|ts| (now - ts.with_timezone(&Utc)).num_seconds() <= dur)
}

/// What to do with the Lease as it stands. Pure, so the takeover rules are
/// tested without a cluster.
#[derive(Debug, PartialEq)]
pub(crate) enum LeaseDecision {
    /// Another holder's lease is still live.
    Yield,
    /// Ours, or expired: write it with this acquire time.
    Take { acquire_time: DateTime<Utc> },
}

pub(crate) fn decide(lease: &Value, identity: &str, now: DateTime<Utc>) -> LeaseDecision {
    let spec = &lease["spec"];
    let holder = spec["holderIdentity"].as_str().unwrap_or("");
    let dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_SECS);
    // An unreadable renewTime reads as EXPIRED: a lease nobody can prove is live
    // must not block the cluster from ever having a leader again.
    let expired = spec["renewTime"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|ts| (now - ts.with_timezone(&Utc)).num_seconds() > dur)
        .unwrap_or(true);
    if holder != identity && !holder.is_empty() && !expired {
        return LeaseDecision::Yield;
    }
    let acquire_time = if holder == identity {
        spec["acquireTime"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(now)
    } else {
        now
    };
    LeaseDecision::Take { acquire_time }
}

fn manifest(
    identity: &str,
    now: DateTime<Utc>,
    acquire_time: DateTime<Utc>,
    resource_version: Option<&str>,
) -> Value {
    let fmt = SecondsFormat::Micros;
    let mut metadata = json!({ "name": LEASE_NAME, "namespace": LEASE_NS });
    if let Some(rv) = resource_version {
        metadata["resourceVersion"] = json!(rv);
    }
    json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": metadata,
        "spec": {
            "holderIdentity": identity,
            "leaseDurationSeconds": LEASE_SECS,
            "acquireTime": acquire_time.to_rfc3339_opts(fmt, true),
            "renewTime": now.to_rfc3339_opts(fmt, true),
        },
    })
}

/// `Ok(true)` when this node holds the lease after the call, `Ok(false)` when
/// someone else does, `Err` when the API did not answer.
async fn try_acquire(identity: &str, now: DateTime<Utc>) -> Result<bool, CmdError> {
    let current =
        crate::kubectl::get_opt(&["get", "lease", LEASE_NAME, "-n", LEASE_NS, "-o", "json"])
            .await?;
    let Some(lease) = current else {
        // No lease yet. `create` is atomic: exactly one node wins.
        return match crate::kubectl::create(&manifest(identity, now, now, None).to_string()).await {
            Ok(()) => Ok(true),
            Err(e) if e.is_already_exists() => Ok(false),
            Err(e) => Err(e),
        };
    };
    match decide(&lease, identity, now) {
        LeaseDecision::Yield => Ok(false),
        LeaseDecision::Take { acquire_time } => {
            let rv = lease["metadata"]["resourceVersion"].as_str();
            if rv.is_none() {
                return Err(CmdError::parse(
                    "kubectl get lease",
                    "lease has no resourceVersion",
                ));
            }
            // Compare-and-swap on resourceVersion: if another node renewed or
            // took it since we read it, this is a Conflict and we are not leader.
            match crate::kubectl::replace(&manifest(identity, now, acquire_time, rv).to_string())
                .await
            {
                Ok(()) => Ok(true),
                Err(e) if e.is_conflict() => Ok(false),
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(holder: &str, renewed_secs_ago: i64, now: DateTime<Utc>) -> Value {
        json!({
            "metadata": {"resourceVersion": "42"},
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": 30,
                "acquireTime": (now - chrono::Duration::seconds(3600)).to_rfc3339(),
                "renewTime": (now - chrono::Duration::seconds(renewed_secs_ago)).to_rfc3339(),
            }
        })
    }

    #[test]
    fn a_manual_run_needs_a_live_lease_of_its_own() {
        let now = Utc::now();
        assert!(holds_live(&lease("n1", 5, now), "n1", now));
        assert!(!holds_live(&lease("n1", 31, now), "n1", now), "expired");
        assert!(!holds_live(&lease("n2", 5, now), "n1", now), "someone else's");
        let unreadable = json!({"spec": {"holderIdentity": "n1", "renewTime": "garbage"}});
        assert!(!holds_live(&unreadable, "n1", now));
    }

    #[test]
    fn a_live_lease_held_by_another_node_is_respected() {
        let now = Utc::now();
        assert_eq!(
            decide(&lease("n2", 5, now), "n1", now),
            LeaseDecision::Yield
        );
    }

    #[test]
    fn an_expired_lease_is_taken_with_a_fresh_acquire_time() {
        let now = Utc::now();
        assert_eq!(
            decide(&lease("n2", 31, now), "n1", now),
            LeaseDecision::Take { acquire_time: now }
        );
    }

    #[test]
    fn renewing_our_own_lease_keeps_the_original_acquire_time() {
        let now = Utc::now();
        match decide(&lease("n1", 5, now), "n1", now) {
            LeaseDecision::Take { acquire_time } => assert!(acquire_time < now),
            other => panic!("expected Take, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_renew_time_does_not_lock_the_cluster_out_forever() {
        let now = Utc::now();
        let l = json!({"spec": {"holderIdentity": "n2", "renewTime": "garbage"}});
        assert!(matches!(decide(&l, "n1", now), LeaseDecision::Take { .. }));
    }

    #[test]
    fn this_node_stops_acting_before_another_may_take_over() {
        const {
            assert!(ACT_WITHIN_MS < LEASE_SECS * 1000);
            assert!((RENEW_EVERY.as_millis() as i64) < ACT_WITHIN_MS);
        }
    }

    #[test]
    fn leadership_expires_without_renewal() {
        let l = Leadership {
            last_renewed_ms: Arc::new(AtomicI64::new(now_ms() - ACT_WITHIN_MS - 1)),
            fixed: None,
        };
        assert!(!l.is_leader());
        l.last_renewed_ms.store(now_ms(), Ordering::SeqCst);
        assert!(l.is_leader());
        l.last_renewed_ms.store(0, Ordering::SeqCst);
        assert!(!l.is_leader());
    }
}
