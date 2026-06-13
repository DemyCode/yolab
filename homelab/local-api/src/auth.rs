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
const SECRET_NS: &str = "rook-ceph";

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
    let out = tokio::process::Command::new("kubectl")
        .args([
            "get", "secret", SECRET_NAME, "-n", SECRET_NS,
            "-o", "jsonpath={.data.sessions}",
        ])
        .output()
        .await;
    let Ok(out) = out else { return HashMap::new() };
    let b64 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b64.is_empty() {
        return HashMap::new();
    }
    // base64-decode the value stored by kubectl
    let Ok(decoded) = std::process::Command::new("base64")
        .args(["-d"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(b64.as_bytes()).ok();
            c.wait_with_output()
        })
    else {
        return HashMap::new();
    };
    let json = String::from_utf8_lossy(&decoded.stdout);
    let now = now_secs();
    serde_json::from_str::<HashMap<String, i64>>(&json)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, exp)| *exp > now)   // drop expired sessions on load
        .collect()
}

async fn save_sessions_to_k8s(sessions: &HashMap<String, i64>) {
    let json = serde_json::to_string(sessions).unwrap_or_default();
    // kubectl create/apply a Secret with the JSON as a string data field.
    // Using --dry-run=client + apply avoids "already exists" errors.
    let manifest = format!(
        r#"{{"apiVersion":"v1","kind":"Secret","metadata":{{"name":"{SECRET_NAME}","namespace":"{SECRET_NS}"}},"stringData":{{"sessions":{}}}}}"#,
        serde_json::to_string(&json).unwrap_or_default()
    );
    let _ = tokio::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(stdin) = c.stdin.as_mut() {
                let _ = stdin.write_all(manifest.as_bytes());
            }
            Ok(c)
        });
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

fn is_cluster_internal(req: &Request<Body>) -> bool {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| match ci.0.ip() {
            std::net::IpAddr::V6(v6) => {
                let b = v6.octets()[0];
                b == 0xfc || b == 0xfd
            }
            _ => false,
        })
        .unwrap_or(false)
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
    if is_cluster_internal(&req) {
        return next.run(req).await;
    }
    let hash = password_hash(&state.config);
    if hash.is_empty() {
        return next.run(req).await;
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
