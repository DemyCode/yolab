use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Extension, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::session;
use crate::store::Store;

const SESSION_DAYS: i64 = 30;
pub(crate) const COOKIE: &str = "__Host-yolab_session";

pub const LOGIN_WINDOW_SECS: i64 = 15 * 60;
pub const LOGIN_FAILURES_PER_CLIENT: usize = 10;
pub const LOGIN_FAILURES_OVERALL: usize = 200;

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

fn peer_ip(req: &Request<Body>) -> Option<IpAddr> {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
}

fn is_proxied(headers: &HeaderMap) -> bool {
    headers.contains_key("x-forwarded-for")
        || headers.contains_key(header::FORWARDED)
        || headers.contains_key(header::VIA)
}

fn is_loopback(req: &Request<Body>) -> bool {
    peer_ip(req).is_some_and(|ip| ip.is_loopback()) && !is_proxied(req.headers())
}

fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn client_bucket(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn is_cross_site_write(req: &Request<Body>) -> bool {
    use axum::http::Method;
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }
    let headers = req.headers();
    if let Some(site) = header_text(headers, "sec-fetch-site") {
        if !matches!(site, "same-origin" | "none") {
            return true;
        }
    }
    let Some(origin) = header_text(headers, "origin") else {
        return false;
    };
    let Some(host) = header_text(headers, "host") else {
        return true;
    };
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    !authority.is_some_and(|a| a.eq_ignore_ascii_case(host))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginClient(pub String);

fn login_client(req: &Request<Body>) -> LoginClient {
    let ip = match peer_ip(req) {
        Some(peer) if peer.is_loopback() => forwarded_for(req.headers()).unwrap_or(peer),
        Some(peer) => peer,
        None => return LoginClient("unknown".into()),
    };
    LoginClient(client_bucket(ip))
}

#[derive(Default)]
pub struct LoginFailures {
    by_client: HashMap<String, Vec<i64>>,
}

impl LoginFailures {
    fn prune(&mut self, now: i64) {
        self.by_client.retain(|_, times| {
            times.retain(|t| now - *t < LOGIN_WINDOW_SECS);
            !times.is_empty()
        });
    }

    fn wait_for(times: &[i64], now: i64) -> Option<i64> {
        times
            .iter()
            .min()
            .map(|oldest| (oldest + LOGIN_WINDOW_SECS - now).max(1))
    }

    pub fn retry_after(&mut self, client: &str, now: i64) -> Option<i64> {
        self.prune(now);
        if let Some(times) = self.by_client.get(client) {
            if times.len() >= LOGIN_FAILURES_PER_CLIENT {
                return Self::wait_for(times, now);
            }
        }
        let all: Vec<i64> = self.by_client.values().flatten().copied().collect();
        if all.len() >= LOGIN_FAILURES_OVERALL {
            return Self::wait_for(&all, now);
        }
        None
    }

    pub fn failed(&mut self, client: &str, now: i64) {
        self.by_client
            .entry(client.to_string())
            .or_default()
            .push(now);
    }

    pub fn succeeded(&mut self, client: &str) {
        self.by_client.remove(client);
    }
}

fn has_cluster_token(req: &Request<Body>, cfg: &Config) -> bool {
    let Some(presented) = req
        .headers()
        .get(CLUSTER_AUTH_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    crate::session::ct_eq(presented, &cfg.cluster_token())
}

#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<Config>,
    store: &'static Mutex<Store>,
    store_path: Option<PathBuf>,
    failures: Arc<Mutex<LoginFailures>>,
}

impl AuthState {
    pub fn new(config: Arc<Config>) -> Self {
        AuthState {
            config,
            store: crate::store::shared(),
            store_path: Some(crate::store::default_path()),
            failures: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn isolated(config: Arc<Config>) -> Self {
        AuthState {
            config,
            store: Box::leak(Box::new(Mutex::new(Store::new("test")))),
            store_path: None,
            failures: Arc::default(),
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'static, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn failures(&self) -> std::sync::MutexGuard<'_, LoginFailures> {
        self.failures
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
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if req.uri().path() == "/api/login" {
        let client = login_client(&req);
        req.extensions_mut().insert(client);
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
    if is_cross_site_write(&req) {
        return (
            StatusCode::FORBIDDEN,
            r#"{"detail":"This change must come from this machine's own page"}"#,
        )
            .into_response();
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

fn too_many_attempts(retry_after: i64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after.to_string())],
        r#"{"detail":"Too many wrong passwords, try again later"}"#,
    )
        .into_response()
}

pub async fn login(
    State(state): State<crate::AppState>,
    Extension(LoginClient(client)): Extension<LoginClient>,
    axum::Json(body): axum::Json<LoginRequest>,
) -> Response {
    let blocked = state.auth.failures().retry_after(&client, now_secs());
    if let Some(wait) = blocked {
        return too_many_attempts(wait);
    }
    let hash = password_hash(&state.config);
    if !session::verify_password(&body.password, &hash) {
        state.auth.failures().failed(&client, now_secs());
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Wrong password"}"#).into_response();
    }
    state.auth.failures().succeeded(&client);
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
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Strict)
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
                failures: Arc::default(),
            },
            config,
            kube: crate::k8s::Kube::with(crate::k8s::testing::unreachable()),
            http: crate::http::Client::new(),
            host: Default::default(),
        }
    }

    async fn do_login(state: &crate::AppState, password: &str) -> Response {
        login_from(state, "2001:db8:1:2::/64", password).await
    }

    async fn login_from(state: &crate::AppState, client: &str, password: &str) -> Response {
        login(
            State(state.clone()),
            Extension(LoginClient(client.to_string())),
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
        assert!(
            cookie.starts_with("__Host-") && !cookie.to_ascii_lowercase().contains("domain="),
            "a sibling *.yolab.io page can plant a cookie for the whole domain unless the name carries the __Host- prefix: {cookie}"
        );
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

    fn client_of(peer: Option<&str>, forwarded: Option<&str>) -> String {
        let mut req = request("/api/login", peer);
        if let Some(xff) = forwarded {
            req = with_header(req, "x-forwarded-for", xff);
        }
        login_client(&req).0
    }

    #[tokio::test]
    async fn a_request_caddy_forwarded_from_the_internet_is_not_loopback() {
        let (_d, cfg) = unprovisioned();
        let s = AuthState::isolated(cfg);
        for (name, value) in [
            ("x-forwarded-for", "203.0.113.9"),
            ("forwarded", "for=203.0.113.9"),
            ("via", "1.1 Caddy"),
        ] {
            let req = with_header(request("/api/protected", Some("[::1]:5000")), name, value);
            assert_eq!(
                status_of(s.clone(), req).await,
                StatusCode::UNAUTHORIZED,
                "a {name} header means a proxy relayed it, so the passwordless loopback door stays shut"
            );
        }
    }

    #[test]
    fn the_login_client_is_the_visitor_caddy_names_not_caddy_itself() {
        assert_eq!(
            client_of(Some("[::1]:5000"), Some("203.0.113.9")),
            "203.0.113.9"
        );
        assert_eq!(
            client_of(Some("[::1]:5000"), Some("2001:db8:aa:bb:1:2:3:4, 10.0.0.1")),
            "2001:db8:aa:bb::/64"
        );
    }

    #[test]
    fn an_off_box_caller_cannot_choose_its_login_client_with_a_header() {
        assert_eq!(
            client_of(Some("[2001:db8:5:6::9]:5000"), Some("198.51.100.1")),
            "2001:db8:5:6::/64"
        );
    }

    #[test]
    fn every_address_in_one_ipv6_64_counts_as_one_client() {
        assert_eq!(
            client_of(Some("[2001:db8:1:2::1]:1"), None),
            client_of(Some("[2001:db8:1:2:ffff:ffff:ffff:ffff]:1"), None)
        );
        assert_ne!(
            client_of(Some("[2001:db8:1:2::1]:1"), None),
            client_of(Some("[2001:db8:1:3::1]:1"), None)
        );
        assert_eq!(
            client_of(Some("[::ffff:198.51.100.7]:1"), None),
            "198.51.100.7"
        );
    }

    #[test]
    fn a_garbled_forwarded_for_falls_back_to_the_peer() {
        assert_eq!(
            client_of(Some("127.0.0.1:5000"), Some("not-an-ip")),
            "127.0.0.1"
        );
    }

    #[tokio::test]
    async fn the_middleware_hands_the_login_handler_its_client() {
        let (_d, cfg) = provisioned();
        let app = Router::new()
            .route(
                "/api/login",
                get(|Extension(LoginClient(c)): Extension<LoginClient>| async move { c }),
            )
            .layer(axum::middleware::from_fn_with_state(
                AuthState::isolated(cfg),
                auth_middleware,
            ));
        let req = with_header(
            request("/api/login", Some("[::1]:5000")),
            "x-forwarded-for",
            "203.0.113.9",
        );
        let res = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"203.0.113.9");
    }

    #[tokio::test]
    async fn wrong_passwords_lock_that_client_out_for_a_while() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        for _ in 0..LOGIN_FAILURES_PER_CLIENT {
            assert_eq!(
                login_from(&state, "a", "wrong").await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let res = login_from(&state, "a", PASSWORD).await;
        assert_eq!(
            res.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "even the right password waits once the guesses ran out, or the lockout tells a guesser when they hit it"
        );
        assert!(issued_cookie(&res).is_none());
        let wait: i64 = res.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(wait > 0 && wait <= LOGIN_WINDOW_SECS, "Retry-After {wait}");
        assert_eq!(
            login_from(&state, "b", PASSWORD).await.status(),
            StatusCode::OK,
            "another client is not punished for the first one's guesses"
        );
    }

    #[test]
    fn failures_older_than_the_window_are_forgiven() {
        let mut f = LoginFailures::default();
        for _ in 0..LOGIN_FAILURES_PER_CLIENT {
            f.failed("a", 1_000);
        }
        assert!(f.retry_after("a", 1_000).is_some());
        assert!(f.retry_after("a", 1_000 + LOGIN_WINDOW_SECS - 1).is_some());
        assert_eq!(f.retry_after("a", 1_000 + LOGIN_WINDOW_SECS), None);
        assert!(
            f.by_client.is_empty(),
            "expired entries do not pile up in memory"
        );
    }

    #[test]
    fn a_right_password_clears_that_clients_count() {
        let mut f = LoginFailures::default();
        for _ in 0..LOGIN_FAILURES_PER_CLIENT - 1 {
            f.failed("a", 1_000);
        }
        f.succeeded("a");
        f.failed("a", 1_000);
        assert_eq!(f.retry_after("a", 1_000), None);
    }

    #[test]
    fn guesses_spread_over_many_clients_still_hit_an_overall_cap() {
        let mut f = LoginFailures::default();
        for i in 0..LOGIN_FAILURES_OVERALL {
            f.failed(&format!("client-{i}"), 1_000);
        }
        assert!(
            f.retry_after("a-fresh-client", 1_000).is_some(),
            "rotating through addresses must not buy unlimited guesses"
        );
    }

    #[tokio::test]
    async fn login_is_throttled_before_the_password_is_checked() {
        let (_d, cfg) = unprovisioned();
        let state = app_state(cfg);
        for _ in 0..LOGIN_FAILURES_PER_CLIENT {
            login_from(&state, "a", "x").await;
        }
        assert_eq!(
            login_from(&state, "a", "x").await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    fn write_router(state: AuthState) -> Router {
        Router::new()
            .route(
                "/api/system/reboot/all",
                axum::routing::post(|| async { "rebooting" }),
            )
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    fn signed_in_post(token: &str, headers: &[(&'static str, &str)]) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/system/reboot/all")
            .body(Body::empty())
            .unwrap();
        let addr: SocketAddr = "[::1]:5000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req = with_header(req, "host", "cluster.5.yolab.io");
        req = with_header(req, "x-forwarded-for", "203.0.113.9");
        for &(name, value) in headers {
            req = with_header(req, name, value);
        }
        with_session(req, token)
    }

    async fn post_status(
        state: &crate::AppState,
        token: &str,
        headers: &[(&'static str, &str)],
    ) -> StatusCode {
        write_router(state.auth.clone())
            .oneshot(signed_in_post(token, headers))
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn another_customers_page_cannot_make_your_browser_change_your_machine() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        for headers in [
            vec![
                ("origin", "https://evil.9.yolab.io"),
                ("sec-fetch-site", "same-site"),
            ],
            vec![("origin", "https://evil.9.yolab.io")],
            vec![("sec-fetch-site", "same-site")],
            vec![("sec-fetch-site", "cross-site")],
            vec![("origin", "null")],
            vec![("origin", "https://cluster.5.yolab.io.evil.example")],
        ] {
            assert_eq!(
                post_status(&state, &token, &headers).await,
                StatusCode::FORBIDDEN,
                "every customer's services share yolab.io, so SameSite=Strict still sends \
                 the cookie from their pages: {headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_machines_own_page_still_changes_things() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        for headers in [
            vec![
                ("origin", "https://cluster.5.yolab.io"),
                ("sec-fetch-site", "same-origin"),
            ],
            vec![("origin", "https://CLUSTER.5.yolab.io")],
            vec![("sec-fetch-site", "same-origin")],
            vec![],
        ] {
            assert_eq!(
                post_status(&state, &token, &headers).await,
                StatusCode::OK,
                "{headers:?}"
            );
        }
    }

    #[tokio::test]
    async fn reading_is_not_blocked_by_the_write_check() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        let req = with_header(
            with_session(request("/api/protected", OFF_BOX), &token),
            "sec-fetch-site",
            "same-site",
        );
        assert_eq!(status_of(state.auth.clone(), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn another_machine_with_the_cluster_token_is_not_a_browser() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/system/reboot/all")
            .body(Body::empty())
            .unwrap();
        req = with_header(req, CLUSTER_AUTH_HEADER, "cluster-tok");
        req = with_header(req, "origin", "https://elsewhere.example");
        let res = write_router(state.auth.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
