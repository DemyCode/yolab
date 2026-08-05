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
    use axum::{extract::ConnectInfo, routing::get, Router};
    use std::net::SocketAddr;
    use tower::ServiceExt as _;

    // "password" hashed with SHA-512 crypt, the format `openssl passwd -6` emits
    // and what the installer writes into config.toml.
    const HASH: &str = "$6$UG3IURKt1uqugrtk$i3e3tXg2NMIXuOb9JXztEAwCcsIcfn81WYBkzsfmwA7keyOajafp/PAAlFtcrMHVXo3cXK9z03YRRLaplZZm90";
    const PASSWORD: &str = "password";

    fn write_config(body: &str) -> (tempfile::TempDir, Arc<Config>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        let cfg = Arc::new(Config::for_test(&path));
        (dir, cfg)
    }

    /// A config with a password set — i.e. a provisioned node, the production shape.
    fn provisioned() -> (tempfile::TempDir, Arc<Config>) {
        write_config(&format!(
            "[homelab]\nhomelab_password_hash = \"{HASH}\"\n[tunnel]\naccount_token = \"cluster-tok\"\n"
        ))
    }

    /// A config with no password — a fresh or dev machine.
    fn unprovisioned() -> (tempfile::TempDir, Arc<Config>) {
        write_config("[homelab]\nhostname = \"yolab\"\n")
    }

    fn auth_state(config: Arc<Config>) -> AuthState {
        AuthState { sessions: new_sessions(), config }
    }

    /// Two protected routes plus /api/login, behind the real middleware.
    fn router(state: AuthState) -> Router {
        Router::new()
            .route("/api/protected", get(|| async { "reached" }))
            .route("/api/login", get(|| async { "reached" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    /// Builds a request as if it arrived from `peer`, which is what `is_loopback`
    /// keys off. `None` models the ConnectInfo extension being absent entirely.
    fn request(uri: &str, peer: Option<&str>) -> Request<Body> {
        let mut req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap();
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

    // ── ct_eq ─────────────────────────────────────────────────────────────────

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

    // ── password_hash / verify_password ───────────────────────────────────────

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

    /// Without this guard an unconfigured node would accept *any* password, since
    /// most crypt implementations treat an empty hash as a trivial match.
    #[test]
    fn verify_password_never_succeeds_against_an_empty_hash() {
        assert!(!verify_password("anything", ""));
        assert!(!verify_password("", ""));
    }

    // ── Middleware: no password configured ────────────────────────────────────

    #[tokio::test]
    async fn loopback_is_allowed_when_no_password_is_configured() {
        // Caddy proxies public traffic from ::1, and dev hits localhost directly.
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

    /// The fail-closed branch. An unprovisioned node is reachable over the mesh
    /// before anyone has set a password; if this returned 200 the whole API —
    /// including /api/terminal/exec — would be open to every peer.
    #[tokio::test]
    async fn off_box_callers_are_rejected_when_no_password_is_configured() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/protected", OFF_BOX)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// `is_loopback` reads ConnectInfo out of the request extensions. If the
    /// server is ever built without `into_make_service_with_connect_info`, that
    /// extension is missing — and "unknown origin" has to read as "not local".
    #[tokio::test]
    async fn a_request_with_no_connect_info_is_not_treated_as_loopback() {
        let (_d, cfg) = unprovisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/protected", None)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// A session cookie is not a substitute for a password: with no hash set, the
    /// only door is loopback.
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
            status_of(state, with_session(request("/api/protected", OFF_BOX), "valid-token")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // ── Middleware: the cluster token (node → node) ───────────────────────────

    /// Node→node calls authenticate with the shared token, not with a source
    /// address. The predecessor trusted anything in fc00::/7 — which includes pod
    /// IPs, so any pod in the cluster could call privileged endpoints unauthed.
    #[tokio::test]
    async fn the_cluster_token_authorizes_a_call_from_another_node() {
        let (_d, cfg) = provisioned();
        let req = with_header(request("/api/protected", OFF_BOX), CLUSTER_AUTH_HEADER, "cluster-tok");
        assert_eq!(status_of(auth_state(cfg), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_wrong_cluster_token_is_rejected() {
        let (_d, cfg) = provisioned();
        let req = with_header(request("/api/protected", OFF_BOX), CLUSTER_AUTH_HEADER, "wrong-tok");
        assert_eq!(status_of(auth_state(cfg), req).await, StatusCode::UNAUTHORIZED);
    }

    /// `cluster_token()` returns "" when config.toml is missing or malformed. A
    /// caller presenting an empty header must not match that — otherwise losing
    /// the config file would turn into "everyone is a trusted node".
    #[tokio::test]
    async fn an_empty_cluster_token_never_authorizes() {
        let (_d, cfg) = write_config("[homelab]\nhomelab_password_hash = \"x\"\n");
        let state = auth_state(cfg);
        for presented in ["", " "] {
            let req = with_header(request("/api/protected", OFF_BOX), CLUSTER_AUTH_HEADER, presented);
            assert_eq!(status_of(state.clone(), req).await, StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn a_cluster_token_that_is_a_prefix_of_the_real_one_is_rejected() {
        let (_d, cfg) = provisioned();
        let req = with_header(request("/api/protected", OFF_BOX), CLUSTER_AUTH_HEADER, "cluster");
        assert_eq!(status_of(auth_state(cfg), req).await, StatusCode::UNAUTHORIZED);
    }

    // ── Middleware: session cookies ───────────────────────────────────────────

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
            status_of(state, with_session(request("/api/protected", OFF_BOX), "live-token")).await,
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
            status_of(state, with_session(request("/api/protected", OFF_BOX), "stale-token")).await,
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

    /// Loopback is *not* a bypass once a password is set: Caddy proxies the whole
    /// public internet from ::1, so trusting it there would publish the API.
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
        // An empty token must not match an empty-keyed map entry, if one existed.
        state.sessions.write().await.insert(String::new(), now_secs() + 3600);
        assert_eq!(
            status_of(state, with_session(request("/api/protected", OFF_BOX), "")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // ── Middleware: the /api/login exemption ──────────────────────────────────

    #[tokio::test]
    async fn login_is_reachable_without_credentials() {
        // Otherwise there would be no way to obtain the cookie the rest needs.
        let (_d, cfg) = provisioned();
        assert_eq!(
            status_of(auth_state(cfg), request("/api/login", OFF_BOX)).await,
            StatusCode::OK
        );
    }

    /// The exemption is an exact path match, so nothing else inherits it.
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

    // ── login / logout handlers ───────────────────────────────────────────────

    fn app_state(config: Arc<Config>) -> crate::AppState {
        crate::AppState { auth: auth_state(Arc::clone(&config)), config }
    }

    async fn do_login(state: &crate::AppState, password: &str) -> Response {
        login(
            State(state.clone()),
            axum::Json(LoginRequest { password: password.to_string() }),
        )
        .await
    }

    /// The Set-Cookie value, if the response issued one.
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
        assert!(issued_cookie(&res).is_none(), "a failed login must not set a cookie");
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
        assert_eq!(sessions.len(), 1, "exactly one session should have been created");
        let (token, expiry) = sessions.iter().next().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(*expiry > now_secs(), "a freshly issued session must not be pre-expired");
    }

    #[tokio::test]
    async fn login_marks_the_cookie_http_only_and_same_site_strict() {
        // The session cookie is a bearer credential for the whole API; it must be
        // unreadable from JS and unattached to cross-site requests.
        let (_d, cfg) = provisioned();
        let res = do_login(&app_state(cfg), PASSWORD).await;
        let cookie = issued_cookie(&res).expect("login should set a cookie");
        assert!(cookie.contains("HttpOnly"), "got: {cookie}");
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

    /// KNOWN GAP, pinned deliberately so a fix has to come with a decision.
    ///
    /// `login` only rejects when a hash *exists* and does not match, so on a node
    /// with no password configured any request to /api/login mints a valid 30-day
    /// session — and /api/login is itself exempt from the middleware, so this is
    /// reachable from off-box. The session stays in the map and becomes usable the
    /// moment a password is later set.
    ///
    /// The middleware fails closed for off-box callers without a password, so the
    /// hole is only exploitable in the window before provisioning finishes. Left
    /// as-is for now at the owner's direction; when it is closed, this test will
    /// fail and should be rewritten to assert 401.
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
        let token = state.auth.sessions.read().await.keys().next().unwrap().clone();

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

    /// A revoked token must stop working immediately, not merely stop being sent.
    #[tokio::test]
    async fn a_logged_out_token_no_longer_passes_the_middleware() {
        let (_d, cfg) = provisioned();
        let state = app_state(cfg);
        do_login(&state, PASSWORD).await;
        let token = state.auth.sessions.read().await.keys().next().unwrap().clone();

        let req = with_session(request("/api/protected", OFF_BOX), &token);
        assert_eq!(status_of(state.auth.clone(), req).await, StatusCode::OK);

        let jar = CookieJar::new().add(Cookie::new("yolab_session", token.clone()));
        logout(State(state.clone()), jar).await;

        let req = with_session(request("/api/protected", OFF_BOX), &token);
        assert_eq!(status_of(state.auth.clone(), req).await, StatusCode::UNAUTHORIZED);
    }
}
