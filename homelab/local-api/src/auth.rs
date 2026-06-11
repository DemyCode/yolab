use std::{collections::HashSet, sync::Arc};

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

pub type Sessions = Arc<RwLock<HashSet<String>>>;

pub fn new_sessions() -> Sessions {
    Arc::new(RwLock::new(HashSet::new()))
}

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
        .map(|ci| {
            let ip = ci.0.ip().to_string().to_lowercase();
            ip.starts_with("fd") || ip.starts_with("fc")
        })
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct AuthState {
    pub sessions: Sessions,
    pub config: Arc<Config>,
}

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
    if token.is_empty() || !state.sessions.read().await.contains(&token) {
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response();
    }
    next.run(req).await
}

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
    state.auth.sessions.write().await.insert(token.clone());
    let cookie = Cookie::build(("yolab_session", token))
        .http_only(true)
        .same_site(SameSite::Strict)
        .max_age(time::Duration::days(30))
        .path("/")
        .build();
    (jar_with(cookie), axum::Json(OkResponse { ok: true })).into_response()
}

pub async fn logout() -> Response {
    let cookie = Cookie::build(("yolab_session", ""))
        .max_age(time::Duration::seconds(0))
        .path("/")
        .build();
    (jar_with(cookie), axum::Json(OkResponse { ok: true })).into_response()
}

fn jar_with(cookie: Cookie<'static>) -> CookieJar {
    CookieJar::new().add(cookie)
}
