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
use std::time::{Duration, Instant};

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

    /// A handle for a lease this process has just seen live but does not renew
    /// (`local-api run`): leader for `ACT_WITHIN` from now, then not.
    pub fn confirmed_now() -> Self {
        Self::renewed_at(now_ms())
    }

    fn renewed_at(ms: i64) -> Self {
        Self {
            last_renewed_ms: Arc::new(AtomicI64::new(ms)),
            fixed: None,
        }
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
    if !may_stand(&identity) {
        // An empty holder reads as a RELEASED lease to `decide`, so a node with
        // no name would be taken over by everyone and take over from everyone:
        // no single leader at all. Better never to lead.
        tracing::error!("leader: this node has no hostname — it will never lead the cluster");
        return handle;
    }
    tokio::spawn(async move {
        let mut seen: Option<(String, Instant)> = None;
        loop {
            let attempt_started = now_ms();
            match try_acquire(&identity, Utc::now(), &mut seen).await {
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

fn may_stand(identity: &str) -> bool {
    !identity.trim().is_empty()
}

/// Whether `identity` holds a live lease right now, read straight from the API —
/// for a one-off process (`local-api run`) that takes part in no election. `Err`
/// when the API did not answer: not knowing is not "yes".
pub async fn held_by(identity: &str) -> Result<bool, CmdError> {
    if !may_stand(identity) {
        return Ok(false);
    }
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

/// How long this node has watched the lease record sit at `version` unchanged,
/// on its own monotonic clock. Starts at zero whenever the version moves.
fn unchanged_for(seen: &mut Option<(String, Instant)>, version: &str, now: Instant) -> Duration {
    match seen {
        Some((v, since)) if v == version => now.saturating_duration_since(*since),
        _ => {
            *seen = Some((version.to_string(), now));
            Duration::ZERO
        }
    }
}

/// EXPIRY IS OBSERVED, NOT READ. The holder's `renewTime` is a timestamp from
/// ANOTHER machine's clock; comparing it with ours meant a node whose clock ran
/// 10s ahead saw every live lease as expired and took it — two leaders, two
/// writers of every cluster-scoped controller. So a lease held by someone else
/// expires only once THIS node has watched its record stay unchanged for the
/// lease duration (the rule client-go's leader election uses). The cost: a node
/// that just started waits one lease duration before taking over a dead leader.
pub(crate) fn decide(
    lease: &Value,
    identity: &str,
    now: DateTime<Utc>,
    unchanged: Duration,
) -> LeaseDecision {
    let spec = &lease["spec"];
    let holder = spec["holderIdentity"].as_str().unwrap_or("");
    let dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_SECS).max(0);
    let expired = unchanged > Duration::from_secs(dur.unsigned_abs());
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
async fn try_acquire(
    identity: &str,
    now: DateTime<Utc>,
    seen: &mut Option<(String, Instant)>,
) -> Result<bool, CmdError> {
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
    // The version is both what expiry is observed against and what the takeover
    // swaps on; without it there is neither.
    let Some(version) = lease["metadata"]["resourceVersion"].as_str() else {
        return Err(CmdError::parse(
            "kubectl get lease",
            "lease has no resourceVersion",
        ));
    };
    let unchanged = unchanged_for(seen, version, Instant::now());
    match decide(&lease, identity, now, unchanged) {
        LeaseDecision::Yield => Ok(false),
        LeaseDecision::Take { acquire_time } => {
            let rv = Some(version);
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

    const SECS: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn a_live_lease_held_by_another_node_is_respected() {
        let now = Utc::now();
        let fresh = decide(&lease("n2", 5, now), "n1", now, SECS(5));
        assert_eq!(fresh, LeaseDecision::Yield);
    }

    #[test]
    fn an_expired_lease_is_taken_with_a_fresh_acquire_time() {
        let now = Utc::now();
        let dead = decide(&lease("n2", 31, now), "n1", now, SECS(31));
        assert_eq!(dead, LeaseDecision::Take { acquire_time: now });
    }

    /// The skew bug: this node's clock runs ahead, so the holder's renewTime
    /// LOOKS a minute old. The holder is renewing it right now, though, and
    /// that is all that counts.
    #[test]
    fn a_clock_that_runs_ahead_does_not_steal_a_lease_that_is_being_renewed() {
        let now = Utc::now();
        let looks_stale = lease("n2", 60, now);
        let just_changed = decide(&looks_stale, "n1", now, Duration::ZERO);
        assert_eq!(just_changed, LeaseDecision::Yield);
    }

    #[test]
    fn expiry_is_timed_from_when_this_node_last_saw_the_record_change() {
        let t0 = Instant::now();
        let mut seen = None;
        assert_eq!(unchanged_for(&mut seen, "7", t0), Duration::ZERO);
        assert_eq!(unchanged_for(&mut seen, "7", t0 + SECS(20)), SECS(20));
        // A renewal moves the version and restarts the clock.
        assert_eq!(unchanged_for(&mut seen, "8", t0 + SECS(25)), Duration::ZERO);
        assert_eq!(unchanged_for(&mut seen, "8", t0 + SECS(40)), SECS(15));
    }

    #[test]
    fn renewing_our_own_lease_keeps_the_original_acquire_time() {
        let now = Utc::now();
        match decide(&lease("n1", 5, now), "n1", now, Duration::ZERO) {
            LeaseDecision::Take { acquire_time } => assert!(acquire_time < now),
            other => panic!("expected Take, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_lease_that_never_changes_does_not_lock_the_cluster_out() {
        let now = Utc::now();
        let l = json!({"spec": {"holderIdentity": "n2", "renewTime": "garbage"}});
        let decision = decide(&l, "n1", now, SECS(31));
        assert!(matches!(decision, LeaseDecision::Take { .. }));
    }

    #[test]
    fn this_node_stops_acting_before_another_may_take_over() {
        const {
            assert!(ACT_WITHIN_MS < LEASE_SECS * 1000);
            assert!((RENEW_EVERY.as_millis() as i64) < ACT_WITHIN_MS);
        }
    }

    #[test]
    fn a_node_without_a_name_never_stands_for_leader() {
        assert!(!may_stand(""));
        assert!(!may_stand("  "));
        assert!(may_stand("yolab-n1"));
    }

    #[tokio::test]
    async fn a_node_without_a_name_never_holds_the_lease_for_a_manual_run() {
        // Answered before any API call: a released lease has an empty holder too.
        assert!(!held_by("").await.unwrap());
    }

    #[tokio::test]
    async fn starting_without_a_name_yields_a_handle_that_never_leads() {
        assert!(!start(String::new()).is_leader());
    }

    #[test]
    fn a_released_lease_is_taken_even_if_recently_renewed() {
        let now = Utc::now();
        assert!(matches!(
            decide(&lease("", 1, now), "n1", now, Duration::ZERO),
            LeaseDecision::Take { acquire_time } if acquire_time == now
        ));
    }

    #[test]
    fn the_written_lease_is_a_compare_and_swap_only_when_it_names_a_version() {
        let now = Utc::now();
        let renew = manifest("n1", now, now, Some("42"));
        assert_eq!(renew["metadata"]["resourceVersion"], "42");
        assert_eq!(renew["spec"]["holderIdentity"], "n1");
        assert_eq!(renew["spec"]["leaseDurationSeconds"], LEASE_SECS);
        let create = manifest("n1", now, now, None);
        assert!(create["metadata"].get("resourceVersion").is_none());
    }

    #[test]
    fn a_manual_confirmation_lasts_only_as_long_as_the_daemon_would_trust_it() {
        assert!(Leadership::confirmed_now().is_leader());
        assert!(!Leadership::renewed_at(now_ms() - ACT_WITHIN_MS - 1).is_leader());
        assert!(!Leadership::renewed_at(0).is_leader());
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
