use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::session;
use crate::store::Store;

const SESSION_DAYS: i64 = 30;
const COOKIE: &str = "yolab_session";

fn now_secs() -> i64 {
    i64::try_from(crate::system::now_secs()).unwrap_or(i64::MAX)
}

fn password_hash(cfg: &Config) -> String {
    cfg.toml()
        .and_then(|t| {
            t.get("homelab")?
                .get("homelab_password_hash")?
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default()
}

pub const CLUSTER_AUTH_HEADER: &str = "x-yolab-cluster";

fn is_loopback(req: &Request<Body>) -> bool {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false)
}

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

#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<Config>,
    store: &'static Mutex<Store>,
    store_path: Option<PathBuf>,
}

impl AuthState {
    pub fn new(config: Arc<Config>) -> Self {
        AuthState {
            config,
            store: crate::store::shared(),
            store_path: Some(crate::store::default_path()),
        }
    }

    #[cfg(test)]
    pub fn isolated(config: Arc<Config>) -> Self {
        AuthState {
            config,
            store: Box::leak(Box::new(Mutex::new(Store::new("test")))),
            store_path: None,
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'static, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn saved(&self, store: &mut Store) -> anyhow::Result<()> {
        if let Some(path) = &self.store_path {
            store.persist(path)?;
            crate::runtime::wake("store-sync");
        }
        Ok(())
    }

    fn valid(&self, token: &str, hash: &str) -> anyhow::Result<bool> {
        let mut store = self.store();
        let key = store.session_key()?;
        let revoked: BTreeMap<String, i64> = store
            .revoked_sessions()?
            .into_iter()
            .map(|(id, entry)| (id, entry.value))
            .collect();
        Ok(session::valid(&key, token, hash, &revoked, now_secs()).is_some())
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response()
}

fn failed(e: anyhow::Error) -> Response {
    tracing::error!("sessions: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": format!("{e:#}") })),
    )
        .into_response()
}

fn session_token(jar: &CookieJar) -> String {
    jar.get(COOKIE)
        .map(|c| c.value().to_string())
        .unwrap_or_default()
}

pub async fn auth_middleware(
    State(state): State<AuthState>,
    jar: CookieJar,
    req: Request<Body>,
    next: Next,
) -> Response {
    if req.uri().path() == "/api/login" {
        return next.run(req).await;
    }
    if has_cluster_token(&req, &state.config) {
        return next.run(req).await;
    }
    let hash = password_hash(&state.config);
    if hash.is_empty() {
        if is_loopback(&req) {
            return next.run(req).await;
        }
        return unauthorized();
    }
    let token = session_token(&jar);
    if token.is_empty() {
        return unauthorized();
    }
    match state.valid(&token, &hash) {
        Ok(true) => next.run(req).await,
        Ok(false) => unauthorized(),
        Err(e) => failed(e),
    }
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
    if !session::verify_password(&body.password, &hash) {
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Wrong password"}"#).into_response();
    }
    let token = {
        let auth = &state.auth;
        let mut store = auth.store();
        let key = match store.session_key() {
            Ok(key) => key,
            Err(e) => return failed(e.into()),
        };
        if let Err(e) = auth.saved(&mut store) {
            return failed(e);
        }
        let expires = now_secs() + SESSION_DAYS * 86400;
        session::issue(&key, &hash, &session::random_token(32), expires)
    };
    let cookie = Cookie::build((COOKIE, token))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .max_age(time::Duration::days(SESSION_DAYS))
        .path("/")
        .build();
    (jar_with(cookie), axum::Json(OkResponse { ok: true })).into_response()
}

pub async fn check() -> StatusCode {
    StatusCode::OK
}

pub async fn logout(State(state): State<crate::AppState>, jar: CookieJar) -> Response {
    let token = session_token(&jar);
    if !token.is_empty() {
        let auth = &state.auth;
        let mut store = auth.store();
        let key = match store.session_key() {
            Ok(key) => key,
            Err(e) => return failed(e.into()),
        };
        if let Some(found) = session::parse(&key, &token) {
            if let Err(e) = store.revoke_session(&found.id, found.expires, now_secs()) {
                return failed(e.into());
            }
            if let Err(e) = auth.saved(&mut store) {
                return failed(e);
            }
        }
    }
    let cookie = Cookie::build((COOKIE, ""))
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
    use axum::{extract::ConnectInfo, routing::get, Router};
    use std::net::SocketAddr;
    use tower::ServiceExt as _;

    const HASH: &str = "$6$UG3IURKt1uqugrtk$i3e3tXg2NMIXuOb9JXztEAwCcsIcfn81WYBkzsfmwA7keyOajafp/PAAlFtcrMHVXo3cXK9z03YRRLaplZZm90";
    const PASSWORD: &str = "password";

    fn write_config(body: &str) -> (tempfile::TempDir, Arc<Config>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        let cfg = Arc::new(Config::for_test(&path));
        (dir, cfg)
    }

    fn provisioned() -> (tempfile::TempDir, Arc<Config>) {
        write_config(&format!(
            "[homelab]\nhomelab_password_hash = \"{HASH}\"\n[tunnel]\naccount_token = \"cluster-tok\"\n"
        ))
    }

    fn unprovisioned() -> (tempfile::TempDir, Arc<Config>) {
        write_config("[homelab]\nhostname = \"yolab\"\n")
    }

    fn router(state: AuthState) -> Router {
        Router::new()
            .route("/api/protected", get(|| async { "reached" }))
            .route("/api/login", get(|| async { "reached" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn request(uri: &str, peer: Option<&str>) -> Request<Body> {
        let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            let addr: SocketAddr = peer.parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        req
    }

    fn with_header(mut req: Request<Body>, name: &'static str, value: &str) -> Request<Body> {
        req.headers_mut().insert(name, value.parse().unwrap());
        req
    }

    fn with_session(req: Request<Body>, token: &str) -> Request<Body> {
        with_header(req, "cookie", &format!("{COOKIE}={token}"))
    }

    async fn status_of(state: AuthState, req: Request<Body>) -> StatusCode {
        router(state).oneshot(req).await.unwrap().status()
    }

    const LOOPBACK: Option<&str> = Some("127.0.0.1:5000");
    const OFF_BOX: Option<&str> = Some("[fd00:42::5]:5000");

    fn app_state(config: Arc<Config>) -> crate::AppState {
        crate::AppState {
            auth: AuthState::isolated(Arc::clone(&config)),
            config,
            kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::Client::new(),
            host: Default::default(),
        }
    }

    fn sibling(state: &crate::AppState, config: Arc<Config>) -> crate::AppState {
        crate::AppState {
            auth: AuthState {
                config: Arc::clone(&config),
                store: state.auth.store,
                store_path: None,
            },
            config,
            kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::Client::new(),
            host: Default::default(),
        }
    }

    async fn do_login(state: &crate::AppState, password: &str) -> Response {
        login(
            State(state.clone()),
            axum::Json(LoginRequest {
                password: password.to_string(),
            }),
        )
        .await
    }

    fn issued_cookie(res: &Response) -> Option<String> {
        res.headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    async fn signed_in_token(state: &crate::AppState) -> String {
        let res = do_login(state, PASSWORD).await;
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = issued_cookie(&res).expect("login should set a cookie");
        cookie
            .split(';')
            .next()
            .and_then(|kv| kv.split_once('='))
            .map(|(_, v)| v.to_string())
            .expect("no session in Set-Cookie")
    }

    async fn reaches(state: &crate::AppState, token: &str) -> StatusCode {
        let req = with_session(request("/api/protected", OFF_BOX), token);
        status_of(state.auth.clone(), req).await
    }

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
        assert!(!ct_eq("", ""));
        assert!(!ct_eq("", "anything"));
        assert!(!ct_eq("anything", ""));
    }

    #[test]
    fn password_hash_reads_the_configured_hash() {
        let (_d, cfg) = provisioned();
        assert_eq!(password_hash(&cfg), HASH);
    }

    #[test]
    fn password_hash_is_empty_when_absent_or_unreadable() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(password_hash(&cfg), "");
        let missing = Config::for_test(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(password_hash(&missing), "");
    }

    #[test]
    fn verify_password_checks_the_configured_hash() {
        assert!(session::verify_password(PASSWORD, HASH));
        assert!(!session::verify_password("not-the-password", HASH));
        assert!(!session::verify_password("", HASH));
    }

    #[tokio::test]
    async fn loopback_is_allowed_when_no_password_is_configured() {
        let (_d, cfg) = unprovisioned();
        let s = AuthState::isolated(cfg);
        assert_eq!(
            status_of(s.clone(), request("/api/protected", LOOPBACK)).await,
            StatusCode::OK
        );
        assert_eq!(
            status_of(s, request("/api/protected", Some("[::1]:5000"))).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn off_box_callers_are_rejected_when_no_password_is_configured() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(
            status_of(AuthState::isolated(cfg), request("/api/protected", OFF_BOX)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_request_with_no_connect_info_is_not_treated_as_loopback() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(
            status_of(AuthState::isolated(cfg), request("/api/protected", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_cluster_token_authorizes_a_call_from_another_node() {
        let (_d, cfg) = provisioned();
        let req = with_header(
            request("/api/protected", OFF_BOX),
            CLUSTER_AUTH_HEADER,
            "cluster-tok",
        );
        assert_eq!(
            status_of(AuthState::isolated(cfg), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_wrong_or_partial_cluster_token_is_rejected() {
        let (_d, cfg) = provisioned();
        let state = AuthState::isolated(cfg);
        for presented in ["wrong-tok", "cluster", "", " "] {
            let req = with_header(
                request("/api/protected", OFF_BOX),
                CLUSTER_AUTH_HEADER,
                presented,
            );
            assert_eq!(
                status_of(state.clone(), req).await,
                StatusCode::UNAUTHORIZED,
                "{presented:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_request_with_no_or_a_made_up_session_is_rejected_once_a_password_exists() {
        let (_d, cfg) = provisioned();
        let state = AuthState::isolated(cfg);
        for req in [
            request("/api/protected", OFF_BOX),
            request("/api/protected", LOOPBACK),
            with_session(request("/api/protected", OFF_BOX), ""),
            with_session(request("/api/protected", OFF_BOX), "never-issued"),
        ] {
            assert_eq!(
                status_of(state.clone(), req).await,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[tokio::test]
    async fn login_is_reachable_without_credentials() {
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(AuthState::isolated(cfg), request("/api/login", OFF_BOX)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn only_the_exact_login_path_is_exempt() {
        let (_d, cfg) = provisioned();
        let state = AuthState::isolated(cfg);
        for path in ["/api/login/", "/api/loginx", "/api/login/../protected"] {
            assert_eq!(
                status_of(state.clone(), request(path, OFF_BOX)).await,
                StatusCode::UNAUTHORIZED,
                "{path} must not inherit the /api/login exemption"
            );
        }
    }

    #[tokio::test]
    async fn login_rejects_the_wrong_password() {
        let (_d, cfg) = provisioned();
        let res = do_login(&app_state(cfg), "hunter2").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            issued_cookie(&res).is_none(),
            "a failed login must not set a cookie"
        );
    }

    #[tokio::test]
    async fn login_with_no_password_configured_signs_nobody_in() {
        let (_d, cfg) = unprovisioned();
        let res = do_login(&app_state(cfg), "any password at all").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_right_password_opens_the_door() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        assert_eq!(reaches(&state, &token).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn login_marks_the_cookie_http_only_secure_and_same_site_strict() {
        let (_d, cfg) = provisioned();
        let res = do_login(&app_state(cfg), PASSWORD).await;
        let cookie = issued_cookie(&res).expect("login should set a cookie");
        assert!(cookie.contains("HttpOnly"), "got: {cookie}");
        assert!(cookie.contains("Secure"), "got: {cookie}");
        assert!(cookie.contains("SameSite=Strict"), "got: {cookie}");
        assert!(cookie.contains("Path=/"), "got: {cookie}");
    }

    #[tokio::test]
    async fn login_issues_a_different_token_every_time() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_ne!(signed_in_token(&state).await, signed_in_token(&state).await);
    }

    #[tokio::test]
    async fn a_session_from_one_machine_opens_another_that_shares_its_store() {
        let (_d, cfg) = provisioned();
        let first = app_state(cfg);
        let token = signed_in_token(&first).await;
        let (_d2, other_cfg) = provisioned();
        let second = sibling(&first, other_cfg);
        assert_eq!(reaches(&second, &token).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_session_from_another_cluster_is_refused() {
        let (_d, cfg) = provisioned();
        let token = signed_in_token(&app_state(Arc::clone(&cfg))).await;
        assert_eq!(
            reaches(&app_state(cfg), &token).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn changing_the_machine_password_signs_everyone_out() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        let (_d2, changed) = write_config(
            "[homelab]\nhomelab_password_hash = \"$6$other$hash\"\n[tunnel]\naccount_token = \"cluster-tok\"\n",
        );
        assert_eq!(
            reaches(&sibling(&state, changed), &token).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_logged_out_token_no_longer_passes_the_middleware() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        let other = signed_in_token(&state).await;
        assert_eq!(reaches(&state, &token).await, StatusCode::OK);

        let jar = CookieJar::new().add(Cookie::new(COOKIE, token.clone()));
        let res = logout(State(state.clone()), jar).await;
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = issued_cookie(&res).expect("logout should clear the cookie");
        assert!(cookie.contains("Max-Age=0"), "got: {cookie}");

        assert_eq!(reaches(&state, &token).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            reaches(&state, &other).await,
            StatusCode::OK,
            "signing out ends only the session it was sent with"
        );
    }

    #[tokio::test]
    async fn logout_without_a_cookie_is_harmless() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        let res = logout(State(state.clone()), CookieJar::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reaches(&state, &token).await, StatusCode::OK);
    }
}
