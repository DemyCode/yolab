use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use serde_json::json;

use crate::host::Host;

pub struct DashboardPolicy {
    pub port: u16,
    pub url_prefix: String,
    pub password_file: String,
    pub mon_addr: String,
}

struct ServedAt {
    scheme: String,
    port: String,
    prefix: String,
}

fn parse_served(served: &str) -> Option<ServedAt> {
    let (scheme, rest) = served.split_once("://")?;
    let hostport = rest.split('/').next().unwrap_or("");
    let port = hostport.rsplit(':').next().unwrap_or("").to_string();
    let prefix = match rest.split_once('/') {
        Some((_, p)) => format!("/{}", p.trim_end_matches('/')),
        None => String::new(),
    };
    Some(ServedAt {
        scheme: scheme.to_string(),
        port,
        prefix,
    })
}

fn restart_needed(served: &str, want_port: u16, want_prefix: &str) -> bool {
    match parse_served(served) {
        Some(s) => s.scheme != "http" || s.port != want_port.to_string() || s.prefix != want_prefix,
        None => false,
    }
}

#[derive(Debug, PartialEq)]
enum LoginCheck {
    Verified,
    ReapplyNeeded,
    Unreachable,
    NotAPasswordProblem(u16),
}

fn interpret_login_code(code: u16) -> LoginCheck {
    match code {
        200 | 201 => LoginCheck::Verified,
        400 | 401 => LoginCheck::ReapplyNeeded,
        0 => LoginCheck::Unreachable,
        other => LoginCheck::NotAPasswordProblem(other),
    }
}

fn adopt_local_password(existing_file_contents: Option<&str>) -> Option<String> {
    existing_file_contents
        .map(|s| s.chars().filter(|c| !c.is_whitespace()).collect::<String>())
        .filter(|s| !s.is_empty())
}

fn generate_password() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(20)
        .map(char::from)
        .collect()
}

fn trimmed(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

async fn dashboard_url<H: Host>(host: &H) -> String {
    host.ceph_json(&["mgr", "services"])
        .await
        .ok()
        .and_then(|v| v["dashboard"].as_str().map(str::to_string))
        .unwrap_or_default()
}

async fn verify_login(dash_url: &str, password: &str) -> u16 {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    else {
        return 0;
    };
    let url = format!("{}/api/auth", dash_url.trim_end_matches('/'));
    let body = json!({"username": "admin", "password": password}).to_string();
    match client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/vnd.ceph.api.v1.0+json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp.status().as_u16(),
        Err(_) => 0,
    }
}

const PW_KEY: &str = "yolab/dashboard/admin-password";

pub async fn run<H: Host>(host: &H, node: &str, policy: &DashboardPolicy) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("dashboard: ceph not reachable yet — will configure on a later run");
        return Ok(());
    }

    let enabled = host
        .ceph_json(&["mgr", "module", "ls"])
        .await
        .ok()
        .and_then(|v| {
            v["enabled_modules"]
                .as_array()
                .map(|a| a.iter().any(|m| m == "dashboard"))
        })
        .unwrap_or(false);
    if !enabled {
        tracing::info!("dashboard: enabling the dashboard module");
        if host
            .ceph(&["mgr", "module", "enable", "dashboard"])
            .await
            .is_err()
        {
            tracing::warn!("dashboard: could not enable the dashboard module — will retry");
            return Ok(());
        }
    }

    let port_s = policy.port.to_string();
    let addr_key = format!("mgr/dashboard/{node}/server_addr");
    for (key, value) in [
        ("mgr/dashboard/ssl", "false"),
        ("mgr/dashboard/url_prefix", policy.url_prefix.as_str()),
        ("mgr/dashboard/server_port", port_s.as_str()),
        ("mgr/dashboard/ssl_server_port", port_s.as_str()),
        (addr_key.as_str(), policy.mon_addr.as_str()),
    ] {
        host.ceph(&["config", "set", "mgr", key, value])
            .await
            .with_context(|| format!("dashboard: config set mgr {key}"))?;
    }

    let active = host
        .ceph_json(&["mgr", "stat"])
        .await
        .ok()
        .and_then(|v| v["active_name"].as_str().map(str::to_string))
        .unwrap_or_default();
    let mut served = dashboard_url(host).await;

    if !served.is_empty()
        && active == node
        && restart_needed(&served, policy.port, &policy.url_prefix)
    {
        tracing::warn!(
            "dashboard: the mgr serves {served} but should serve http://<addr>:{}{} — restarting the dashboard module to apply it",
            policy.port,
            policy.url_prefix
        );
        if host
            .ceph(&["mgr", "module", "disable", "dashboard"])
            .await
            .is_ok()
            && host
                .ceph(&["mgr", "module", "enable", "dashboard"])
                .await
                .is_ok()
        {
            for _ in 0..30 {
                served = dashboard_url(host).await;
                if served.trim_end_matches('/').ends_with(&policy.url_prefix) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            tracing::info!(
                "dashboard: now served at {}",
                if served.is_empty() {
                    "<not back yet>"
                } else {
                    &served
                }
            );
        } else {
            tracing::warn!(
                "dashboard: could not restart the dashboard module — the prefix stays unapplied"
            );
        }
    }

    if let Some(parent) = Path::new(&policy.password_file).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut pw = host
        .ceph(&["config-key", "get", PW_KEY])
        .await
        .ok()
        .map(|s| trimmed(&s))
        .filter(|s| !s.is_empty());

    if pw.is_none() {
        let local = std::fs::read_to_string(&policy.password_file).ok();
        let candidate = match adopt_local_password(local.as_deref()) {
            Some(p) => {
                tracing::info!("dashboard: promoting this node's password to the cluster-wide one");
                p
            }
            None => {
                tracing::info!("dashboard: generating the cluster-wide dashboard password");
                generate_password()
            }
        };
        if host
            .ceph(&["config-key", "set", PW_KEY, &candidate])
            .await
            .is_err()
        {
            tracing::warn!(
                "dashboard: could not store the dashboard password in the cluster — will retry"
            );
            return Ok(());
        }
        pw = host
            .ceph(&["config-key", "get", PW_KEY])
            .await
            .ok()
            .map(|s| trimmed(&s))
            .filter(|s| !s.is_empty());
    }

    let Some(pw) = pw else {
        tracing::warn!("dashboard: no dashboard password available yet — will retry");
        return Ok(());
    };

    std::fs::write(&policy.password_file, &pw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &policy.password_file,
            std::fs::Permissions::from_mode(0o600),
        )?;
    }

    let user_exists = host
        .ceph(&["dashboard", "ac-user-show", "admin"])
        .await
        .is_ok();
    if !user_exists {
        match host
            .ceph(&[
                "dashboard",
                "ac-user-create",
                "admin",
                "-i",
                &policy.password_file,
                "administrator",
                "--force-password",
            ])
            .await
        {
            Ok(_) => tracing::info!("dashboard: dashboard user admin created"),
            Err(e) => {
                tracing::warn!("dashboard: could not create the dashboard user: {e}");
                return Ok(());
            }
        }
    }

    let dash_url = dashboard_url(host).await;
    if dash_url.is_empty() {
        tracing::info!(
            "dashboard: no active mgr is serving the dashboard yet — cannot verify the login"
        );
        return Ok(());
    }

    let code = verify_login(&dash_url, &pw).await;
    match interpret_login_code(code) {
        LoginCheck::Verified => tracing::info!("dashboard: login verified for user admin"),
        LoginCheck::ReapplyNeeded => {
            tracing::warn!("dashboard: the stored password does not log in (HTTP {code}) — re-applying it");
            match host
                .ceph(&[
                    "dashboard",
                    "ac-user-set-password",
                    "admin",
                    "-i",
                    &policy.password_file,
                    "--force-password",
                ])
                .await
            {
                Ok(_) => tracing::info!("dashboard: password re-applied; it will be verified again on the next run"),
                Err(e) => tracing::warn!("dashboard: could not re-apply the password: {e}"),
            }
        }
        LoginCheck::Unreachable => tracing::warn!("dashboard: could not reach {dash_url} to verify the login"),
        LoginCheck::NotAPasswordProblem(c) => tracing::warn!(
            "dashboard: unexpected response {c} from {dash_url} while verifying the login — not a password problem"
        ),
    }

    tracing::info!(
        "dashboard: configured on {}:{}{}",
        policy.mon_addr,
        policy.port,
        policy.url_prefix
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn policy() -> DashboardPolicy {
        DashboardPolicy {
            port: 7000,
            url_prefix: "/ceph-dashboard".into(),
            password_file: "/var/lib/ceph/dashboard-password".into(),
            mon_addr: "fd00:cafe::1".into(),
        }
    }

    #[test]
    fn parse_served_splits_an_ipv6_dashboard_url() {
        let s = parse_served("http://[fd00:cafe::1]:7000/ceph-dashboard/").unwrap();
        assert_eq!(s.scheme, "http");
        assert_eq!(s.port, "7000");
        assert_eq!(s.prefix, "/ceph-dashboard");
    }

    #[test]
    fn parse_served_handles_no_prefix_at_all() {
        let s = parse_served("http://[fd00:cafe::1]:7000").unwrap();
        assert_eq!(s.prefix, "");
    }

    #[test]
    fn restart_needed_is_false_once_everything_matches() {
        assert!(!restart_needed(
            "http://[fd00:cafe::1]:7000/ceph-dashboard",
            7000,
            "/ceph-dashboard"
        ));
    }

    #[test]
    fn restart_needed_catches_a_wrong_prefix() {
        assert!(restart_needed(
            "http://[fd00:cafe::1]:7000/",
            7000,
            "/ceph-dashboard"
        ));
    }

    #[test]
    fn restart_needed_catches_a_wrong_port() {
        assert!(restart_needed(
            "http://[fd00:cafe::1]:8443/ceph-dashboard",
            7000,
            "/ceph-dashboard"
        ));
    }

    #[test]
    fn restart_needed_catches_tls_still_on() {
        assert!(restart_needed(
            "https://[fd00:cafe::1]:7000/ceph-dashboard",
            7000,
            "/ceph-dashboard"
        ));
    }

    #[test]
    fn restart_needed_fails_closed_on_unparseable_input() {
        assert!(!restart_needed("nonsense", 7000, "/ceph-dashboard"));
    }

    #[test]
    fn login_codes_are_classified() {
        assert_eq!(interpret_login_code(200), LoginCheck::Verified);
        assert_eq!(interpret_login_code(201), LoginCheck::Verified);
        assert_eq!(interpret_login_code(400), LoginCheck::ReapplyNeeded);
        assert_eq!(interpret_login_code(401), LoginCheck::ReapplyNeeded);
        assert_eq!(interpret_login_code(0), LoginCheck::Unreachable);
        assert_eq!(
            interpret_login_code(415),
            LoginCheck::NotAPasswordProblem(415)
        );
        assert_eq!(
            interpret_login_code(404),
            LoginCheck::NotAPasswordProblem(404)
        );
    }

    #[test]
    fn adopts_an_existing_local_password_trimmed() {
        assert_eq!(
            adopt_local_password(Some("hunter2\n")).as_deref(),
            Some("hunter2")
        );
    }

    #[test]
    fn does_not_adopt_a_missing_or_empty_file() {
        assert_eq!(adopt_local_password(None), None);
        assert_eq!(adopt_local_password(Some("")), None);
        assert_eq!(adopt_local_password(Some("   \n")), None);
    }

    #[tokio::test]
    async fn does_nothing_while_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        run(&host, "yolab-n1", &policy()).await.unwrap();
        assert!(!host.ran("mgr module enable"));
    }

    #[tokio::test]
    async fn enables_the_module_only_when_missing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "ceph mgr module ls",
                r#"{"enabled_modules":["dashboard","iostat"]}"#,
            )
            .ok("ceph config set", "")
            .ok("ceph mgr stat", r#"{"active_name":"yolab-n2"}"#)
            .ok("ceph mgr services", r#"{"dashboard":""}"#)
            .ok(
                "ceph config-key get yolab/dashboard/admin-password",
                "clusterpw123",
            )
            .ok("ceph dashboard ac-user-show admin", "")
            .fail("ceph mgr services", "unreachable");

        let dir = tempfile::tempdir().unwrap();
        let mut p = policy();
        p.password_file = dir
            .path()
            .join("dashboard-password")
            .to_string_lossy()
            .into_owned();

        run(&host, "yolab-n1", &p).await.unwrap();

        assert!(!host.ran("mgr module enable dashboard"));
    }

    #[tokio::test]
    async fn generates_a_password_when_none_exists_anywhere() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph mgr module ls", r#"{"enabled_modules":["dashboard"]}"#)
            .ok("ceph config set", "")
            .ok("ceph mgr stat", r#"{"active_name":"yolab-n2"}"#)
            .ok("ceph mgr services", r#"{"dashboard":""}"#)
            .fail(
                "ceph config-key get yolab/dashboard/admin-password",
                "not found",
            )
            .ok(
                "ceph config-key get yolab/dashboard/admin-password",
                "generated-value-abc",
            )
            .ok("ceph config-key set yolab/dashboard/admin-password", "")
            .fail("ceph dashboard ac-user-show admin", "no such user")
            .ok("ceph dashboard ac-user-create", "");

        let dir = tempfile::tempdir().unwrap();
        let mut p = policy();
        p.password_file = dir
            .path()
            .join("dashboard-password")
            .to_string_lossy()
            .into_owned();

        run(&host, "yolab-n1", &p).await.unwrap();

        assert!(host.ran("dashboard ac-user-create"));
        assert_eq!(
            std::fs::read_to_string(&p.password_file).unwrap(),
            "generated-value-abc"
        );
    }
}
