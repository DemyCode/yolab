use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

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

static LOADED: AtomicBool = AtomicBool::new(false);
static REVOKED_BEFORE_LOAD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

async fn load_sessions_from_k8s() -> Result<HashMap<String, i64>, crate::exec::CmdError> {
    let Some(data) = crate::kubectl::get_secret(SECRET_NAME, SECRET_NS).await? else {
        return Ok(HashMap::new());
    };
    let Some(json) = data.get("sessions") else {
        return Ok(HashMap::new());
    };
    let stored = serde_json::from_str::<HashMap<String, i64>>(json).map_err(|e| {
        crate::exec::CmdError::parse(format!("secret {SECRET_NS}/{SECRET_NAME}"), e)
    })?;
    Ok(live_only(stored, now_secs()))
}

fn live_only(sessions: HashMap<String, i64>, now: i64) -> HashMap<String, i64> {
    sessions.into_iter().filter(|(_, exp)| *exp > now).collect()
}

fn merge_loaded(live: &mut HashMap<String, i64>, stored: HashMap<String, i64>, revoked: &[String]) {
    for (token, exp) in stored {
        if revoked.contains(&token) {
            continue;
        }
        live.entry(token).or_insert(exp);
    }
}

async fn save_sessions_to_k8s(sessions: &HashMap<String, i64>) {
    if !LOADED.load(Ordering::SeqCst) {
        tracing::debug!("sessions not loaded from k8s yet — keeping this change in memory");
        return;
    }
    let json = match serde_json::to_string(sessions) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("could not serialize sessions: {e}");
            return;
        }
    };
    if let Err(e) =
        crate::kubectl::apply_secret(SECRET_NAME, SECRET_NS, &[("sessions", &json)], &[]).await
    {
        tracing::warn!("failed to persist sessions to k8s: {e}");
    }
}

fn note_revoked(token: &str) {
    if !LOADED.load(Ordering::SeqCst) {
        REVOKED_BEFORE_LOAD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(token.to_string());
    }
}

pub async fn init_sessions(sessions: &Sessions) {
    let sessions = sessions.clone();
    tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(2);
        loop {
            match load_sessions_from_k8s().await {
                Ok(stored) => {
                    let mut live = sessions.write().await;
                    let revoked = REVOKED_BEFORE_LOAD
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let had_live = !live.is_empty();
                    let n = stored.len();
                    merge_loaded(&mut live, stored, &revoked);
                    LOADED.store(true, Ordering::SeqCst);
                    if n > 0 {
                        tracing::info!("restored {n} session(s) from k8s secret");
                    }
                    if had_live || !revoked.is_empty() {
                        save_sessions_to_k8s(&live).await;
                    }
                    return;
                }
                Err(e) => {
                    tracing::debug!("sessions: could not load yet ({e}) — retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
    });
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

fn verify_password(password: &str, hash: &str) -> bool {
    if hash.is_empty() {
        return false;
    }
    pwhash::unix::verify(password, hash)
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
    if has_cluster_token(&req, &state.config) {
        return next.run(req).await;
    }
    let hash = password_hash(&state.config);
    if hash.is_empty() {
        if is_loopback(&req) {
            return next.run(req).await;
        }
        return (StatusCode::UNAUTHORIZED, r#"{"detail":"Unauthorized"}"#).into_response();
    }
    let token = jar
        .get("yolab_session")
        .map(|c| c.value().to_string())
        .unwrap_or_default();
    let valid = if token.is_empty() {
        false
    } else {
        let sessions = state.sessions.read().await;
        sessions
            .get(&token)
            .map(|&exp| exp > now_secs())
            .unwrap_or(false)
    };
    if !valid {
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
    let expiry = now_secs() + SESSION_DAYS * 86400;
    {
        let mut sessions = state.auth.sessions.write().await;
        sessions.insert(token.clone(), expiry);
        save_sessions_to_k8s(&sessions).await;
    }
    let cookie = Cookie::build(("yolab_session", token))
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
    let token = jar
        .get("yolab_session")
        .map(|c| c.value().to_string())
        .unwrap_or_default();
    if !token.is_empty() {
        let mut sessions = state.auth.sessions.write().await;
        sessions.remove(&token);
        note_revoked(&token);
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

    fn auth_state(config: Arc<Config>) -> AuthState {
        AuthState {
            sessions: new_sessions(),
            config,
        }
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
        with_header(req, "cookie", &format!("yolab_session={token}"))
    }

    async fn status_of(state: AuthState, req: Request<Body>) -> StatusCode {
        router(state).oneshot(req).await.unwrap().status()
    }

    const LOOPBACK: Option<&str> = Some("127.0.0.1:5000");
    const OFF_BOX: Option<&str> = Some("[fd00:42::5]:5000");

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
    fn verify_password_accepts_the_right_password() {
        assert!(verify_password(PASSWORD, HASH));
    }

    #[test]
    fn verify_password_rejects_the_wrong_password() {
        assert!(!verify_password("not-the-password", HASH));
        assert!(!verify_password("", HASH));
    }

    #[test]
    fn verify_password_never_succeeds_against_an_empty_hash() {
        assert!(!verify_password("anything", ""));
        assert!(!verify_password("", ""));
    }

    #[tokio::test]
    async fn loopback_is_allowed_when_no_password_is_configured() {
        let (_d, cfg) = unprovisioned();
        let s = auth_state(cfg);
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
            status_of(auth_state(cfg), request("/api/protected", OFF_BOX)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_request_with_no_connect_info_is_not_treated_as_loopback() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/protected", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_session_cookie_does_not_open_an_unprovisioned_node_from_off_box() {
        let (_d, cfg) = unprovisioned();
        let state = auth_state(cfg);
        state
            .sessions
            .write()
            .await
            .insert("valid-token".into(), now_secs() + 3600);
        assert_eq!(
            status_of(
                state,
                with_session(request("/api/protected", OFF_BOX), "valid-token")
            )
            .await,
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
        assert_eq!(status_of(auth_state(cfg), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_wrong_cluster_token_is_rejected() {
        let (_d, cfg) = provisioned();
        let req = with_header(
            request("/api/protected", OFF_BOX),
            CLUSTER_AUTH_HEADER,
            "wrong-tok",
        );
        assert_eq!(
            status_of(auth_state(cfg), req).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn an_empty_cluster_token_never_authorizes() {
        let (_d, cfg) = write_config("[homelab]\nhomelab_password_hash = \"x\"\n");
        let state = auth_state(cfg);
        for presented in ["", " "] {
            let req = with_header(
                request("/api/protected", OFF_BOX),
                CLUSTER_AUTH_HEADER,
                presented,
            );
            assert_eq!(
                status_of(state.clone(), req).await,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[tokio::test]
    async fn a_cluster_token_that_is_a_prefix_of_the_real_one_is_rejected() {
        let (_d, cfg) = provisioned();
        let req = with_header(
            request("/api/protected", OFF_BOX),
            CLUSTER_AUTH_HEADER,
            "cluster",
        );
        assert_eq!(
            status_of(auth_state(cfg), req).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_live_session_cookie_is_accepted() {
        let (_d, cfg) = provisioned();
        let state = auth_state(cfg);
        state
            .sessions
            .write()
            .await
            .insert("live-token".into(), now_secs() + 3600);
        assert_eq!(
            status_of(
                state,
                with_session(request("/api/protected", OFF_BOX), "live-token")
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_expired_session_cookie_is_rejected() {
        let (_d, cfg) = provisioned();
        let state = auth_state(cfg);
        state
            .sessions
            .write()
            .await
            .insert("stale-token".into(), now_secs() - 1);
        assert_eq!(
            status_of(
                state,
                with_session(request("/api/protected", OFF_BOX), "stale-token")
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn an_unknown_session_token_is_rejected() {
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(
                auth_state(cfg),
                with_session(request("/api/protected", OFF_BOX), "never-issued")
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_request_with_no_cookie_is_rejected_once_a_password_exists() {
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/protected", OFF_BOX)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn loopback_still_needs_a_session_once_a_password_exists() {
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/protected", LOOPBACK)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn an_empty_session_cookie_is_rejected() {
        let (_d, cfg) = provisioned();
        let state = auth_state(cfg);
        state
            .sessions
            .write()
            .await
            .insert(String::new(), now_secs() + 3600);
        assert_eq!(
            status_of(state, with_session(request("/api/protected", OFF_BOX), "")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn login_is_reachable_without_credentials() {
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/login", OFF_BOX)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn only_the_exact_login_path_is_exempt() {
        let (_d, cfg) = provisioned();
        let state = auth_state(cfg);
        for path in ["/api/login/", "/api/loginx", "/api/login/../protected"] {
            assert_eq!(
                status_of(state.clone(), request(path, OFF_BOX)).await,
                StatusCode::UNAUTHORIZED,
                "{path} must not inherit the /api/login exemption"
            );
        }
    }

    fn app_state(config: Arc<Config>) -> crate::AppState {
        crate::AppState {
            auth: auth_state(Arc::clone(&config)),
            config,
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

    #[tokio::test]
    async fn login_rejects_the_wrong_password() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let res = do_login(&state, "hunter2").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            issued_cookie(&res).is_none(),
            "a failed login must not set a cookie"
        );
        assert!(
            state.auth.sessions.read().await.is_empty(),
            "a failed login must not create a session"
        );
    }

    #[tokio::test]
    async fn login_accepts_the_right_password_and_issues_a_session() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        let res = do_login(&state, PASSWORD).await;
        assert_eq!(res.status(), StatusCode::OK);

        let sessions = state.auth.sessions.read().await;
        assert_eq!(
            sessions.len(),
            1,
            "exactly one session should have been created"
        );
        let (token, expiry) = sessions.iter().next().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(
            *expiry > now_secs(),
            "a freshly issued session must not be pre-expired"
        );
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
        do_login(&state, PASSWORD).await;
        do_login(&state, PASSWORD).await;
        assert_eq!(
            state.auth.sessions.read().await.len(),
            2,
            "session tokens must not collide between logins"
        );
    }

    #[tokio::test]
    async fn login_currently_mints_a_session_when_no_password_is_configured() {
        let (_d, cfg) = unprovisioned();
        let state = app_state(cfg);
        let res = do_login(&state, "any password at all").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(state.auth.sessions.read().await.len(), 1);
    }

    #[tokio::test]
    async fn logout_drops_the_session_and_clears_the_cookie() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        do_login(&state, PASSWORD).await;
        let token = state
            .auth
            .sessions
            .read()
            .await
            .keys()
            .next()
            .unwrap()
            .clone();

        let jar = CookieJar::new().add(Cookie::new("yolab_session", token.clone()));
        let res = logout(State(state.clone()), jar).await;

        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            !state.auth.sessions.read().await.contains_key(&token),
            "the token must be revoked server-side, not just cleared in the browser"
        );
        let cookie = issued_cookie(&res).expect("logout should clear the cookie");
        assert!(cookie.contains("Max-Age=0"), "got: {cookie}");
    }

    #[tokio::test]
    async fn logout_without_a_cookie_is_harmless() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        do_login(&state, PASSWORD).await;

        let res = logout(State(state.clone()), CookieJar::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.auth.sessions.read().await.len(),
            1,
            "a cookie-less logout must not revoke anyone else's session"
        );
    }

    #[tokio::test]
    async fn a_logged_out_token_no_longer_passes_the_middleware() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        do_login(&state, PASSWORD).await;
        let token = state
            .auth
            .sessions
            .read()
            .await
            .keys()
            .next()
            .unwrap()
            .clone();

        let req = with_session(request("/api/protected", OFF_BOX), &token);
        assert_eq!(status_of(state.auth.clone(), req).await, StatusCode::OK);

        let jar = CookieJar::new().add(Cookie::new("yolab_session", token.clone()));
        logout(State(state.clone()), jar).await;

        let req = with_session(request("/api/protected", OFF_BOX), &token);
        assert_eq!(
            status_of(state.auth.clone(), req).await,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[cfg(test)]
mod session_persistence_tests {
    use super::*;

    #[test]
    fn stored_sessions_merge_into_live_ones_without_resurrecting_logouts() {
        let mut live = HashMap::from([("new".to_string(), 200)]);
        let stored = HashMap::from([
            ("old".to_string(), 100),
            ("new".to_string(), 50),
            ("logged-out".to_string(), 100),
        ]);
        merge_loaded(&mut live, stored, &["logged-out".to_string()]);
        assert_eq!(live.get("old"), Some(&100));
        assert_eq!(live.get("new"), Some(&200));
        assert!(!live.contains_key("logged-out"));
    }

    #[test]
    fn expired_sessions_are_dropped_on_load() {
        let s = HashMap::from([("a".to_string(), 10), ("b".to_string(), 1000)]);
        let live = live_only(s, 500);
        assert!(!live.contains_key("a"));
        assert!(live.contains_key("b"));
    }
}
