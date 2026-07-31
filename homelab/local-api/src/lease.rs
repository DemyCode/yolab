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

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
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
    let existing = kubectl::run(&[
        "get", "lease", name, "-n", LEASE_NS, "-o", "json", "--ignore-not-found",
    ]).await.ok()?;

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
        return kubectl::create(&manifest.to_string()).await.ok().map(|_| LeaseGuard);
    }

    let lease: Value = serde_json::from_str(&existing).ok()?;
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
    kubectl::run(&["patch", "lease", name, "-n", LEASE_NS, "--type=merge", "-p", &patch])
        .await
        .ok()
        .map(|_| LeaseGuard)
}
