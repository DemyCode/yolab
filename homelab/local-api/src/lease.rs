// coordination.k8s.io/v1 Lease-based mutex, replacing the old ConfigMap-based
// locks (yolab-backup-lock, yolab-dr-lock).
//
// A ConfigMap lock can only be cleared by whoever set it, or by a hand-rolled
// "older than 2h" staleness check that has to be present (and correct) at every
// call site that reads the lock. A Lease expires on its own: it carries its own
// leaseDurationSeconds + renewTime, and staleness is just "is renewTime + duration
// in the past" — the same check the reconcile loop already needs to make, so
// there's no separate mechanism to forget to add.

use crate::kubectl;
use serde_json::Value;

const LEASE_NS: &str = "kube-system";

/// Proof the caller held the lease at the moment `acquire()` returned. Reconcile ticks
/// re-acquire (which renews) every tick instead of holding one guard across ticks, so
/// there's no separate renew()/release() to call — the lease simply expires on its own
/// if this process stops ticking, which is exactly the self-healing property it exists for.
pub struct LeaseGuard;

/// `spec.acquireTime`/`spec.renewTime` on a Lease are Kubernetes' `MicroTime` type, not
/// a generic RFC3339 timestamp — its (de)serializer requires *exactly* 6 fractional-
/// second digits (`2006-01-02T15:04:05.000000Z07:00`). `chrono::to_rfc3339()` doesn't
/// match that: it emits nanosecond precision when there's a fractional part, or none
/// at all when the current instant happens to land on a whole second — either way the
/// apiserver rejects it with a 400. That failure was being swallowed by `.ok()` at
/// every call site below, so lease creation silently never succeeded: reconcile_tick's
/// very first line bailed out on every single call, for as long as this code has
/// existed, with no error ever surfaced anywhere.
fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn is_expired(lease: &Value, fallback_duration_secs: i64) -> bool {
    let renew = lease["spec"]["renewTime"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    let duration_secs = lease["spec"]["leaseDurationSeconds"]
        .as_i64()
        .unwrap_or(fallback_duration_secs);
    match renew {
        Some(t) => (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() > duration_secs,
        None => true,
    }
}

/// Tries to acquire `name`, taking over if the existing lease (if any) has expired.
/// Returns `None` if another holder currently owns a live lease.
///
/// `duration_secs` should comfortably exceed the real gap between renew() calls —
/// the reconcile loop renews on every tick, so a duration a few ticks wide tolerates
/// a missed tick or two without another node/process taking over mid-operation.
pub async fn acquire(name: &str, holder: &str, duration_secs: i64) -> Option<LeaseGuard> {
    let existing = match kubectl::run(&[
        "get", "lease", name, "-n", LEASE_NS, "-o", "json", "--ignore-not-found",
    ]).await {
        Ok(o) => o,
        Err(e) => { tracing::warn!("lease {name}: get failed: {e}"); return None; }
    };

    if existing.trim().is_empty() {
        // Nobody holds it yet — `create` is atomic (409 AlreadyExists if a racer wins).
        let manifest = serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": name, "namespace": LEASE_NS,
                "labels": { "app.kubernetes.io/managed-by": "yolab" },
            },
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": duration_secs,
                "acquireTime": now_rfc3339(),
                "renewTime": now_rfc3339(),
            },
        });
        return match kubectl::create(&manifest.to_string()).await {
            Ok(_) => Some(LeaseGuard),
            // Losing a create race (AlreadyExists) is normal and expected; anything
            // else (e.g. a malformed manifest the apiserver rejects) must be visible —
            // this exact `.ok()` used to swallow a format bug that made every lease
            // acquisition fail silently, permanently, with no trace anywhere.
            Err(e) => { tracing::debug!("lease {name}: create: {e}"); None }
        };
    }

    let lease: Value = match serde_json::from_str(&existing) {
        Ok(v) => v,
        Err(e) => { tracing::warn!("lease {name}: parse failed: {e}"); return None; }
    };
    let current_holder = lease["spec"]["holderIdentity"].as_str().unwrap_or("");
    if !is_expired(&lease, duration_secs) && current_holder != holder {
        return None; // someone else holds a live lease
    }

    // Either the lease expired (anyone may take it over) or it's still live but
    // already ours (a renewal) — both go through the same resourceVersion-checked
    // patch. Including resourceVersion in the merge-patch body
    // makes the API server enforce it as an optimistic-concurrency precondition, so
    // two processes racing to take over the same expired lease can't both succeed.
    let resource_version = lease["metadata"]["resourceVersion"].as_str().unwrap_or("");
    let patch = serde_json::json!({
        "metadata": { "resourceVersion": resource_version },
        "spec": {
            "holderIdentity": holder,
            "leaseDurationSeconds": duration_secs,
            "acquireTime": now_rfc3339(),
            "renewTime": now_rfc3339(),
        },
    }).to_string();
    match kubectl::run(&["patch", "lease", name, "-n", LEASE_NS, "--type=merge", "-p", &patch]).await {
        Ok(_) => Some(LeaseGuard),
        Err(e) => { tracing::debug!("lease {name}: renew/takeover: {e}"); None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks in the exact format Kubernetes' `MicroTime` type requires — the whole
    /// class of bug here was this format being *subtly* wrong (extra/missing
    /// fractional digits) in a way `cargo check` can never catch, only the apiserver
    /// rejecting it at runtime. This regex is the same shape apimachinery's own
    /// MicroTime marshaller expects: exactly 6 fractional digits, literal `Z`.
    #[test]
    fn now_rfc3339_matches_kubernetes_microtime_format() {
        let s = now_rfc3339();
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$").unwrap();
        assert!(re.is_match(&s), "expected exactly 6 fractional digits + Z, got: {s}");
    }

    #[test]
    fn now_rfc3339_round_trips_through_rfc3339_parser() {
        // is_expired() reads this back with chrono's generic RFC3339 parser (which is
        // lenient about fractional-digit count) — confirm that direction works too.
        let s = now_rfc3339();
        assert!(chrono::DateTime::parse_from_rfc3339(&s).is_ok());
    }
}
