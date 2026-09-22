//! The API surface as data, and the cross-cutting tests that walk all of it.
//!
//! `ROUTE_TABLE` is every path `router::build_router` registers. It is written
//! down rather than read back because axum's `Router` cannot be enumerated at
//! runtime — so the `route-table-is-complete` nix check diffs this list against
//! the `.route("…")` calls in `router.rs`, in both directions. A route missing
//! from the table would be a route no sweep below ever visits; a table entry
//! naming no real route would be a test passing over nothing.
//!
//! WHY A SWEEP AND NOT PER-ROUTE TESTS. Auth is a property of the whole surface,
//! not of any one handler: the failure mode is a route added below the
//! `.layer(auth_middleware)` line, or registered on a second Router that never
//! gets the layer. No handler-level test can see that, and a reviewer reading a
//! diff that adds one `.route(…)` line will not see it either. Walking the table
//! means a new route is covered the moment it is added — and if someone adds one
//! without touching the table, the nix check fails instead.

/// Path, and the HTTP methods registered on it.
pub(crate) const ROUTE_TABLE: &[(&str, &[&str])] = &[
    ("/api/login", &["POST"]),
    ("/api/logout", &["POST"]),
    ("/api/auth/check", &["GET"]),
    ("/api/status", &["GET"]),
    ("/api/system/controllers", &["GET"]),
    ("/api/console/link", &["GET"]),
    ("/api/update", &["POST"]),
    ("/api/update/all", &["POST"]),
    ("/api/system/reboot", &["POST"]),
    ("/api/system/reboot/all", &["POST"]),
    ("/api/update/trigger", &["POST"]),
    ("/api/update/channel", &["GET", "PUT"]),
    ("/api/rebuild-log", &["GET"]),
    ("/api/backups/recovery-key", &["GET"]),
    ("/api/backups/s3", &["GET"]),
    ("/api/backups/s3/enable", &["POST"]),
    ("/api/backups/state", &["GET"]),
    ("/api/backups/snapshots", &["GET"]),
    ("/api/backups/runs", &["GET"]),
    ("/api/backups/restore", &["POST"]),
    ("/api/backups/restores", &["GET"]),
    ("/api/backups/apps", &["GET"]),
    ("/api/backups/cluster/run-now", &["POST"]),
    ("/api/backups/apps/:namespace/run-now", &["POST"]),
    ("/api/backups/apps/:namespace/definition", &["GET"]),
    ("/api/logs", &["GET"]),
    ("/api/disks", &["GET"]),
    ("/api/disks/:node/:id", &["PUT"]),
    ("/api/heal", &["GET", "POST"]),
    ("/api/heal/peer", &["GET"]),
    ("/api/heal/peer/prepare", &["POST"]),
    ("/api/heal/peer/arm", &["POST"]),
    ("/api/heal/peer/undo", &["POST"]),
    ("/api/notifications", &["GET"]),
    ("/api/notifications/test", &["POST"]),
    ("/api/notifications/deliver", &["POST"]),
    ("/api/storage/policy", &["GET", "PUT"]),
    ("/api/ceph/detail", &["GET"]),
    ("/api/ceph/dashboard", &["GET"]),
    ("/ceph-dashboard", &["ANY"]),
    ("/ceph-dashboard/", &["ANY"]),
    ("/ceph-dashboard/*rest", &["ANY"]),
    ("/api/cluster/health", &["GET"]),
    ("/api/ceph/osd/:id/mark-in", &["POST"]),
    ("/api/ceph/osd/:id/mark-out", &["POST"]),
    ("/api/nodes", &["GET"]),
    ("/api/nodes/links", &["GET"]),
    ("/api/cluster/join-info", &["GET"]),
    ("/api/cluster/mesh-candidates", &["GET"]),
    ("/api/mesh/paths", &["GET"]),
    ("/api/cluster/ceph-join", &["GET"]),
    ("/api/apps/repos", &["GET", "POST"]),
    ("/api/apps/repos/:name", &["DELETE"]),
    ("/api/apps/repos/sync", &["POST"]),
    ("/api/apps/custom", &["GET", "POST"]),
    ("/api/apps/custom/:id", &["DELETE"]),
    ("/api/apps/custom/chart", &["POST"]),
    ("/api/tunnel/domain", &["GET"]),
    ("/api/apps/catalog", &["GET"]),
    ("/api/apps/catalog/:id/refresh", &["POST"]),
    ("/api/apps", &["GET"]),
    ("/api/apps/:id", &["POST", "DELETE"]),
    ("/api/apps/:id/update", &["POST"]),
    ("/api/apps/:id/definition", &["GET"]),
    ("/api/apps/:id/backup", &["PUT"]),
    ("/api/apps/:id/scan-outputs", &["POST"]),
    ("/api/apps/:id/pods", &["GET"]),
    ("/api/apps/:id/logs/:pod_name", &["GET"]),
    ("/api/terminal/exec", &["POST"]),
];

/// The ONLY route reachable without credentials. Everything else in
/// `ROUTE_TABLE` must answer 401 to a stranger — `auth_middleware` short-circuits
/// on this exact path and on nothing else.
pub(crate) const PUBLIC_ROUTES: &[&str] = &["/api/login"];

/// A concrete URI for a route pattern, so the request is well-formed.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|seg| match seg.chars().next() {
            Some(':') => "probe",
            Some('*') => "probe",
            _ => seg,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestApi;
    use axum::http::StatusCode;

    /// An `ANY` route takes every verb; the sweep only needs one of them.
    fn verbs(methods: &[&str]) -> Vec<&'static str> {
        methods
            .iter()
            .map(|m| match *m {
                "ANY" => "GET",
                "GET" => "GET",
                "POST" => "POST",
                "PUT" => "PUT",
                "DELETE" => "DELETE",
                other => panic!("unknown method in ROUTE_TABLE: {other}"),
            })
            .collect()
    }

    #[test]
    fn the_route_table_is_not_empty_and_has_no_duplicates() {
        assert!(ROUTE_TABLE.len() > 50, "the table lost most of its routes");
        let mut paths: Vec<&str> = ROUTE_TABLE.iter().map(|&(p, _)| p).collect();
        paths.sort_unstable();
        let before = paths.len();
        paths.dedup();
        assert_eq!(before, paths.len(), "a path is listed twice");
    }

    #[test]
    fn every_listed_route_is_absolute_and_has_at_least_one_method() {
        for &(path, methods) in ROUTE_TABLE {
            assert!(path.starts_with('/'), "{path} is not absolute");
            assert!(!methods.is_empty(), "{path} registers no method");
        }
    }

    /// THE SWEEP. On a provisioned node — one that has finished setup and has a
    /// password — a caller from off this machine with no session and no cluster
    /// token must be refused by every route but `/api/login`.
    ///
    /// This is the test that makes adding a route safe: a route registered
    /// outside the auth layer answers something other than 401 here, and names
    /// itself in the failure.
    #[tokio::test]
    async fn every_route_refuses_an_unauthenticated_stranger() {
        let api = TestApi::provisioned();
        let mut reached = Vec::new();
        for &(path, methods) in ROUTE_TABLE {
            if PUBLIC_ROUTES.contains(&path) {
                continue;
            }
            for method in verbs(methods) {
                let res = api.send(method, &concrete(path), Some("{}")).await;
                if res.reached_handler() {
                    reached.push(format!("{method} {path} -> {}", res.status));
                }
            }
        }
        assert!(
            reached.is_empty(),
            "these routes answered an unauthenticated stranger instead of 401:\n  {}",
            reached.join("\n  ")
        );
    }

    /// The same sweep for a node that has not been set up yet. No password is
    /// configured, so there is nothing to check a session against — the door has
    /// to be held shut by address alone, and everything off-box is still refused.
    #[tokio::test]
    async fn an_unprovisioned_node_still_refuses_everything_off_box() {
        let api = TestApi::unprovisioned();
        let mut reached = Vec::new();
        for &(path, methods) in ROUTE_TABLE {
            if PUBLIC_ROUTES.contains(&path) {
                continue;
            }
            for method in verbs(methods) {
                let res = api.send(method, &concrete(path), Some("{}")).await;
                if res.reached_handler() {
                    reached.push(format!("{method} {path} -> {}", res.status));
                }
            }
        }
        assert!(
            reached.is_empty(),
            "an unprovisioned node let a stranger reach:\n  {}",
            reached.join("\n  ")
        );
    }

    /// A PROVISIONED NODE DOES NOT TRUST ITS OWN LOOPBACK.
    ///
    /// The loopback exemption exists only for the window before a password is
    /// set. Once one is, Caddy's proxied traffic carries the user's session like
    /// anyone else's, and a loopback exemption would mean any process on the box
    /// — including a container that got a host port — is an administrator.
    #[tokio::test]
    async fn a_provisioned_node_does_not_trust_loopback() {
        let api = TestApi::provisioned().from_loopback();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    }

    /// The other half: before setup, loopback IS the only credential there can
    /// be, because Caddy has to be able to serve the setup page.
    #[tokio::test]
    async fn an_unprovisioned_node_trusts_loopback() {
        let api = TestApi::unprovisioned().from_loopback();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn login_is_reachable_without_credentials() {
        let api = TestApi::provisioned();
        let res = api.post("/api/login", r#"{"password":"wrong"}"#).await;
        // Reached the handler and was told the password is wrong — not turned
        // away by the middleware before it could try.
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
        assert_eq!(res.json()["detail"], "Wrong password", "body: {}", res.body);
    }

    #[tokio::test]
    async fn a_session_from_the_real_password_opens_the_door() {
        let api = TestApi::provisioned().login().await;
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_cluster_token_opens_node_to_node_calls() {
        let api = TestApi::provisioned().as_peer_node();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::OK);
    }

    /// The token is compared in constant time and must match exactly. A prefix
    /// must not be enough — that is what `ct_eq`'s length check is for.
    #[tokio::test]
    async fn a_wrong_cluster_token_is_refused() {
        for wrong in ["", "cluster-to", "cluster-tok-extra", "CLUSTER-TOK"] {
            let api = TestApi::provisioned().with_cluster_token(wrong);
            let res = api.get("/api/auth/check").await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "cluster token {wrong:?} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_path_is_refused_rather_than_described() {
        // 404 would tell a stranger which paths exist. The auth layer wraps the
        // fallback too, so they learn nothing.
        let api = TestApi::provisioned();
        let res = api.get("/api/does-not-exist").await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn concrete_fills_in_every_kind_of_path_parameter() {
        assert_eq!(concrete("/api/apps"), "/api/apps");
        assert_eq!(concrete("/api/apps/:id/pods"), "/api/apps/probe/pods");
        assert_eq!(concrete("/api/disks/:node/:id"), "/api/disks/probe/probe");
        assert_eq!(concrete("/ceph-dashboard/*rest"), "/ceph-dashboard/probe");
    }
}
