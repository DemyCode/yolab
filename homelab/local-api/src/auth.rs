use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::store::Store;
use crate::users::{self, Account, ADMIN, MIN_PASSWORD_LEN};

const SESSION_DAYS: i64 = 30;
const COOKIE: &str = "yolab_session";

fn now_secs() -> i64 {
    i64::try_from(crate::system::now_secs()).unwrap_or(i64::MAX)
}

fn legacy_admin_hash(cfg: &Config) -> String {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedIn(pub String);

struct Book {
    accounts: BTreeMap<String, String>,
    stored: bool,
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

    fn book(&self, store: &Store) -> anyhow::Result<Book> {
        let stored: BTreeMap<String, Account> = store
            .accounts()?
            .into_iter()
            .map(|(name, entry)| (name, entry.value))
            .collect();
        Ok(Book {
            accounts: users::active(&stored, &legacy_admin_hash(&self.config)),
            stored: !stored.is_empty(),
        })
    }

    fn saved(&self, store: &mut Store) -> anyhow::Result<()> {
        if let Some(path) = &self.store_path {
            store.persist(path)?;
            crate::runtime::wake("store-sync");
        }
        Ok(())
    }

    fn signed_in(&self, token: &str) -> anyhow::Result<Option<users::Session>> {
        let mut store = self.store();
        let book = self.book(&store)?;
        if book.accounts.is_empty() {
            return Ok(None);
        }
        let key = store.session_key()?;
        let revoked: BTreeMap<String, i64> = store
            .revoked_sessions()?
            .into_iter()
            .map(|(id, entry)| (id, entry.value))
            .collect();
        Ok(users::signed_in(
            &key,
            token,
            &book.accounts,
            &revoked,
            now_secs(),
        ))
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response()
}

fn failed(e: anyhow::Error) -> Response {
    tracing::error!("accounts: {e:#}");
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
        return next.run(req).await;
    }
    if has_cluster_token(&req, &state.config) {
        return next.run(req).await;
    }
    let no_account = match state.book(&state.store()) {
        Ok(book) => book.accounts.is_empty(),
        Err(e) => return failed(e),
    };
    if no_account {
        if is_loopback(&req) {
            return next.run(req).await;
        }
        return unauthorized();
    }
    let token = session_token(&jar);
    if token.is_empty() {
        return unauthorized();
    }
    match state.signed_in(&token) {
        Ok(Some(session)) => {
            req.extensions_mut().insert(SignedIn(session.user));
            next.run(req).await
        }
        Ok(None) => unauthorized(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub username: Option<String>,
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
    let user = body
        .username
        .map(|u| u.trim().to_lowercase())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| ADMIN.to_string());
    let issued = {
        let auth = &state.auth;
        let mut store = auth.store();
        let book = match auth.book(&store) {
            Ok(book) => book,
            Err(e) => return failed(e),
        };
        let Some(hash) = book.accounts.get(&user) else {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"detail":"Wrong username or password"}"#,
            )
                .into_response();
        };
        if !users::verify_password(&body.password, hash) {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"detail":"Wrong username or password"}"#,
            )
                .into_response();
        }
        let key = match store.session_key() {
            Ok(key) => key,
            Err(e) => return failed(e.into()),
        };
        if let Err(e) = auth.saved(&mut store) {
            return failed(e);
        }
        let expires = now_secs() + SESSION_DAYS * 86400;
        users::issue(&key, &user, hash, &users::random_token(32), expires)
    };
    let cookie = Cookie::build((COOKIE, issued))
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
        if let Some(session) = users::parse(&key, &token) {
            if let Err(e) = store.revoke_session(&session.id, session.expires, now_secs()) {
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

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct AccountList {
    pub me: Option<String>,
    pub users: Vec<String>,
}

pub async fn me(signed_in: Option<Extension<SignedIn>>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "username": signed_in.map(|Extension(s)| s.0) }))
}

pub async fn list_users(
    State(state): State<crate::AppState>,
    signed_in: Option<Extension<SignedIn>>,
) -> Response {
    let store = state.auth.store();
    match state.auth.book(&store) {
        Ok(book) => axum::Json(AccountList {
            me: signed_in.map(|Extension(s)| s.0),
            users: book.accounts.into_keys().collect(),
        })
        .into_response(),
        Err(e) => failed(e),
    }
}

#[derive(Deserialize)]
pub struct PasswordRequest {
    pub password: String,
}

fn refused(status: StatusCode, detail: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": detail }))).into_response()
}

pub async fn put_user(
    State(state): State<crate::AppState>,
    Path(name): Path<String>,
    axum::Json(body): axum::Json<PasswordRequest>,
) -> Response {
    if !users::valid_username(&name) {
        return refused(
            StatusCode::BAD_REQUEST,
            "A username is 1 to 32 lowercase letters, digits, - or _, starting with a letter or digit",
        );
    }
    if body.password.chars().count() < MIN_PASSWORD_LEN {
        return refused(
            StatusCode::BAD_REQUEST,
            &format!("A password needs at least {MIN_PASSWORD_LEN} characters"),
        );
    }
    let hash = match users::hash_password(&body.password) {
        Ok(hash) => hash,
        Err(e) => return failed(e),
    };
    let auth = &state.auth;
    let mut store = auth.store();
    let book = match auth.book(&store) {
        Ok(book) => book,
        Err(e) => return failed(e),
    };
    if !book.stored {
        for (user, legacy) in &book.accounts {
            let kept = Account::Active {
                hash: legacy.clone(),
            };
            if let Err(e) = store.set_account(user, &kept) {
                return failed(e.into());
            }
        }
    }
    if let Err(e) = store.set_account(&name, &Account::Active { hash }) {
        return failed(e.into());
    }
    if let Err(e) = auth.saved(&mut store) {
        return failed(e);
    }
    axum::Json(OkResponse { ok: true }).into_response()
}

pub async fn delete_user(
    State(state): State<crate::AppState>,
    Path(name): Path<String>,
) -> Response {
    let auth = &state.auth;
    let mut store = auth.store();
    let book = match auth.book(&store) {
        Ok(book) => book,
        Err(e) => return failed(e),
    };
    if !book.accounts.contains_key(&name) {
        return refused(StatusCode::NOT_FOUND, "There is no such user");
    }
    if book.accounts.len() == 1 {
        return refused(
            StatusCode::CONFLICT,
            "This is the last user: add another one before removing it, or nobody could sign in",
        );
    }
    if !book.stored {
        for (user, legacy) in &book.accounts {
            let kept = Account::Active {
                hash: legacy.clone(),
            };
            if let Err(e) = store.set_account(user, &kept) {
                return failed(e.into());
            }
        }
    }
    if let Err(e) = store.set_account(&name, &Account::Removed) {
        return failed(e.into());
    }
    if let Err(e) = auth.saved(&mut store) {
        return failed(e);
    }
    axum::Json(OkResponse { ok: true }).into_response()
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

    async fn do_login(state: &crate::AppState, username: Option<&str>, password: &str) -> Response {
        login(
            State(state.clone()),
            axum::Json(LoginRequest {
                username: username.map(str::to_string),
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

    fn token_of(res: &Response) -> String {
        let cookie = issued_cookie(res).expect("login should set a cookie");
        cookie
            .split(';')
            .next()
            .and_then(|kv| kv.split_once('='))
            .map(|(_, v)| v.to_string())
            .expect("no session in Set-Cookie")
    }

    async fn signed_in_token(state: &crate::AppState) -> String {
        let res = do_login(state, None, PASSWORD).await;
        assert_eq!(res.status(), StatusCode::OK);
        token_of(&res)
    }

    async fn add_user(state: &crate::AppState, name: &str, password: &str) -> StatusCode {
        put_user(
            State(state.clone()),
            Path(name.to_string()),
            axum::Json(PasswordRequest {
                password: password.to_string(),
            }),
        )
        .await
        .status()
    }

    async fn remove_user(state: &crate::AppState, name: &str) -> StatusCode {
        delete_user(State(state.clone()), Path(name.to_string()))
            .await
            .status()
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
    fn the_machine_password_is_read_from_the_config() {
        let (_d, cfg) = provisioned();
        assert_eq!(legacy_admin_hash(&cfg), HASH);
        let (_d, cfg) = unprovisioned();
        assert_eq!(legacy_admin_hash(&cfg), "");
        let missing = Config::for_test(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(legacy_admin_hash(&missing), "");
    }

    #[tokio::test]
    async fn loopback_is_allowed_when_no_account_exists() {
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
    async fn off_box_callers_are_rejected_when_no_account_exists() {
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
    async fn a_request_with_no_or_a_made_up_session_is_rejected_once_an_account_exists() {
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
    async fn the_machine_password_signs_in_as_admin() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        assert_eq!(reaches(&state, &token).await, StatusCode::OK);
        let named = do_login(&state, Some("Admin"), PASSWORD).await;
        assert_eq!(named.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_rejects_the_wrong_password_or_an_unknown_user() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        for (user, password) in [(None, "hunter2"), (Some("nobody"), PASSWORD)] {
            let res = do_login(&state, user, password).await;
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
            assert!(
                issued_cookie(&res).is_none(),
                "a failed login must not set a cookie"
            );
        }
    }

    #[tokio::test]
    async fn login_with_no_account_signs_nobody_in() {
        let (_d, cfg) = unprovisioned();
        let res = do_login(&app_state(cfg), None, "any password at all").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_marks_the_cookie_http_only_secure_and_same_site_strict() {
        let (_d, cfg) = provisioned();
        let res = do_login(&app_state(cfg), None, PASSWORD).await;
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
        let first = app_state(Arc::clone(&cfg));
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

    #[tokio::test]
    async fn an_added_user_signs_in_with_their_own_password() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_eq!(
            add_user(&state, "alice", "alice-pass").await,
            StatusCode::OK
        );
        let res = do_login(&state, Some("alice"), "alice-pass").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(reaches(&state, &token_of(&res)).await, StatusCode::OK);
        let wrong = do_login(&state, Some("alice"), PASSWORD).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn adding_the_first_user_keeps_the_machine_password_as_admin() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let admin_token = signed_in_token(&state).await;
        assert_eq!(
            add_user(&state, "alice", "alice-pass").await,
            StatusCode::OK
        );
        assert_eq!(reaches(&state, &admin_token).await, StatusCode::OK);
        let admin = do_login(&state, None, PASSWORD).await;
        assert_eq!(admin.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn changing_a_password_signs_that_user_out_everywhere() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_eq!(
            add_user(&state, "alice", "first-pass").await,
            StatusCode::OK
        );
        let old = token_of(&do_login(&state, Some("alice"), "first-pass").await);
        let admin = signed_in_token(&state).await;
        assert_eq!(
            add_user(&state, "alice", "second-pass").await,
            StatusCode::OK
        );
        assert_eq!(reaches(&state, &old).await, StatusCode::UNAUTHORIZED);
        assert_eq!(reaches(&state, &admin).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_removed_user_is_signed_out_and_cannot_sign_in() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_eq!(
            add_user(&state, "alice", "alice-pass").await,
            StatusCode::OK
        );
        let token = token_of(&do_login(&state, Some("alice"), "alice-pass").await);
        assert_eq!(remove_user(&state, "alice").await, StatusCode::OK);
        assert_eq!(reaches(&state, &token).await, StatusCode::UNAUTHORIZED);
        let res = do_login(&state, Some("alice"), "alice-pass").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_last_user_cannot_be_removed() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_eq!(remove_user(&state, ADMIN).await, StatusCode::CONFLICT);
        assert_eq!(remove_user(&state, "nobody").await, StatusCode::NOT_FOUND);
        assert_eq!(
            add_user(&state, "alice", "alice-pass").await,
            StatusCode::OK
        );
        assert_eq!(remove_user(&state, ADMIN).await, StatusCode::OK);
        assert_eq!(remove_user(&state, "alice").await, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_bad_username_or_a_short_password_is_refused() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        assert_eq!(
            add_user(&state, "Alice", "alice-pass").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            add_user(&state, "a.b", "alice-pass").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            add_user(&state, "alice", "short").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn the_signed_in_user_is_known_to_the_handlers() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let token = signed_in_token(&state).await;
        let app = Router::new().route("/api/protected", get(me)).layer(
            axum::middleware::from_fn_with_state(state.auth.clone(), auth_middleware),
        );
        let res = app
            .oneshot(with_session(request("/api/protected", OFF_BOX), &token))
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["username"], ADMIN);
    }
}
