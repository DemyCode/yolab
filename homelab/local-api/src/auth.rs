use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::Config;

// Token → expiry timestamp (unix seconds).
pub type Sessions = Arc<RwLock<HashMap<String, i64>>>;

const SESSION_DAYS: i64 = 30;
const SECRET_NAME: &str = "yolab-sessions";
const SECRET_NS: &str = "kube-system";

pub fn new_sessions() -> Sessions {
    Arc::new(RwLock::new(HashMap::new()))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── K8s Secret persistence ────────────────────────────────────────────────────

async fn load_sessions_from_k8s() -> HashMap<String, i64> {
    let Some(data) = crate::kubectl::get_secret(SECRET_NAME, SECRET_NS).await else {
        return HashMap::new();
    };
    let Some(json) = data.get("sessions") else {
        return HashMap::new();
    };
    let now = now_secs();
    serde_json::from_str::<HashMap<String, i64>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, exp)| *exp > now) // drop expired sessions on load
        .collect()
}

async fn save_sessions_to_k8s(sessions: &HashMap<String, i64>) {
    let json = serde_json::to_string(sessions).unwrap_or_default();
    if let Err(e) = crate::kubectl::apply_secret(
        SECRET_NAME,
        SECRET_NS,
        &[("sessions", &json)],
        &[],
    )
    .await
    {
        tracing::warn!("failed to persist sessions to k8s: {e}");
    }
}

// ── Public init ───────────────────────────────────────────────────────────────

/// Load persisted sessions at startup so users survive local-api restarts.
pub async fn init_sessions(sessions: &Sessions) {
    let loaded = load_sessions_from_k8s().await;
    if !loaded.is_empty() {
        tracing::info!("restored {} session(s) from k8s secret", loaded.len());
        *sessions.write().await = loaded;
    }
}

// ── Auth helpers ──────────────────────────────────────────────────────────────

fn password_hash(cfg: &Config) -> String {
    let text = std::fs::read_to_string(&cfg.config_path).unwrap_or_default();
    let table: toml::Table = toml::from_str(&text).unwrap_or_default();
    table
        .get("homelab")
        .and_then(|h| h.get("homelab_password_hash"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn verify_password(password: &str, hash: &str) -> bool {
    if hash.is_empty() {
        return false;
    }
    pwhash::unix::verify(password, hash)
}

/// Header carrying the pre-shared cluster token on node→node calls.
pub const CLUSTER_AUTH_HEADER: &str = "x-yolab-cluster";

/// True only when the request originates from this machine's loopback
/// interface. Caddy reverse-proxies public UI/API traffic from `[::1]`, and
/// mac/dev setups hit `localhost` directly — both are loopback. Anything
/// arriving from a WireGuard address (mesh peers) or a pod IP is NOT loopback.
fn is_loopback(req: &Request<Body>) -> bool {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false)
}

/// Constant-time byte comparison so token checks don't leak length/prefix via
/// timing. Both must be non-empty to ever return true.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// True when the request presents the shared cluster token (node→node call).
fn has_cluster_token(req: &Request<Body>, cfg: &Config) -> bool {
    let Some(presented) = req
        .headers()
        .get(CLUSTER_AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    ct_eq(presented, &cfg.cluster_token())
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AuthState {
    pub sessions: Sessions,
    pub config: Arc<Config>,
}

// ── Middleware ────────────────────────────────────────────────────────────────

pub async fn auth_middleware(
    State(state): State<AuthState>,
    jar: CookieJar,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if path == "/api/login" {
        return next.run(req).await;
    }
    // Node→node calls authenticate with the pre-shared cluster token, NOT by
    // source address. The old code trusted any peer in fc00::/7 — but pod IPs
    // (fd00:42::/…) fall in that range, so any pod could reach a node's private
    // address and call privileged endpoints (e.g. /api/terminal/exec) unauthed.
    if has_cluster_token(&req, &state.config) {
        return next.run(req).await;
    }
    let hash = password_hash(&state.config);
    if hash.is_empty() {
        // No password configured (mac/dev, or a not-yet-provisioned node).
        // Fail closed for anything off-box; only same-machine loopback callers
        // — i.e. the Caddy reverse proxy and localhost dev — are allowed
        // through. A provisioned NixOS node always has a hash, so this branch
        // never opens the door remotely in production.
        if is_loopback(&req) {
            return next.run(req).await;
        }
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response();
    }
    let token = jar.get("yolab_session").map(|c| c.value().to_string()).unwrap_or_default();
    let valid = if token.is_empty() {
        false
    } else {
        let sessions = state.sessions.read().await;
        sessions.get(&token).map(|&exp| exp > now_secs()).unwrap_or(false)
    };
    if !valid {
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response();
    }
    next.run(req).await
}

// ── Handlers ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

pub async fn login(
    State(state): State<crate::AppState>,
    axum::Json(body): axum::Json<LoginRequest>,
) -> Response {
    let hash = password_hash(&state.config);
    if !hash.is_empty() && !verify_password(&body.password, &hash) {
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Wrong password"}"#).into_response();
    }
    let token: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let expiry = now_secs() + SESSION_DAYS * 86400;
    {
        let mut sessions = state.auth.sessions.write().await;
        sessions.insert(token.clone(), expiry);
        save_sessions_to_k8s(&sessions).await;
    }
    let cookie = Cookie::build(("yolab_session", token))
        .http_only(true)
        .same_site(SameSite::Strict)
        .max_age(time::Duration::days(SESSION_DAYS))
        .path("/")
        .build();
    (jar_with(cookie), axum::Json(OkResponse { ok: true })).into_response()
}

/// Used by Caddy forward_auth to gate access to /glances.
/// Returns 200 if the session cookie is valid; the middleware returns 401 otherwise.
pub async fn check() -> StatusCode {
    StatusCode::OK
}

pub async fn logout(
    State(state): State<crate::AppState>,
    jar: CookieJar,
) -> Response {
    let token = jar.get("yolab_session").map(|c| c.value().to_string()).unwrap_or_default();
    if !token.is_empty() {
        let mut sessions = state.auth.sessions.write().await;
        sessions.remove(&token);
        save_sessions_to_k8s(&sessions).await;
    }
    let cookie = Cookie::build(("yolab_session", ""))
        .max_age(time::Duration::seconds(0))
        .path("/")
        .build();
    (jar_with(cookie), axum::Json(OkResponse { ok: true })).into_response()
}

fn jar_with(cookie: Cookie<'static>) -> CookieJar {
    CookieJar::new().add(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_identical() {
        assert!(ct_eq("s3cret-token", "s3cret-token"));
    }

    #[test]
    fn ct_eq_rejects_different() {
        assert!(!ct_eq("s3cret-token", "s3cret-tokeN"));
        assert!(!ct_eq("short", "longer-value"));
    }

    #[test]
    fn ct_eq_rejects_empty() {
        // An unreadable/absent token must never authorize a caller.
        assert!(!ct_eq("", ""));
        assert!(!ct_eq("", "anything"));
        assert!(!ct_eq("anything", ""));
    }
}
