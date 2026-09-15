//! Phone notifications, through an ntfy server on every machine.
//!
//! WHY ON THE MACHINE, NOT IN KUBERNETES. The notifications that matter most are
//! about Kubernetes or Ceph not working, so neither the server nor the sender
//! may depend on them. ntfy runs as a NixOS service on every machine (see
//! homelab/nixos/common.nix).
//!
//! ONE ADDRESS, EVERY MACHINE. The phone subscribes once, to
//! `notify.<user>.<domain>`, a name every machine shares (`shared_names`): the
//! platform's DNS answers with the machines that are up. So every machine must
//! hold every notification — whichever one the phone reaches. A notification is
//! published on this machine's ntfy and delivered to every other machine that
//! answers (`POST /api/notifications/deliver`).
//!
//! THE TOPIC IS THE SECRET, AND THE SAME EVERYWHERE. The `ntfy://host/topic` link
//! the phone app subscribes from carries no credentials, so every topic is denied
//! but one with an unguessable name, readable and writable by whoever knows it.
//! It is derived from the cluster's k3s token, which every machine of the cluster
//! already has and a FORCE HEAL keeps: identical on every machine without being
//! copied anywhere, different for every cluster, and never revealing the token.

pub(crate) mod alerts;

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::AppState;

/// Where ntfy listens: loopback only. Caddy and local-api are its only clients.
const NTFY_LOCAL: &str = "http://[::1]:2586";
/// The ntfy environment file the NixOS service reads (`environmentFile`).
const ENV_FILE: &str = "var/lib/yolab/ntfy/ntfy.env";

/// The cluster's notification topic, derived from its k3s token.
pub(crate) fn topic_from_config(text: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let table: toml::Table = toml::from_str(text).context("config.toml is not TOML")?;
    let token = table
        .get("node")
        .and_then(|n| n.get("k3s"))
        .and_then(|k| k.get("token"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .context("config.toml has no [node.k3s] token")?;
    let digest = Sha256::digest(format!("yolab-ntfy-topic:{token}").as_bytes());
    Ok(format!("yolab-{}", &hex::encode(digest)[..32]))
}

pub(crate) fn topic(config_path: &str) -> Result<String> {
    let text =
        std::fs::read_to_string(config_path).with_context(|| format!("read {config_path}"))?;
    topic_from_config(&text)
}

/// Boot step, before ntfy starts: the access rule ntfy reads from its
/// environment file — every topic denied (`auth-default-access`), the cluster's
/// topic open to whoever knows its name.
pub(crate) fn write_ntfy_env(root: &Path, config_path: &str) -> Result<()> {
    let topic = topic(config_path)?;
    let env = format!("NTFY_AUTH_ACCESS='*:{topic}:rw'\n");
    crate::config::write_private_file(&root.join(ENV_FILE), env.as_bytes())
}

/// `local-api notify <subcommand>`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("credentials") => {
            let config = crate::config::machine_dir().join("config.toml");
            match write_ntfy_env(Path::new("/"), &config.to_string_lossy()) {
                Ok(()) => 0,
                Err(e) => {
                    tracing::error!("notify credentials: {e:#}");
                    1
                }
            }
        }
        other => {
            eprintln!("notify: unknown subcommand {other:?} (known: credentials)");
            2
        }
    }
}

// ── This machine's public names ───────────────────────────────────────────────

/// The parts of `[tunnel]` in config.toml the shared names need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tunnel {
    pub enabled: bool,
    pub platform_api_url: String,
    pub account_token: String,
    pub tunnel_id: String,
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
        let host = s("dns_url")
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            enabled: t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            platform_api_url: s("platform_api_url").trim_end_matches('/').to_string(),
            account_token: s("account_token"),
            tunnel_id: s("tunnel_id"),
            host,
        })
    }

    /// The user's own zone: the machine's host without its first label,
    /// `6.yolab.io` for `node1.6.yolab.io` — built the same way in common.nix.
    pub fn user_domain(&self) -> Option<&str> {
        self.host.split_once('.').map(|(_, rest)| rest).filter(|r| !r.is_empty())
    }

    /// A name every machine of the user shares, e.g. `notify.6.yolab.io`.
    pub fn shared_host(&self, name: &str) -> Option<String> {
        Some(format!("{name}.{}", self.user_domain()?))
    }

    /// The machine's name for people: the first label of its own host.
    pub fn machine_label(&self) -> String {
        self.host.split('.').next().unwrap_or(&self.host).to_string()
    }
}

// ── Sending ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Notification {
    pub title: String,
    pub message: String,
    /// ntfy's 1 (min) to 5 (max).
    pub priority: u8,
    pub tags: Vec<String>,
    /// Opened when the notification is tapped.
    pub click: Option<String>,
}

/// Publishes on this machine's own server. JSON rather than headers, so titles
/// and messages are not limited to ASCII.
async fn publish_local(topic: &str, n: &Notification) -> Result<()> {
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

/// Publishes on this machine and delivers to every other machine that answers,
/// so whichever machine the phone reaches through the shared name has it.
///
/// Succeeds when this machine has it. A machine that does not take it is down or
/// unreachable — and then the phone is not connected to it either.
pub(crate) async fn publish_everywhere(
    cfg: &crate::config::Config,
    topic: &str,
    n: &Notification,
    peer_addrs: &[String],
) -> Result<()> {
    publish_local(topic, n).await?;
    let client = reqwest::Client::new();
    let token = cfg.cluster_token();
    let deliveries = peer_addrs.iter().map(|addr| {
        let request = client
            .post(format!("http://[{addr}]:{}/api/notifications/deliver", cfg.port))
            .header(crate::auth::CLUSTER_AUTH_HEADER, &token)
            .timeout(Duration::from_secs(10))
            .json(n);
        async move {
            match request.send().await.map(|r| r.error_for_status()) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) | Err(e) => {
                    tracing::warn!("notifications: deliver to [{addr}]: {e}")
                }
            }
        }
    });
    futures::future::join_all(deliveries).await;
    Ok(())
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Serialize)]
struct Subscription {
    topic: String,
    /// Opens the ntfy app and subscribes: what the QR code holds.
    subscribe_url: String,
    /// The same topic in a browser.
    web_url: String,
}

fn subscription(tunnel: &Tunnel, topic: &str) -> Option<Subscription> {
    let host = tunnel.shared_host("notify")?;
    Some(Subscription {
        subscribe_url: format!("ntfy://{host}/{topic}?display=YoLab"),
        web_url: format!("https://{host}/{topic}"),
        topic: topic.to_string(),
    })
}

fn unavailable(why: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({ "available": false, "reason": why.to_string() })),
    )
}

/// `GET /api/notifications` — how to subscribe to the cluster's notifications.
pub async fn get_subscription(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let tunnel = match Tunnel::read(&s.config.config_path) {
        Ok(t) => t,
        Err(e) => return unavailable(format!("{e:#}")),
    };
    if !tunnel.enabled {
        return unavailable("this machine is not connected to the YoLab platform");
    }
    let topic = match topic(&s.config.config_path) {
        Ok(t) => t,
        Err(e) => return unavailable(format!("{e:#}")),
    };
    match subscription(&tunnel, &topic) {
        Some(sub) => (
            StatusCode::OK,
            Json(json!({ "available": true, "subscription": sub })),
        ),
        None => unavailable("this machine has no public name"),
    }
}

/// `POST /api/notifications/test` — a test notification, to every machine.
pub async fn post_test(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let result = async {
        let tunnel = Tunnel::read(&s.config.config_path)?;
        let topic = topic(&s.config.config_path)?;
        let view = crate::heal::current_view(&s.config).await;
        let n = Notification {
            title: "YoLab".into(),
            message: format!("Notifications work. Sent from {}.", tunnel.machine_label()),
            priority: 3,
            tags: vec!["white_check_mark".into()],
            click: tunnel.shared_host("cluster").map(|h| format!("https://{h}/")),
        };
        publish_everywhere(&s.config, &topic, &n, &view.peer_addrs).await
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

/// `POST /api/notifications/deliver` — another machine hands this one a
/// notification to publish on its own server. Node to node (cluster token).
pub async fn post_deliver(
    State(s): State<AppState>,
    Json(n): Json<Notification>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let topic = topic(&s.config.config_path)?;
        publish_local(&topic, &n).await
    }
    .await;
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "published": true }))),
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
dns_url = "https://node1.6.yolab.io"

[node.k3s]
token = "abcdef0123456789"
"#;

    #[test]
    fn shared_names_live_in_the_users_zone() {
        let t = Tunnel::parse(CONFIG).unwrap();
        assert!(t.enabled);
        assert_eq!(t.platform_api_url, "https://api.yolab.io");
        assert_eq!(t.user_domain(), Some("6.yolab.io"));
        assert_eq!(t.shared_host("notify").as_deref(), Some("notify.6.yolab.io"));
        assert_eq!(t.machine_label(), "node1");
        assert!(Tunnel::parse("[homelab]\n").is_err());
        let bare = Tunnel {
            host: "localhost".into(),
            ..t
        };
        assert_eq!(bare.shared_host("notify"), None);
    }

    #[test]
    fn the_topic_is_the_same_for_the_same_cluster_and_hides_the_token() {
        let a = topic_from_config(CONFIG).unwrap();
        assert_eq!(a, topic_from_config(CONFIG).unwrap());
        assert!(a.starts_with("yolab-") && a.len() == 38, "{a}");
        assert!(a[6..].chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.contains("abcdef0123456789"));
        let other = CONFIG.replace("abcdef0123456789", "another-cluster");
        assert_ne!(a, topic_from_config(&other).unwrap());
        assert!(topic_from_config("[node.k3s]\ntoken = \"\"\n").is_err());
    }

    #[test]
    fn only_the_clusters_topic_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, CONFIG).unwrap();
        write_ntfy_env(dir.path(), &config.to_string_lossy()).unwrap();
        let env = std::fs::read_to_string(dir.path().join(ENV_FILE)).unwrap();
        let topic = topic_from_config(CONFIG).unwrap();
        assert_eq!(env, format!("NTFY_AUTH_ACCESS='*:{topic}:rw'\n"));
    }

    #[test]
    fn the_subscribe_link_uses_the_shared_name() {
        let t = Tunnel::parse(CONFIG).unwrap();
        let sub = subscription(&t, "yolab-abc").unwrap();
        assert_eq!(sub.subscribe_url, "ntfy://notify.6.yolab.io/yolab-abc?display=YoLab");
        assert_eq!(sub.web_url, "https://notify.6.yolab.io/yolab-abc");
    }
}
