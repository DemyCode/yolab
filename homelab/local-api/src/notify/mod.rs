//! Phone notifications, through an ntfy server on every machine.
//!
//! WHY ON THE MACHINE, NOT IN KUBERNETES. The notifications that matter most are
//! the ones about Kubernetes or Ceph not working, so the server and the sender
//! must not depend on either. ntfy runs as a NixOS service (see
//! homelab/nixos/common.nix), reached through the machine's own Caddy and
//! WireGuard tunnel under a second name, `ntfy-<machine>.<account domain>`:
//! ntfy only serves from the root of a host, never under a path.
//!
//! EVERY MACHINE IS ITS OWN SERVER. The phone subscribes to each machine, and
//! each machine sends what it sees — a machine that is down cannot take the
//! notifications about it down too. A problem the whole cluster has is sent by
//! every machine that sees it.
//!
//! THE TOPIC IS THE SECRET. The phone app subscribes from a `ntfy://host/topic`
//! link, which carries no credentials. So the server denies every topic but one
//! with a long random name, generated once per machine
//! (`/var/lib/yolab/ntfy/topic`), readable and writable by whoever knows it —
//! the same model as ntfy.sh itself. The page shows it only to the signed-in
//! owner, as a QR code.

pub(crate) mod alerts;
pub(crate) mod dns;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use serde_json::{json, Value};

use crate::AppState;

/// Where ntfy listens: loopback only, Caddy is its one client from outside.
const NTFY_LOCAL: &str = "http://[::1]:2586";
/// The ntfy environment file the NixOS service reads (`environmentFile`).
const ENV_FILE: &str = "var/lib/yolab/ntfy/ntfy.env";
const TOPIC_FILE: &str = "var/lib/yolab/ntfy/topic";

fn topic_path(root: &Path) -> PathBuf {
    root.join(TOPIC_FILE)
}

/// This machine's topic, if notifications were set up on it.
pub(crate) fn topic(root: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(topic_path(root)) {
        Ok(t) => {
            let t = t.trim().to_string();
            if !valid_topic(&t) {
                bail!("{} does not hold a valid topic", topic_path(root).display());
            }
            Ok(Some(t))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", topic_path(root).display())),
    }
}

/// ntfy's own rule for topic names.
fn valid_topic(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Boot step, before ntfy starts: the topic, generated once and kept, and the
/// access rules ntfy reads from its environment file — every topic denied
/// (`auth-default-access`), this one open to whoever knows its name.
pub(crate) fn ensure_credentials(root: &Path) -> Result<String> {
    let topic = match topic(root)? {
        Some(t) => t,
        None => {
            let t = format!("yolab-{}", crate::routers::backup_common::random_hex(16));
            crate::config::write_private_file(&topic_path(root), t.as_bytes())?;
            tracing::info!("notifications: generated this machine's topic");
            t
        }
    };
    let env = format!("NTFY_AUTH_ACCESS='*:{topic}:rw'\n");
    crate::config::write_private_file(&root.join(ENV_FILE), env.as_bytes())?;
    Ok(topic)
}

/// `local-api notify <subcommand>`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("credentials") => match ensure_credentials(Path::new("/")) {
            Ok(_) => 0,
            Err(e) => {
                tracing::error!("notify credentials: {e:#}");
                1
            }
        },
        other => {
            eprintln!("notify: unknown subcommand {other:?} (known: credentials)");
            2
        }
    }
}

// ── This machine's public names ───────────────────────────────────────────────

/// The parts of `[tunnel]` in config.toml notifications need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tunnel {
    pub enabled: bool,
    pub platform_api_url: String,
    pub account_token: String,
    pub tunnel_id: String,
    pub sub_ipv6: String,
    /// The machine's own host name, e.g. `node1.6.yolab.io`.
    pub host: String,
}

impl Tunnel {
    pub fn read(config_path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("read {config_path}"))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let table: toml::Table = toml::from_str(text).context("config.toml is not TOML")?;
        let t = table
            .get("tunnel")
            .and_then(|t| t.as_table())
            .context("config.toml has no [tunnel]")?;
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let dns_url = s("dns_url");
        let host = dns_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            enabled: t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            platform_api_url: s("platform_api_url").trim_end_matches('/').to_string(),
            account_token: s("account_token"),
            tunnel_id: s("tunnel_id"),
            sub_ipv6: s("sub_ipv6"),
            host,
        })
    }

    /// The notification server's host name — the same one Caddy serves, built
    /// the same way in homelab/nixos/common.nix: `ntfy-` before the machine's.
    pub fn ntfy_host(&self) -> Option<String> {
        (!self.host.is_empty()).then(|| format!("ntfy-{}", self.host))
    }

    /// The DNS record name of that host on the platform: its first label.
    pub fn ntfy_record_name(&self) -> Option<String> {
        let host = self.ntfy_host()?;
        host.split('.').next().map(str::to_string)
    }

    /// The machine's name for people: the first label of its own host.
    pub fn machine_label(&self) -> String {
        self.host.split('.').next().unwrap_or(&self.host).to_string()
    }
}

// ── Sending ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Notification {
    pub title: String,
    pub message: String,
    /// ntfy's 1 (min) to 5 (max).
    pub priority: u8,
    pub tags: Vec<String>,
    /// Opened when the notification is tapped.
    pub click: Option<String>,
}

/// Publishes to this machine's own server. JSON rather than headers, so titles
/// and messages are not limited to ASCII.
pub(crate) async fn publish(topic: &str, n: &Notification) -> Result<()> {
    let mut body = json!({
        "topic": topic,
        "title": n.title,
        "message": n.message,
        "priority": n.priority,
        "tags": n.tags,
    });
    if let Some(click) = &n.click {
        body["click"] = Value::String(click.clone());
    }
    let response = reqwest::Client::new()
        .post(NTFY_LOCAL)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("reach this machine's ntfy")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("ntfy refused the notification: {status} {}", text.trim());
    }
    Ok(())
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Serialize)]
struct Subscription {
    machine: String,
    topic: String,
    /// Opens the ntfy app and subscribes: what the QR code holds.
    subscribe_url: String,
    /// The same topic in a browser.
    web_url: String,
}

fn subscription(tunnel: &Tunnel, topic: &str) -> Option<Subscription> {
    let host = tunnel.ntfy_host()?;
    let machine = tunnel.machine_label();
    Some(Subscription {
        subscribe_url: format!(
            "ntfy://{host}/{topic}?display={}",
            format!("YoLab {machine}").replace(' ', "+")
        ),
        web_url: format!("https://{host}/{topic}"),
        topic: topic.to_string(),
        machine,
    })
}

fn not_set_up(why: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({ "available": false, "reason": why.to_string() })),
    )
}

/// `GET /api/notifications` — how to subscribe to this machine's notifications.
pub async fn get_subscription(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let tunnel = match Tunnel::read(&s.config.config_path) {
        Ok(t) => t,
        Err(e) => return not_set_up(format!("{e:#}")),
    };
    if !tunnel.enabled {
        return not_set_up("this machine is not connected to the YoLab platform");
    }
    let topic = match topic(Path::new("/")) {
        Ok(Some(t)) => t,
        Ok(None) => return not_set_up("notifications are not set up on this machine yet"),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{e:#}") })),
            )
        }
    };
    match subscription(&tunnel, &topic) {
        Some(sub) => (
            StatusCode::OK,
            Json(json!({ "available": true, "subscription": sub })),
        ),
        None => not_set_up("this machine has no public name"),
    }
}

/// `POST /api/notifications/test` — sends a test notification from this machine.
pub async fn post_test(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let result = async {
        let tunnel = Tunnel::read(&s.config.config_path)?;
        let Some(topic) = topic(Path::new("/"))? else {
            bail!("notifications are not set up on this machine yet");
        };
        let n = Notification {
            title: format!("YoLab {}", tunnel.machine_label()),
            message: "Notifications from this machine work.".into(),
            priority: 3,
            tags: vec!["white_check_mark".into()],
            click: Some(format!("https://{}/", tunnel.host)),
        };
        publish(&topic, &n).await
    }
    .await;
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "sent": true }))),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[tunnel]
enabled = true
platform_api_url = "https://api.yolab.io/"
account_token = "tok"
tunnel_id = "25"
sub_ipv6 = "2a01:4f8::20"
dns_url = "https://node1.6.yolab.io"
"#;

    #[test]
    fn the_server_name_is_the_machine_name_with_a_prefix() {
        let t = Tunnel::parse(CONFIG).unwrap();
        assert!(t.enabled);
        assert_eq!(t.platform_api_url, "https://api.yolab.io");
        assert_eq!(t.ntfy_host().as_deref(), Some("ntfy-node1.6.yolab.io"));
        assert_eq!(t.ntfy_record_name().as_deref(), Some("ntfy-node1"));
        assert_eq!(t.machine_label(), "node1");
        assert!(Tunnel::parse("[homelab]\n").is_err());
    }

    #[test]
    fn the_topic_is_generated_once_and_only_it_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(topic(dir.path()).unwrap(), None);
        let first = ensure_credentials(dir.path()).unwrap();
        assert!(first.starts_with("yolab-") && first.len() == 38, "{first}");
        assert!(valid_topic(&first));
        assert_eq!(ensure_credentials(dir.path()).unwrap(), first, "kept across boots");
        let env = std::fs::read_to_string(dir.path().join(ENV_FILE)).unwrap();
        assert_eq!(env, format!("NTFY_AUTH_ACCESS='*:{first}:rw'\n"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(topic_path(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_damaged_topic_file_is_an_error_not_a_new_topic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(topic_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(topic_path(dir.path()), "not a topic/../x").unwrap();
        assert!(topic(dir.path()).is_err());
        assert!(ensure_credentials(dir.path()).is_err());
    }

    #[test]
    fn the_subscribe_link_opens_the_app_on_this_machines_server() {
        let t = Tunnel::parse(CONFIG).unwrap();
        let sub = subscription(&t, "yolab-abc").unwrap();
        assert_eq!(
            sub.subscribe_url,
            "ntfy://ntfy-node1.6.yolab.io/yolab-abc?display=YoLab+node1"
        );
        assert_eq!(sub.web_url, "https://ntfy-node1.6.yolab.io/yolab-abc");
    }
}
