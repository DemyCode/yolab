//! A caching middleware that answers twice: what we already knew, then the truth.
//!
//! WHY THIS EXISTS. Measured on a live, healthy three-node cluster:
//!
//!   /api/status              22ms
//!   /api/nodes              193ms
//!   /api/disks              309ms
//!   /api/backups/status     505ms
//!   /api/ceph/status        931ms
//!   /api/cluster/health    2016ms
//!   /api/ceph/detail       5476ms
//!
//! None of that is query cost. It is `ceph`, `kubectl` and `systemctl`
//! processes being spawned and waited on. StoragePage polls `/api/ceph/detail`
//! every 20s, so it sat blocked on one for better than a quarter of every
//! window, and each open tab paid separately because nothing sat between them
//! and the shell. On a DEGRADED cluster these get far worse — `rbd ls` against a
//! pool with down placement groups does not return at all.
//!
//! TWO ANSWERS TO ONE REQUEST. A client that opts in gets an `application/
//! x-ndjson` stream instead of a single JSON body: one frame with the value we
//! already had, sent immediately, then a second frame with the value the handler
//! actually produced. The page paints from the first frame in milliseconds and
//! corrects itself when the second lands, on one connection, with no second
//! request and no polling trick.
//!
//! That is also what lets this file stay small. An ordinary
//! stale-while-revalidate cache needs a background task, a re-callable handler
//! and a flag to stop refreshes piling up. Here the second frame IS the refresh,
//! so none of that exists: the work happens on the request that wanted it, in
//! the order the client can use it.
//!
//! OPT-IN, because the response shape changes. Only a client sending
//! `x-yolab-progressive: 1` gets ndjson. Everything else — curl, tests, the
//! tunnel, any future integration — gets exactly the single JSON body it always
//! got, served from cache when that is fresh enough and computed when it is not.
//!
//! A CACHED VALUE IS NEVER PRESENTED AS A LIVE ONE.
//!
//! `useResource` in the client used to render last-known values from
//! localStorage, and that was deliberately removed for a reason worth repeating:
//! during an incident a confidently-rendered stale number — "1.2 TB free" from
//! before a disk failed, a disk still shown "in use" after it was pulled — is
//! worse than a spinner. This cache is only safe because it never repeats that.
//! Every single-body response carries
//!
//!   x-yolab-cache         hit | miss
//!   x-yolab-cache-age-ms  how old the body is, 0 on a miss
//!   x-yolab-cache-ttl-ms  how old it was allowed to get
//!
//! and every ndjson frame carries the same three fields inline, because headers
//! cannot change halfway through a response. Those are load-bearing, not
//! diagnostics: if the UI stops surfacing them, this file has reintroduced the
//! bug that one was deleted for.
//!
//! ALLOWLISTED, never "cache every GET". A blanket rule would eventually cache
//! something per-user, something enormous, or something whose freshness is the
//! entire point. Only the paths in `POLICIES` are cached, each with the TTL its
//! own cost and volatility justify.

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
// tokio's Instant, not std's, so `#[tokio::test(start_paused = true)]` can drive
// expiry without any test sleeping for real.
use tokio::time::Instant;

pub const HEADER_STATE: &str = "x-yolab-cache";
pub const HEADER_AGE_MS: &str = "x-yolab-cache-age-ms";
pub const HEADER_TTL_MS: &str = "x-yolab-cache-ttl-ms";
/// What a client sends to ask for the two-frame form.
pub const HEADER_PROGRESSIVE: &str = "x-yolab-progressive";
pub const NDJSON: &str = "application/x-ndjson";

/// Cap on a handler response we are willing to buffer in order to cache it.
/// Anything larger is streamed through untouched rather than held in memory.
const MAX_CACHEABLE_BYTES: usize = 4 * 1024 * 1024;

/// How long a value may be served as fresh, and the hard limit past which it is
/// not served at all.
///
/// The second number is the one that matters during an incident. However
/// honestly a body is labelled, an operator watching a cluster come apart must
/// not be handed a minute-old picture of it, so past `hard` the entry is dropped
/// and the caller waits for the truth.
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

/// The cached routes, with the TTL each one's cost and volatility justify.
///
/// Deliberately a short, explicit list. These are the measured-expensive reads
/// the UI polls; everything else goes straight through. Paths are matched
/// exactly — no prefixes — so adding a route here is always a decision someone
/// made on purpose.
///
/// 15s against the UI's 20s poll is chosen so a poll normally finds the entry
/// just past its TTL: the page paints instantly from frame one and still gets a
/// freshly computed frame two every cycle. A TTL longer than the poll would mean
/// whole cycles that never recompute at all.
fn policy_for(path: &str) -> Option<Policy> {
    const POLICIES: &[(&str, Policy)] = &[
        ("/api/ceph/detail", Policy::secs(15, 60)),
        ("/api/cluster/health", Policy::secs(15, 60)),
        ("/api/ceph/status", Policy::secs(15, 60)),
        ("/api/disks", Policy::secs(15, 60)),
        ("/api/backups/status", Policy::secs(15, 60)),
        ("/api/nodes", Policy::secs(15, 60)),
        ("/api/nodes/links", Policy::secs(15, 60)),
    ];
    POLICIES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, policy)| *policy)
}

struct Entry {
    body: Arc<Value>,
    fetched_at: Instant,
}

fn entries() -> &'static Mutex<HashMap<String, Entry>> {
    static E: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One lock per key, held across a handler run so concurrent misses collapse
/// into a single subprocess.
///
/// A cold cache with three tabs open must not become three `ceph` invocations on
/// a box that may already be the reason they are slow. Kept in its own map, not
/// inside `entries`, because the entry map must never stay locked while a
/// handler runs — that would serialise every endpoint behind the slowest one.
fn flight(key: &str) -> Arc<Mutex<()>> {
    static F: OnceLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = F.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard.entry(key.to_string()).or_default().clone()
}

/// A cached body and its age, or `None` when there is nothing usable.
async fn look_up(key: &str, policy: Policy) -> Option<(Arc<Value>, Duration)> {
    let mut map = entries().lock().await;
    let entry = map.get(key)?;
    let age = entry.fetched_at.elapsed();
    if age >= policy.hard {
        // Dropped rather than left to be overwritten, so a handler that then
        // fails cannot resurrect it.
        map.remove(key);
        return None;
    }
    Some((entry.body.clone(), age))
}

async fn store(key: &str, body: Value) {
    entries().lock().await.insert(
        key.to_string(),
        Entry {
            body: Arc::new(body),
            fetched_at: Instant::now(),
        },
    );
}

/// Drop everything the cache holds.
///
/// EVERY MUTATION MUST DO THIS, which is why `middleware` calls it for any
/// successful non-GET rather than leaving it to each handler. A cache that
/// outlives the action that invalidated it is worse than no cache: the operator
/// marks an OSD out, the page keeps showing it in for the rest of the TTL, and
/// the only reasonable conclusion is that the button did not work. The risk is
/// not staleness in the abstract, it is contradicting something the user just
/// did.
///
/// There is deliberately no per-path variant. One existed, was used only by a
/// test, and `-D warnings` correctly called it dead code — see the note on the
/// blast radius in `middleware` for why nothing needs it.
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
    // One JSON object per line is the whole contract; a newline inside a frame
    // would split it in two for the reader.
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

/// What came back from the handler: something worth caching, or something to
/// send on untouched.
///
/// AN ENUM RATHER THAN `Result<Value, Response>`, which is what this was. The
/// `Err` side never meant an error — a 204, a stream, an ordinary 500 are all
/// perfectly good answers that simply cannot be cached — so `Result` was telling
/// the reader the wrong thing about every one of them. clippy objected for its
/// own reason (`result_large_err`: an axum Response is 128 bytes, and paying
/// that on every success path is waste), and both complaints have the same fix.
enum Handled {
    /// A plain 200 JSON body, small enough to hold: cacheable.
    Cacheable(Value),
    /// Anything else. Passed through exactly as the handler produced it — a
    /// cache that stored error bodies would turn a one-second blip into a
    /// TTL-long lie.
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
        // Too large to buffer, and the body is gone with it — there is nothing
        // left to pass through, so say so rather than return an empty 200.
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
    // ANY SUCCESSFUL WRITE DROPS THE WHOLE CACHE, and it happens here rather than
    // in each mutation handler on purpose.
    //
    // Per-handler invalidation is a rule someone has to remember every time they
    // add a route, and the one they forget is the one that makes a button look
    // broken: the operator marks an OSD out, the page goes on showing it in for
    // the rest of the TTL, and the only reasonable conclusion is that the click
    // did nothing. The middleware already sees every request, so it can enforce
    // this for routes that do not know the cache exists.
    //
    // Everything, not just the route that was written to: marking an OSD out
    // changes /api/ceph/detail, /api/cluster/health and /api/disks at once. The
    // cached set is seven small entries, so dropping all of them costs one
    // recompute of whatever is actually on screen, and needs no map from write
    // routes to the reads they affect — a map that would itself go stale.
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
    // Path and query together: two different queries are two different answers,
    // and keying on the path alone would serve one for the other.
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

    // Fresh and nobody is waiting on anything better: this is the case that
    // actually removes load, and it is why several tabs inside one TTL cost one
    // subprocess between them rather than one each.
    if let Some((body, age)) = &cached {
        if *age < policy.ttl {
            return single(body, "hit", *age, policy);
        }
    }

    if progressive {
        if let Some((body, age)) = cached {
            // The two-frame answer. Frame one goes out before the handler is even
            // started, so the page paints from it while the box is still shelling
            // out for frame two.
            let first = frame(&body, "stale", age, policy);
            let key = key.clone();
            let stream = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(first));
                match run_handler(req, next).await {
                    Handled::Cacheable(fresh) => {
                        store(&key, fresh.clone()).await;
                        yield Ok(axum::body::Bytes::from(frame(&fresh, "fresh", Duration::ZERO, policy)));
                    }
                    Handled::PassThrough(res) => {
                        // The handler failed AFTER we already promised a 200 and
                        // sent a frame. The status line is long gone, so the only
                        // honest thing left is to say so in a frame the client
                        // can recognise, and let it keep showing frame one.
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
                .insert(HEADER_STATE, HeaderValue::from_static("stale"));
            return res;
        }
        // Nothing cached at all. There is no first frame to send, so this is an
        // ordinary miss — still ndjson, so the client has one shape to parse.
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

    // Plain client, nothing fresh: compute and answer once, as always.
    match compute_once(&key, policy, req, next).await {
        Computed::Fresh(body) => single(&body, "miss", Duration::ZERO, policy),
        Computed::Ready(res) => *res,
    }
}

/// The outcome of trying to produce a value for this key.
///
/// Like `Handled`, an enum rather than a `Result` whose `Err` is not an error:
/// both "another request filled the cache while we queued" and "the handler
/// returned something uncacheable" end the same way — there is a finished
/// Response, send it. Only `Fresh` leaves the caller anything to decide, which
/// is how it should frame the body it just computed.
enum Computed {
    Fresh(Value),
    Ready(Box<Response>),
}

/// Compute under the key's flight lock, so simultaneous misses become one run.
async fn compute_once(key: &str, policy: Policy, req: Request, next: Next) -> Computed {
    let flight = flight(key);
    let _guard = flight.lock().await;
    // Whoever held this lock before us has just filled the cache. Use their
    // result rather than spawning an identical subprocess a millisecond later.
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

    /// The cache is process-global and keyed by path, so tests that use the same
    /// real path would stomp each other when the harness runs them in parallel —
    /// which it does. Every test takes this first and starts from an empty cache.
    fn test_lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    async fn begin() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = test_lock().lock().await;
        clear_for_test().await;
        guard
    }

    /// A router whose handler counts its calls, mounted at a REAL cached path —
    /// the middleware keys off the path, so a made-up one would simply pass
    /// through and the test would assert nothing.
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

    /// THE POINT OF THE WHOLE FILE: one request, two answers.
    #[tokio::test(start_paused = true)]
    async fn a_progressive_client_gets_the_cached_frame_then_the_real_one() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        // Prime the cache, then age it past the TTL so there is something to send
        // ahead of the handler.
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

        // And frame two is what a later reader gets.
        let res = app(calls.clone()).oneshot(req(false)).await.unwrap();
        assert_eq!(body_string(res).await, r#"{"n":2}"#);
    }

    /// Inside the TTL there is nothing better to offer, so the handler is never
    /// run — this is the case that actually removes load from the box when
    /// several tabs are open.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_value_is_served_without_running_the_handler() {
        let _g = begin().await;
        let calls = Arc::new(AtomicUsize::new(0));

        app(calls.clone()).oneshot(req(false)).await.unwrap();
        let res = app(calls.clone()).oneshot(req(true)).await.unwrap();

        assert_eq!(res.headers()[HEADER_STATE], "hit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Past the hard limit the old body is not sent at all, in either mode. An
    /// operator watching a cluster come apart must not be handed a minute-old
    /// picture of it, however clearly it is labelled.
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

    /// An uncached path must be untouched — same body, and none of the cache
    /// headers, so nothing downstream can mistake it for a cached answer.
    #[tokio::test(start_paused = true)]
    async fn an_unlisted_path_passes_straight_through() {
        let app = Router::new()
            .route(
                "/api/status",
                get(|| async { axum::Json(serde_json::json!({"live": true})) }),
            )
            .layer(axum::middleware::from_fn(middleware));

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
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

    /// A failing handler must not be stored — a one-second blip would otherwise
    /// become a TTL-long lie.
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
    fn only_the_listed_paths_are_cached() {
        assert!(policy_for("/api/ceph/detail").is_some());
        assert!(policy_for("/api/status").is_none());
        // Exact matches only: a prefix rule would quietly catch future routes
        // nobody decided to cache.
        assert!(policy_for("/api/ceph/detail/extra").is_none());
    }

    /// The TTL has to be shorter than the UI's poll, or whole cycles go by that
    /// never recompute and the second frame stops being worth sending.
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
    /// A write must clear the cache even though the route it hit knows nothing
    /// about the cache. This is the property that keeps a button press from being
    /// contradicted by a cached read for the rest of the TTL.
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
            // A different path entirely, exactly as a real mutation would be.
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

    /// A write that FAILED changed nothing, so throwing the cache away would just
    /// hand the box a pile of recomputes for no reason.
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
