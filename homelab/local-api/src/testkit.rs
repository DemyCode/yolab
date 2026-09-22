
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

pub(crate) const PASSWORD_HASH: &str = "$6$UG3IURKt1uqugrtk$i3e3tXg2NMIXuOb9JXztEAwCcsIcfn81WYBkzsfmwA7keyOajafp/PAAlFtcrMHVXo3cXK9z03YRRLaplZZm90";
pub(crate) const PASSWORD: &str = "password";
pub(crate) const CLUSTER_TOKEN: &str = "cluster-tok";

const OFF_BOX: &str = "[fd00:cafe::9]:40000";
const LOOPBACK: &str = "127.0.0.1:40000";

pub(crate) struct Res {
    pub status: StatusCode,
    pub body: String,
    pub set_cookie: Option<String>,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    pub fn reached_handler(&self) -> bool {
        self.status != StatusCode::UNAUTHORIZED
    }
}

pub(crate) struct TestApi {
    router: Router,
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

    pub fn provisioned() -> Self {
        Self::with_config(&format!(
            "[homelab]\nhostname = \"yolab\"\nhomelab_password_hash = \"{PASSWORD_HASH}\"\n\
             [tunnel]\naccount_token = \"{CLUSTER_TOKEN}\"\n"
        ))
    }

    pub fn unprovisioned() -> Self {
        Self::with_config(&format!(
            "[homelab]\nhostname = \"yolab\"\n[tunnel]\naccount_token = \"{CLUSTER_TOKEN}\"\n"
        ))
    }

    pub fn over_loopback(mut self) -> Self {
        self.peer = LOOPBACK;
        self
    }

    pub fn with_peer_token(mut self) -> Self {
        self.cluster_token = Some(CLUSTER_TOKEN.to_string());
        self
    }

    pub fn with_cluster_token(mut self, token: &str) -> Self {
        self.cluster_token = Some(token.to_string());
        self
    }

    pub async fn login(mut self) -> Self {
        let body = format!("{{\"password\":\"{PASSWORD}\"}}");
        let res = self.send("POST", "/api/login", Some(&body)).await;
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

    pub async fn send(&self, method: &str, uri: &str, body: Option<&str>) -> Res {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = &self.session {
            req = req.header(header::COOKIE, format!("yolab_session={token}"));
        }
        if let Some(token) = &self.cluster_token {
            req = req.header(crate::auth::CLUSTER_AUTH_HEADER, token.as_str());
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
