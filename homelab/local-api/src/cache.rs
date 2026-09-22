use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub const HEADER_STATE: &str = "x-yolab-cache";
pub const HEADER_AGE_MS: &str = "x-yolab-cache-age-ms";
pub const HEADER_TTL_MS: &str = "x-yolab-cache-ttl-ms";
pub const HEADER_PROGRESSIVE: &str = "x-yolab-progressive";
pub const NDJSON: &str = "application/x-ndjson";

const MAX_CACHEABLE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub ttl: Duration,
    pub hard: Duration,
}

impl Policy {
    const fn secs(ttl: u64, hard: u64) -> Self {
        Policy {
            ttl: Duration::from_secs(ttl),
            hard: Duration::from_secs(hard),
        }
    }
}

pub(crate) fn policy_for(path: &str) -> Option<Policy> {
    if !path.starts_with("/api/") {
        return None;
    }
    if matches!(path, "/api/auth/check" | "/api/login" | "/api/logout") {
        return None;
    }
    if matches!(
        path,
        "/api/account/token"
            | "/api/backups/recovery-key"
            | "/api/ceph/dashboard"
            | "/api/cluster/ceph-join"
            | "/api/notifications"
    ) {
        return None;
    }
    if path == "/api/heal" || path.starts_with("/api/heal/") {
        return None;
    }
    if path == "/api/logs" || path == "/api/rebuild-log" || path.contains("/logs/") {
        return None;
    }

    Some(Policy::secs(15, 60))
}

struct Entry {
    body: Arc<Value>,
    fetched_at: Instant,
}

fn entries() -> &'static Mutex<HashMap<String, Entry>> {
    static E: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flight(key: &str) -> Arc<Mutex<()>> {
    static F: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = F.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard.entry(key.to_string()).or_default().clone()
}

async fn look_up(key: &str, policy: Policy) -> Option<(Arc<Value>, Duration)> {
    let mut map = entries().lock().await;
    let entry = map.get(key)?;
    let age = entry.fetched_at.elapsed();
    if age >= policy.hard {
        map.remove(key);
        return None;
    }
    Some((entry.body.clone(), age))
}

const MAX_ENTRIES: usize = 256;

async fn store(key: &str, body: Value) {
    let mut map = entries().lock().await;
    if map.len() >= MAX_ENTRIES && !map.contains_key(key) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, e)| e.fetched_at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        key.to_string(),
        Entry {
            body: Arc::new(body),
            fetched_at: Instant::now(),
        },
    );
}

pub async fn invalidate_all() {
    entries().lock().await.clear();
}

fn meta(state: &str, age: Duration, policy: Policy) -> (String, u128, u128) {
    (state.to_string(), age.as_millis(), policy.ttl.as_millis())
}

fn frame(body: &Value, state: &str, age: Duration, policy: Policy) -> String {
    let (state, age_ms, ttl_ms) = meta(state, age, policy);
    let envelope = serde_json::json!({
        "cache": state,
        "ageMs": age_ms,
        "ttlMs": ttl_ms,
        "data": body,
    });
    format!("{envelope}\n")
}

fn single(body: &Value, state: &str, age: Duration, policy: Policy) -> Response {
    let mut res = axum::Json(body).into_response();
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(state) {
        h.insert(HEADER_STATE, v);
    }
    let (_, age_ms, ttl_ms) = meta(state, age, policy);
    for (name, millis) in [(HEADER_AGE_MS, age_ms), (HEADER_TTL_MS, ttl_ms)] {
        if let Ok(v) = HeaderValue::from_str(&millis.to_string()) {
            h.insert(name, v);
        }
    }
    res
}

enum Handled {
    Cacheable(Value),
    PassThrough(Box<Response>),
}

async fn run_handler(req: Request, next: Next) -> Handled {
    let res = next.run(req).await;
    if res.status() != StatusCode::OK {
        return Handled::PassThrough(Box::new(res));
    }
    let is_json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !is_json {
        return Handled::PassThrough(Box::new(res));
    }
    let (parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_CACHEABLE_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return Handled::PassThrough(Box::new(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "response too large to cache",
                )
                    .into_response(),
            ))
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Handled::Cacheable(v),
        Err(_) => Handled::PassThrough(Box::new(Response::from_parts(parts, Body::from(bytes)))),
    }
}

pub async fn middleware(req: Request, next: Next) -> Response {
    if req.method() != Method::GET {
        let res = next.run(req).await;
        if res.status().is_success() {
            invalidate_all().await;
        }
        return res;
    }
    let Some(policy) = policy_for(req.uri().path()) else {
        return next.run(req).await;
    };
    let key = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let progressive = req
        .headers()
        .get(HEADER_PROGRESSIVE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let cached = look_up(&key, policy).await;

    if !progressive {
        if let Some((body, age)) = &cached {
            if *age < policy.ttl {
                return single(body, "hit", *age, policy);
            }
        }
    }

    if progressive {
        if let Some((body, age)) = cached {
            let label = if age < policy.ttl { "hit" } else { "stale" };
            let first = frame(&body, label, age, policy);
            let key = key.clone();
            let stream = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(first));
                match run_handler(req, next).await {
                    Handled::Cacheable(fresh) => {
                        store(&key, fresh.clone()).await;
                        yield Ok(axum::body::Bytes::from(frame(&fresh, "fresh", Duration::ZERO, policy)));
                    }
                    Handled::PassThrough(res) => {
                        let status = res.status().as_u16();
                        let envelope = serde_json::json!({
                            "cache": "error",
                            "status": status,
                        });
                        yield Ok(axum::body::Bytes::from(format!("{envelope}\n")));
                    }
                }
            };
            let mut res = Response::new(Body::from_stream(stream));
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON));
            res.headers_mut()
                .insert(HEADER_STATE, HeaderValue::from_static(label));
            return res;
        }
        let body = match compute_once(&key, policy, req, next).await {
            Computed::Fresh(body) => body,
            Computed::Ready(res) => return *res,
        };
        let mut res = Response::new(Body::from(frame(&body, "miss", Duration::ZERO, policy)));
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(NDJSON));
        res.headers_mut()
            .insert(HEADER_STATE, HeaderValue::from_static("miss"));
        return res;
    }

    match compute_once(&key, policy, req, next).await {
        Computed::Fresh(body) => single(&body, "miss", Duration::ZERO, policy),
        Computed::Ready(res) => *res,
    }
}

enum Computed {
    Fresh(Value),
    Ready(Box<Response>),
}

async fn compute_once(key: &str, policy: Policy, req: Request, next: Next) -> Computed {
    let flight = flight(key);
    let _guard = flight.lock().await;
    if let Some((body, age)) = look_up(key, policy).await {
        if age < policy.ttl {
            return Computed::Ready(Box::new(single(&body, "hit", age, policy)));
        }
    }
    match run_handler(req, next).await {
        Handled::Cacheable(body) => {
            store(key, body.clone()).await;
            Computed::Fresh(body)
        }
        Handled::PassThrough(res) => Computed::Ready(res),
    }
}

#[cfg(test)]
pub async fn clear_for_test() {
    entries().lock().await.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn test_lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    async fn begin() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = test_lock().lock().await;
        clear_for_test().await;
        guard
    }

    fn app(calls: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route(
                "/api/ceph/detail",
                get(move || {
                    let calls = calls.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        axum::Json(serde_json::json!({ "n": n }))
                    }
                }),
            )
            .layer(axum::middleware::from_fn(middleware))
    }

    fn req(progressive: bool) -> Request {
        let mut b = Request::builder().uri("/api/ceph/detail");
        if progressive {
            b = b.header(HEADER_PROGRESSIVE, "1");
        }
        b.body(Body::empty()).unwrap()
    }

    async fn body_string(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_client_still_gets_one_ordinary_json_body() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();
        assert_eq!(res.headers()[HEADER_STATE], "miss");
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "application/json",
            "a client that did not opt in must not be handed ndjson"
        );
        assert_eq!(body_string(res).await, r#"{"n":1}"#);

        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();
        assert_eq!(res.headers()[HEADER_STATE], "hit");
        assert_eq!(res.headers()[HEADER_AGE_MS], "0");
        assert_eq!(body_string(res).await, r#"{"n":1}"#);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the hit must not recompute"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_progressive_client_gets_the_cached_frame_then_the_real_one() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        tokio::time::advance(Duration::from_secs(16)).await;

        let res = app(calls.clone()).oneshot(req(true)).await.unwrap();
        assert_eq!(res.headers()[header::CONTENT_TYPE], NDJSON);

        let lines: Vec<Value> = body_string(res)
            .await
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "expected exactly two frames");

        assert_eq!(lines[0]["cache"], "stale");
        assert_eq!(
            lines[0]["data"]["n"], 1,
            "frame one is what we already knew"
        );
        assert_eq!(lines[0]["ageMs"], 16000, "and says exactly how old that is");

        assert_eq!(lines[1]["cache"], "fresh");
        assert_eq!(
            lines[1]["data"]["n"], 2,
            "frame two is what the handler said"
        );
        assert_eq!(lines[1]["ageMs"], 0);

        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();
        assert_eq!(body_string(res).await, r#"{"n":2}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_client_inside_the_ttl_skips_the_handler() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();

        assert_eq!(res.headers()[HEADER_STATE], "hit");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a plain client has no second frame to correct a stale body with, so \
             inside the TTL the cached one is the whole answer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_progressive_client_recomputes_even_when_the_cache_is_fresh() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        let res = app(calls.clone()).oneshot(req(true)).await.unwrap();
        assert_eq!(res.headers()[HEADER_STATE], "hit", "frame one is recent");

        let lines: Vec<Value> = body_string(res)
            .await
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(lines.len(), 2, "the real value must still follow");
        assert_eq!(lines[0]["cache"], "hit");
        assert_eq!(lines[0]["data"]["n"], 1);
        assert_eq!(lines[1]["cache"], "fresh");
        assert_eq!(lines[1]["data"]["n"], 2, "the handler ran anyway");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_past_the_hard_limit_is_never_served() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;

        let res = app(calls.clone()).oneshot(req(true)).await.unwrap();
        let lines: Vec<Value> = body_string(res)
            .await
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "there must be no stale frame to send");
        assert_eq!(lines[0]["cache"], "miss");
        assert_eq!(lines[0]["data"]["n"], 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_mutation_that_invalidates_is_not_masked_by_the_ttl() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        invalidate_all().await;

        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();
        assert_eq!(res.headers()[HEADER_STATE], "miss");
        assert_eq!(body_string(res).await, r#"{"n":2}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn a_denied_path_passes_straight_through() {
        let app = Router::new()
            .route(
                "/api/auth/check",
                get(|| async { axum::Json(serde_json::json!({"live": true})) }),
            )
            .layer(axum::middleware::from_fn(middleware));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/check")
                    .header(HEADER_PROGRESSIVE, "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            res.headers().get(HEADER_STATE).is_none(),
            "an uncached route must not look cached"
        );
        assert_eq!(body_string(res).await, r#"{"live":true}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_response_is_never_cached() {
        let _g = begin().await;
        let app = Router::new()
            .route(
                "/api/ceph/detail",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "ceph is unreachable") }),
            )
            .layer(axum::middleware::from_fn(middleware));

        let res = app.oneshot(req(false)).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(look_up("/api/ceph/detail", Policy::secs(15, 60))
            .await
            .is_none());
    }

    #[test]
    fn everything_is_cached_except_the_denylist() {
        for path in [
            "/api/ceph/detail",
            "/api/nodes/traffic",
            "/api/status",
            "/api/apps",
            "/api/apps/immich/pods",
            "/api/backups/snapshots",
        ] {
            assert!(policy_for(path).is_some(), "{path} should be cached");
        }

        assert!(policy_for("/api/auth/check").is_none());

        for path in [
            "/api/account/token",
            "/api/backups/recovery-key",
            "/api/ceph/dashboard",
            "/api/cluster/ceph-join",
            "/api/notifications",
        ] {
            assert!(policy_for(path).is_none(), "{path} is a secret");
        }

        assert!(policy_for("/api/heal").is_none());
        assert!(policy_for("/api/heal/peer").is_none());

        for path in [
            "/api/logs",
            "/api/rebuild-log",
            "/api/apps/immich/logs/immich-abc123",
        ] {
            assert!(policy_for(path).is_none(), "{path} is a log");
        }

        assert!(policy_for("/index.html").is_none());
        assert!(policy_for("/ceph-dashboard/").is_none());
    }

    #[test]
    fn the_ttl_is_shorter_than_the_ui_poll_interval() {
        for path in ["/api/ceph/detail", "/api/cluster/health", "/api/disks"] {
            let p = policy_for(path).unwrap();
            assert!(
                p.ttl < Duration::from_secs(20),
                "{path}: a TTL at or past the 20s poll means cycles that never refresh"
            );
            assert!(
                p.hard > p.ttl,
                "{path}: the hard limit must be past the TTL"
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn a_successful_write_on_an_unrelated_route_clears_the_cache() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = calls.clone();

        let app = Router::new()
            .route(
                "/api/ceph/detail",
                get(move || {
                    let reads = reads.clone();
                    async move {
                        let n = reads.fetch_add(1, Ordering::SeqCst) + 1;
                        axum::Json(serde_json::json!({ "n": n }))
                    }
                }),
            )
            .route(
                "/api/ceph/osd/3/mark-out",
                axum::routing::post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
            )
            .layer(axum::middleware::from_fn(middleware));

        app.clone().oneshot(req(false)).await.unwrap();
        let res = app.clone().oneshot(req(false)).await.unwrap();
        assert_eq!(
            res.headers()[HEADER_STATE],
            "hit",
            "cached before the write"
        );

        let write = Request::builder()
            .method(Method::POST)
            .uri("/api/ceph/osd/3/mark-out")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(write).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(req(false)).await.unwrap();
        assert_eq!(
            res.headers()[HEADER_STATE],
            "miss",
            "the write must not be masked by a cached read"
        );
        assert_eq!(body_string(res).await, r#"{"n":2}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_write_leaves_the_cache_alone() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = calls.clone();

        let app = Router::new()
            .route(
                "/api/ceph/detail",
                get(move || {
                    let reads = reads.clone();
                    async move {
                        let n = reads.fetch_add(1, Ordering::SeqCst) + 1;
                        axum::Json(serde_json::json!({ "n": n }))
                    }
                }),
            )
            .route(
                "/api/ceph/osd/3/mark-out",
                axum::routing::post(|| async {
                    (StatusCode::CONFLICT, "another drain is already running")
                }),
            )
            .layer(axum::middleware::from_fn(middleware));

        app.clone().oneshot(req(false)).await.unwrap();

        let write = Request::builder()
            .method(Method::POST)
            .uri("/api/ceph/osd/3/mark-out")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(write).await.unwrap();

        let res = app.oneshot(req(false)).await.unwrap();
        assert_eq!(res.headers()[HEADER_STATE], "hit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
