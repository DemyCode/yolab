
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

pub(crate) const PUBLIC_ROUTES: &[&str] = &["/api/login"];

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
    use crate::cache;
    use crate::testkit::TestApi;
    use axum::http::StatusCode;

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

    #[tokio::test]
    async fn a_provisioned_node_does_not_trust_loopback() {
        let api = TestApi::provisioned().over_loopback();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unprovisioned_node_trusts_loopback() {
        let api = TestApi::unprovisioned().over_loopback();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn login_is_reachable_without_credentials() {
        let api = TestApi::provisioned();
        let res = api.post("/api/login", r#"{"password":"wrong"}"#).await;
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
        let api = TestApi::provisioned().with_peer_token();
        let res = api.get("/api/auth/check").await;
        assert_eq!(res.status, StatusCode::OK);
    }

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


    #[test]
    fn nothing_that_returns_a_secret_is_cacheable() {
        for path in [
            "/api/account/token",
            "/api/login",
            "/api/logout",
            "/api/auth/check",
            "/api/backups/recovery-key",
            "/api/ceph/dashboard",
            "/api/cluster/ceph-join",
            "/api/notifications",
        ] {
            assert!(
                cache::policy_for(path).is_none(),
                "{path} returns credentials and would be cached"
            );
        }
    }

    #[test]
    fn no_heal_route_is_cacheable() {
        let heal: Vec<&str> = ROUTE_TABLE
            .iter()
            .map(|&(p, _)| p)
            .filter(|p| p.starts_with("/api/heal"))
            .collect();
        assert!(!heal.is_empty(), "the table lost the heal routes");
        for path in heal {
            assert!(
                cache::policy_for(path).is_none(),
                "{path} decides a destructive action and would be cached"
            );
        }
    }

    #[test]
    fn no_log_route_is_cacheable() {
        for &(path, _) in ROUTE_TABLE {
            if path.contains("/logs") || path == "/api/rebuild-log" {
                assert!(
                    cache::policy_for(&concrete(path)).is_none(),
                    "{path} serves logs and would be cached"
                );
            }
        }
    }

    #[test]
    fn nothing_outside_the_api_is_cacheable() {
        for &(path, _) in ROUTE_TABLE {
            if !path.starts_with("/api/") {
                assert!(
                    cache::policy_for(&concrete(path)).is_none(),
                    "{path} is not an API route and would be cached"
                );
            }
        }
    }

    #[test]
    fn a_cached_body_is_shown_for_less_time_than_it_is_kept() {
        let policy = cache::policy_for("/api/status").expect("/api/status is cached");
        assert!(
            policy.ttl < policy.hard,
            "ttl {:?} is not shorter than the hard limit {:?}",
            policy.ttl,
            policy.hard
        );
    }
}
