//! One harness that stands up the REAL API, so a test drives what a browser
//! drives.
//!
//! The shape is borrowed from umbrelOS, whose `createTestUmbreld` and
//! `createTestVm` return the same object — one backed by an in-process daemon,
//! the other by a whole OS in QEMU — so a test body can be promoted from the
//! cheap tier to the expensive one without being rewritten. `TestApi` is the
//! cheap tier: the real `build_router`, the real auth middleware, the real cache
//! middleware, a real config file on disk, and no cluster. The expensive tier is
//! `nix/tests/*.nix`.
//!
//! Two rules make it worth having:
//!
//! 1. NEVER a stub router. A test that assembles its own `Router` proves that
//!    the handler works, not that it is reachable, not that it is behind auth,
//!    and not that it is mounted at the path the UI asks for. All three have
//!    been wrong in this repo.
//! 2. Requests are off-box by default. `is_loopback` is load-bearing for the
//!    unprovisioned case, so a harness that quietly sent everything from
//!    `127.0.0.1` would pass while the door stood open to the mesh. Ask for
//!    `from_loopback` by name when that is the case under test.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt as _;

use crate::auth::{new_sessions, AuthState};
use crate::config::Config;
use crate::AppState;

/// "password" hashed with SHA-512 crypt — the format `openssl passwd -6` emits,
/// and what the installer writes into config.toml.
pub(crate) const PASSWORD_HASH: &str = "$6$UG3IURKt1uqugrtk$i3e3tXg2NMIXuOb9JXztEAwCcsIcfn81WYBkzsfmwA7keyOajafp/PAAlFtcrMHVXo3cXK9z03YRRLaplZZm90";
pub(crate) const PASSWORD: &str = "password";
/// The pre-shared secret node→node calls present in `x-yolab-cluster`.
pub(crate) const CLUSTER_TOKEN: &str = "cluster-tok";

/// An address that is NOT loopback — a mesh peer, a pod, anything off this box.
const OFF_BOX: &str = "[fd00:cafe::9]:40000";
const LOOPBACK: &str = "127.0.0.1:40000";

/// One response, already read to the end.
pub(crate) struct Res {
    pub status: StatusCode,
    pub body: String,
    pub set_cookie: Option<String>,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    /// The middleware let this through to a handler. Deliberately not
    /// `status == OK`: most handlers need a cluster this harness does not have,
    /// so "was not rejected" is the honest assertion for a reachability test.
    pub fn reached_handler(&self) -> bool {
        self.status != StatusCode::UNAUTHORIZED
    }
}

pub(crate) struct TestApi {
    router: Router,
    /// Kept alive: the config file lives in it for as long as the API may read it.
    _dir: tempfile::TempDir,
    session: Option<String>,
    peer: &'static str,
    cluster_token: Option<String>,
}

impl TestApi {
    fn with_config(body: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).expect("write config");
        let config = Arc::new(Config::for_test(&path));
        let auth = AuthState {
            sessions: new_sessions(),
            config: Arc::clone(&config),
        };
        Self {
            router: crate::router::build_router(AppState { config, auth }),
            _dir: dir,
            session: None,
            peer: OFF_BOX,
            cluster_token: None,
        }
    }

    /// A provisioned node: a password is set, so auth is enforced exactly as it
    /// is in production. This is the default shape for anything security-facing.
    pub fn provisioned() -> Self {
        Self::with_config(&format!(
            "[homelab]\nhostname = \"yolab\"\nhomelab_password_hash = \"{PASSWORD_HASH}\"\n\
             [tunnel]\naccount_token = \"{CLUSTER_TOKEN}\"\n"
        ))
    }

    /// A node with no password yet — what a machine looks like between first
    /// boot and finishing setup.
    pub fn unprovisioned() -> Self {
        Self::with_config(&format!(
            "[homelab]\nhostname = \"yolab\"\n[tunnel]\naccount_token = \"{CLUSTER_TOKEN}\"\n"
        ))
    }

    /// Send as if from this machine itself — what Caddy's reverse proxy looks
    /// like, and the only caller an unprovisioned node trusts.
    pub fn from_loopback(mut self) -> Self {
        self.peer = LOOPBACK;
        self
    }

    /// Present the shared cluster token, as a peer node does.
    pub fn as_peer_node(mut self) -> Self {
        self.cluster_token = Some(CLUSTER_TOKEN.to_string());
        self
    }

    /// Present a cluster token that is wrong.
    pub fn with_cluster_token(mut self, token: &str) -> Self {
        self.cluster_token = Some(token.to_string());
        self
    }

    /// Sign in with the real password, keeping the session cookie for every
    /// later request — the same handshake the UI performs.
    pub async fn login(mut self) -> Self {
        let res = self
            .send(
                "POST",
                "/api/login",
                Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
            )
            .await;
        assert_eq!(res.status, StatusCode::OK, "login failed: {}", res.body);
        let cookie = res.set_cookie.expect("login set no cookie");
        let token = cookie
            .split(';')
            .next()
            .and_then(|kv| kv.split_once('='))
            .map(|(_, v)| v.to_string())
            .expect("no yolab_session in Set-Cookie");
        self.session = Some(token);
        self
    }

    pub async fn get(&self, uri: &str) -> Res {
        self.send("GET", uri, None).await
    }

    pub async fn post(&self, uri: &str, body: &str) -> Res {
        self.send("POST", uri, Some(body)).await
    }

    /// Drives the real router through `oneshot`, exactly as the server would.
    pub async fn send(&self, method: &str, uri: &str, body: Option<&str>) -> Res {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = &self.session {
            req = req.header(header::COOKIE, format!("yolab_session={token}"));
        }
        if let Some(token) = &self.cluster_token {
            req = req.header(crate::auth::CLUSTER_AUTH_HEADER, token);
        }
        let mut req = req
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .expect("build request");
        let addr: SocketAddr = self.peer.parse().expect("peer address");
        req.extensions_mut().insert(ConnectInfo(addr));

        let res = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router never fails");
        let status = res.status();
        let set_cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap_or_default();
        Res {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            set_cookie,
        }
    }
}
