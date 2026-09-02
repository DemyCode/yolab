//! Enable and configure the Ceph dashboard on this node's mgr.
//!
//! Not a reverse proxy to a fixed address: the dashboard runs on the ACTIVE
//! mgr only, and a standby redirects to that mgr's WireGuard address, which a
//! browser cannot reach. `routers/ceph.rs`'s dashboard proxy asks Ceph which
//! mgr is active and forwards there, so a failover changes the answer and
//! nothing else notices.
//!
//! `ceph config set` stores a value; it does not restart anything. The
//! dashboard reads `ssl`/`server_port`/`url_prefix` exactly once, when it
//! mounts its CherryPy tree at module start — so the first run always sets
//! them on an already-serving module, where they sit stored and unused until
//! something restarts it. The symptom is not a dashboard that looks broken:
//! it is one working perfectly at `/` instead of under the prefix, so every
//! proxied request comes back as Ceph's own CherryPy 404. `restart_needed`
//! below is compared against what the mgr REPORTS it serves, never against
//! `ceph config get` — config holding the right value while the running
//! module ignores it IS the fault being repaired, so it cannot be the thing
//! that decides whether it is fixed.

use std::{path::Path, time::Duration};

use anyhow::Result;
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

/// `"http://[fd00::1]:7000/ceph-dashboard/"` -> scheme/port/prefix. The last
/// colon of the host:port segment begins the port — the address is
/// bracketed, so this cannot bite off part of an IPv6 literal.
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

/// scheme covers `ssl`, port covers `server_port`, prefix covers
/// `url_prefix` — the three keys that only take effect on a module restart.
/// `false` (never restart) when `served` cannot even be parsed: restarting is
/// disruptive to any open session, so an unrecognised shape must fail closed.
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
    /// 415 means the Accept header is wrong for this Ceph version, 404 that
    /// url_prefix and the proxy disagree — neither is a password problem, and
    /// re-applying it would hide the real fault.
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

/// This node's own password file, if it holds something usable — adopted
/// rather than replaced so an upgrade from the per-node era promotes a
/// password that already works instead of inventing a new one. Trims
/// whitespace so a file written by the version that appended a newline still
/// matches what Ceph has stored for it.
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

    // TLS off on purpose: Caddy terminates HTTPS at the edge and this is
    // reached only over the WireGuard mesh.
    let _ = host
        .ceph(&["config", "set", "mgr", "mgr/dashboard/ssl", "false"])
        .await;
    let _ = host
        .ceph(&[
            "config",
            "set",
            "mgr",
            "mgr/dashboard/url_prefix",
            &policy.url_prefix,
        ])
        .await;
    let port_s = policy.port.to_string();
    let _ = host
        .ceph(&["config", "set", "mgr", "mgr/dashboard/server_port", &port_s])
        .await;
    let _ = host
        .ceph(&[
            "config",
            "set",
            "mgr",
            "mgr/dashboard/ssl_server_port",
            &port_s,
        ])
        .await;
    let addr_key = format!("mgr/dashboard/{node}/server_addr");
    let _ = host
        .ceph(&["config", "set", "mgr", &addr_key, &policy.mon_addr])
        .await;

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

    // The dashboard user database is not per-node — it lives in the mon KV
    // store, so there is one `admin` account for the whole cluster. Plaintext
    // in config-key is deliberate: the Storage page displays this by design,
    // so it must stay recoverable, and config-key needs the admin keyring —
    // the same trust boundary as the 0600 file it replaces.
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
        // Re-read rather than trusting what was just written: two nodes
        // racing to fill an empty key both find it missing and both set it,
        // so the loser must end up holding the winner's value.
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

    // printf-equivalent, not a trailing newline: Ceph stores the trimmed
    // value and local-api's Storage page reads this file verbatim, so a
    // stray newline here is a password that looks right and never logs in.
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

    // Everything above can succeed and the login still fail, so ask the
    // dashboard itself whether the password the page displays actually logs
    // in, and re-apply on failure only — re-applying invalidates any open
    // session, which three nodes doing it unconditionally on a timer is
    // exactly what caused the per-node password conflict this replaced.
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

    // ── parse_served / restart_needed ─────────────────────────────────────────

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

    // ── interpret_login_code ──────────────────────────────────────────────────

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

    // ── adopt_local_password ───────────────────────────────────────────────────

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

    // ── run(): sequencing against a FakeHost ──────────────────────────────────

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
            .ok("ceph mgr stat", r#"{"active_name":"yolab-n2"}"#) // not us — skip restart branch
            .ok("ceph mgr services", r#"{"dashboard":""}"#)
            .ok(
                "ceph config-key get yolab/dashboard/admin-password",
                "clusterpw123",
            )
            .ok("ceph dashboard ac-user-show admin", "")
            .fail("ceph mgr services", "unreachable"); // second read, for the login-verify step: empty -> skip

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
        // Two answers on the same prefix, consumed in order: the first read
        // finds nothing (triggers generation), the second — after
        // `config-key set` — sees the value that write is presumed to have
        // taken, exactly as two nodes racing to fill the key would each see
        // their own read reflect whichever write actually won.
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
