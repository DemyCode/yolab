//! Direct node-to-node paths, with the relay as the fallback.
//!
//! Every byte between nodes — Ceph replication, etcd raft, mon paxos — goes
//! node → external WireGuard server → node, because wg1's only peer is that
//! server, holding `allowedIPs = fd00:cafe::/112`. Two laptops on the same sofa
//! pay metered relay rates to reach each other.
//!
//! THE MECHANISM IS WIREGUARD'S LONGEST-PREFIX MATCH. Adding a second peer for
//! one node with `allowed-ips fd00:cafe::6/128` beats the hub's /112, so traffic
//! to that node goes direct — no routes, no policy rules, nothing else touched.
//! Removing it falls straight back to the relay on the next packet. Promotion
//! and demotion are therefore single atomic operations, which is what makes this
//! safe enough to do automatically.
//!
//! ## The relay bootstraps its own replacement
//!
//! Discovery needs no new infrastructure: the nodes can already reach each other
//! over the tunnel, so they simply ask each other where they can be found
//! directly. No mDNS, no broadcast, no coordination server — which also means
//! none of the home-router hostility around multicast applies.
//!
//! ## Why a probe address
//!
//! A candidate endpoint cannot be tested by installing it as the real /128:
//! that diverts production traffic onto an unproven path. But WireGuard keys
//! handshakes by PUBLIC KEY, not by allowed-ips — so the peer is added with a
//! junk allowed-ips first, and only a completed handshake promotes it to the
//! real address. An unreachable candidate costs nothing.
//!
//! ## The failure mode this exists to prevent
//!
//! A promoted peer whose endpoint stops answering still wins longest-prefix
//! match, so packets go into a hole instead of falling back — worse than never
//! having tried. A laptop moving from home Wi-Fi to a hotspot does exactly this.
//! The liveness check below is not a refinement; it is the thing that makes the
//! feature safe.

pub mod candidates;
pub mod wg;

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{
    auth::CLUSTER_AUTH_HEADER,
    error::Result,
    host::{Host, RealHost},
    kubectl, AppState,
};

pub use candidates::Candidates;

/// How stale a handshake may be before a direct peer is demoted.
///
/// WireGuard rehandshakes about every 2 minutes under traffic, and persistent
/// keepalive is 25s, so 180s is several missed opportunities rather than one
/// unlucky moment — demoting on a single blip would flap the path under Ceph.
const HANDSHAKE_MAX_AGE_SECS: u64 = 180;

/// How long to wait for a probe handshake before calling a candidate dead.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Gap between probe attempts for a peer that is currently relayed.
///
/// Liveness still runs every tick; only the (slower, speculative) probing backs
/// off. Without this, a two-node cluster in two different houses would spend
/// eight seconds per candidate per minute, forever, discovering nothing.
const PROBE_RETRY: Duration = Duration::from_secs(300);

const TICK: Duration = Duration::from_secs(60);

/// How often the purely-local reconcile runs.
///
/// This is the width of the window in which a pair can be half-promoted, and a
/// half-promoted pair passes NO traffic at all (see `reconcile_local`). At 60s
/// that window took etcd and Ceph down with it; at 5s it is a blip, and the
/// check costs one `wg show`.
const FAST_TICK: Duration = Duration::from_secs(5);

/// Marks the hub peer, which owns the entire cluster subnet.
const SUBNET_SUFFIX: &str = "/112";

/// Carries the caller's OWN wg1 public key on a mesh-candidates request.
///
/// Without this, direct paths never come up at all — not flakily, but every
/// single time. WireGuard silently drops a handshake initiation from a public
/// key it has not been told about; each side's probe only ever configures its
/// OWN outgoing peer entry, so both sides sit sending an initiation the other
/// is unconditionally dropping. Found by hand: added node2 as a peer on node1
/// exactly as the probe code does, watched `wg show` for a handshake, and after
/// several seconds there was none — while node2 had never heard of node1 at
/// all. This header is what lets the RESPONDER recognise the caller before the
/// caller's packet arrives, so mesh_candidates below doubles as the priming
/// step: asking "what are your candidates" is also how you introduce yourself.
const MESH_PUBKEY_HEADER: &str = "x-yolab-mesh-pubkey";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── What this node offers peers ───────────────────────────────────────────────

/// `GET /api/cluster/mesh-candidates` — cluster-authed, node→node.
///
/// Doubles as the introduction step: asking "what are your candidates" is also
/// how a caller who supplies `MESH_PUBKEY_HEADER` gets primed into this node's
/// peer table, so the handshake it is about to attempt is not silently dropped.
pub async fn mesh_candidates(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Candidates>> {
    let host = &RealHost;
    if let Some(caller_key) = headers
        .get(MESH_PUBKEY_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        prime_caller(host, caller_key, addr.ip()).await;
    }
    Ok(Json(Candidates {
        public_key: wg::self_public_key(host).await?,
        listen_port: wg::listen_port(host).await?,
        addresses: candidates::local_addresses().await?,
    }))
}

/// If this public key has never been seen before, configures it as a
/// listen-only peer — no endpoint, junk allowed-ips — purely so a handshake
/// attempt FROM it is accepted rather than dropped.
///
/// Does nothing once ANY peer entry exists for this key, junk or promoted.
/// mesh_candidates is called on every tick regardless of promotion state (see
/// `tick` below), and overwriting an already-promoted peer's real allowed-ips
/// with a junk one on every such call would silently break a working direct
/// path once a minute, forever — priming is only for the very first
/// introduction, after which each side's own tick loop owns that peer entry.
async fn prime_caller<H: Host>(host: &H, caller_public_key: &str, caller_addr: std::net::IpAddr) {
    let already_known = wg::peers(host)
        .await
        .unwrap_or_default()
        .iter()
        .any(|p| p.public_key == caller_public_key);
    if already_known {
        return;
    }
    let Some(probe_addr) = probe_address(&caller_addr.to_string()) else {
        return;
    };
    if let Err(e) = wg::set_peer(host, caller_public_key, None, &probe_addr, 25).await {
        tracing::debug!("mesh: could not prime caller {caller_addr}: {e:#}");
    }
}

// ── What this node reports about itself ───────────────────────────────────────

#[derive(Serialize)]
pub struct PathStatus {
    /// The peer's cluster address, e.g. fd00:cafe::6.
    pub node: String,
    /// "direct" or "relayed".
    pub path: String,
    /// Set only when direct: the endpoint actually in use.
    pub endpoint: Option<String>,
    pub handshake_age_secs: Option<u64>,
}

/// `GET /api/mesh/paths` — is this actually saving anything, or do we merely
/// believe it is? The whole feature exists to cut a bill, so the answer has to
/// be observable rather than assumed.
pub async fn paths(State(state): State<AppState>) -> Result<Json<Vec<PathStatus>>> {
    let host = &RealHost;
    let peers = wg::peers(host).await.unwrap_or_default();
    let addrs = peer_addresses(&state.config.node_ipv6).await;
    Ok(Json(path_statuses(&peers, &addrs, now_secs())))
}

/// Decides direct-vs-relayed for each peer address.
///
/// Split from the handler so the judgement is testable without a WireGuard
/// interface or a cluster. It reports a number that is meant to justify a
/// change in a bill, so getting it wrong in the optimistic direction — claiming
/// "direct" for a path that is actually relayed — is the expensive mistake, and
/// the one the tests below are pointed at.
fn path_statuses(peers: &[wg::Peer], addrs: &[String], now: u64) -> Vec<PathStatus> {
    addrs
        .iter()
        .map(|addr| {
            let want = format!("{addr}/128");
            // A peer only counts as direct when it owns the node's REAL address.
            // A probe peer parked on the junk `dead:` address must never read as
            // direct: it carries no production traffic at all.
            let direct = peers
                .iter()
                .find(|p| p.allowed_ips.iter().any(|a| a == &want));
            match direct {
                // Alive as well as promoted. A promoted peer whose handshake has
                // gone stale is a blackhole, not a working direct path, and
                // reporting it as "direct" would advertise a saving that is
                // actually an outage.
                Some(p) if p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) => PathStatus {
                    node: addr.clone(),
                    path: "direct".into(),
                    endpoint: p.endpoint.clone(),
                    handshake_age_secs: Some(now.saturating_sub(p.last_handshake)),
                },
                _ => PathStatus {
                    node: addr.clone(),
                    path: "relayed".into(),
                    endpoint: None,
                    handshake_age_secs: None,
                },
            }
        })
        .collect()
}

// ── Peer enumeration ──────────────────────────────────────────────────────────

/// Where the last known peer list is kept.
///
/// THE CIRCULAR DEPENDENCY THIS BREAKS: peers come from the Kubernetes API,
/// which is served by k3s, whose embedded etcd commits every write across the
/// very relay this module exists to bypass. On a two-node cluster that etcd has
/// no fault tolerance, so when the mesh is slow the API is slow — and the code
/// that would fix the latency cannot enumerate anyone to fix it for. Observed
/// directly: `kubectl get nodes` timing out on node1 while wg1 was perfectly
/// healthy.
///
/// So the API stays the source of truth, and its answer is remembered. Peers
/// change when someone deliberately adds a machine, which is rare; a cache that
/// is a few hours stale is still right, and being right while the cluster is
/// unhappy is exactly when this matters.
const PEER_CACHE: &str = "/var/lib/yolab/mesh-peers.json";

/// Every other node's cluster address.
///
/// Prefers a live answer and falls back to the last one. A live answer is also
/// written back, so the cache warms itself with no separate bootstrap.
async fn peer_addresses(self_ip: &str) -> Vec<String> {
    match live_peer_addresses(self_ip).await {
        Some(peers) => {
            if let Ok(json) = serde_json::to_string(&peers) {
                if let Some(dir) = std::path::Path::new(PEER_CACHE).parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(PEER_CACHE, json);
            }
            peers
        }
        None => {
            let cached: Vec<String> = std::fs::read_to_string(PEER_CACHE)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            if !cached.is_empty() {
                tracing::debug!(
                    "mesh: cluster API unavailable, using {} cached peers",
                    cached.len()
                );
            }
            // Defensive: self must never appear, even if the cache was written
            // before this node's address changed.
            cached.into_iter().filter(|a| a != self_ip).collect()
        }
    }
}

/// `None` when the cluster could not be asked, which is different from "asked,
/// and there are no other nodes" — the second is a legitimate single-node
/// answer and must NOT clear a cache that has real peers in it.
async fn live_peer_addresses(self_ip: &str) -> Option<Vec<String>> {
    Some(parse_peer_addresses(
        &kubectl::get_nodes().await.ok()?,
        self_ip,
    ))
}

/// Pulls every other node's IPv6 InternalIP out of a `kubectl get nodes` list.
///
/// Split out so the two things that actually matter here are testable without a
/// cluster: that this node is excluded (probing yourself wastes a cycle and
/// would promote a peer to your own address), and that IPv4 InternalIPs are
/// ignored — the mesh is v6-only and a v4 address here would produce an
/// allowed-ips that never matches anything.
fn parse_peer_addresses(nodes: &[serde_json::Value], self_ip: &str) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| {
            n["status"]["addresses"]
                .as_array()?
                .iter()
                .find(|a| {
                    a["type"] == "InternalIP"
                        && a["address"].as_str().is_some_and(|s| s.contains(':'))
                })
                .and_then(|a| a["address"].as_str())
                .map(String::from)
        })
        .filter(|a| a != self_ip)
        .collect()
}

/// A junk address to hang a probe peer on, unique per peer.
///
/// Derived by stamping 0xdead into the seventh hextet of the peer's cluster
/// address, which lands it OUTSIDE fd00:cafe::/112 — inside, and it would steal
/// a real cluster address from the hub peer, breaking the very path being
/// probed. Unique per peer so two probes cannot fight over one address, since
/// WireGuard gives any address to exactly one peer.
pub fn probe_address(peer: &str) -> Option<String> {
    let ip: Ipv6Addr = peer.parse().ok()?;
    let mut seg = ip.segments();
    seg[6] = 0xdead;
    Some(format!("{}/128", Ipv6Addr::from(seg)))
}

/// The inverse of `probe_address`: recovers the peer's real cluster address
/// from the junk one, or `None` if this was never a probe address.
///
/// This is what lets promotion be a purely local decision. The junk address
/// already encodes which node it stands for, so a node can promote a peer it
/// only ever learned about by being called — no candidate exchange, and so no
/// dependence on a control channel that half-promotion has already broken.
///
/// Only the seventh hextet is stamped, and every address in `fd00:cafe::/112`
/// has that hextet zero, so the mapping is exact in both directions.
pub fn real_address(probe: &str) -> Option<String> {
    let ip: Ipv6Addr = probe.trim_end_matches("/128").parse().ok()?;
    let mut seg = ip.segments();
    if seg[6] != 0xdead {
        return None;
    }
    seg[6] = 0;
    Some(format!("{}/128", Ipv6Addr::from(seg)))
}

/// True when the kernel would send this address down the tunnel.
///
/// The quiet failure this prevents: a candidate that is only reachable VIA wg1
/// still completes a handshake, so the peer is promoted, the UI reports
/// "direct", and every byte is still relayed and still billed.
async fn routes_via_tunnel(addr: &str) -> bool {
    let args: Vec<&str> = if addr.contains(':') {
        vec!["-6", "route", "get", addr]
    } else {
        vec!["route", "get", addr]
    };
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("ip")
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    else {
        // Unknown means treat as unusable: a wrong "direct" costs money
        // silently, a wrong "relayed" costs nothing but a relayed connection.
        return true;
    };
    String::from_utf8_lossy(&out.stdout).contains(&format!("dev {}", wg::IFACE))
}

// ── The loop ──────────────────────────────────────────────────────────────────

pub async fn run() {
    let host = &RealHost;
    let mut last_probe: HashMap<String, Instant> = HashMap::new();
    // Per-peer byte counters from the previous reconcile, for blackhole
    // detection. In-memory only: a restart simply re-observes them next tick.
    let mut blackhole: HashMap<String, Traffic> = HashMap::new();
    let mut ticks: u64 = 0;
    loop {
        // Local reconcile FIRST and often. It needs nothing but `wg show`, which
        // is what lets a half-promoted cluster heal itself — see reconcile_local.
        if let Err(e) = reconcile_local(host, &mut blackhole).await {
            tracing::warn!("mesh: reconcile: {e:#}");
        }
        // Discovery is the expensive, network-dependent half, so it stays slow.
        if ticks.is_multiple_of(TICK.as_secs() / FAST_TICK.as_secs()) {
            if let Err(e) = tick(host, &mut last_probe).await {
                tracing::warn!("mesh: {e:#}");
            }
        }
        ticks = ticks.wrapping_add(1);
        tokio::time::sleep(FAST_TICK).await;
    }
}

/// Promotes proven peers and demotes dead ones, using ONLY local WireGuard
/// state. This function must never make a network call, and that constraint is
/// the entire point of it.
///
/// ## The outage this exists to prevent
///
/// WireGuard's allowed-ips is one trie: an address belongs to exactly ONE peer,
/// and that governs INBOUND validation as much as outbound routing. Promoting
/// therefore does not add a preferred route alongside a working relay path — it
/// TRANSFERS ownership of that address away from the hub, both directions at
/// once. "Longest-prefix match gives fallback for free" was simply wrong.
///
/// The consequence is that a half-promoted pair is a total blackhole, not a
/// degraded path:
///
///   - node2 promoted, node1 not: node2 sends direct; node1 sees source ::6 from
///     a peer whose allowed-ips is only the junk probe address, and drops it.
///   - node1 sends via the hub; node2 sees source ::5 arriving from the HUB peer,
///     but ::5 now belongs to its direct node1 peer, so it drops that too.
///
/// Handshakes keep succeeding throughout, because they are authenticated per-key
/// and never consult allowed-ips — so a liveness check that only watches
/// handshake age cannot see any of this. Observed on the live cluster: node2 had
/// sent 302 KiB to node1 while node1 counted 900 B received, because rx_bytes is
/// only incremented for packets that PASS the allowed-ips check.
///
/// And it is self-sealing: the cluster addresses are exactly what
/// `fetch_candidates` talks over, so once asymmetric, the HTTP call that would
/// let the other side catch up can no longer complete. etcd lost quorum, Ceph
/// mons could not form one, and k3s hung behind the RBD mount that depends on
/// them — three different-looking failures, one cause.
///
/// So promotion is decided here, locally, from evidence both sides observe
/// independently at nearly the same instant: a completed handshake at an
/// endpoint that is not the relay. Both nodes converge within one FAST_TICK
/// without ever needing to talk to each other about it.
async fn reconcile_local<H: Host>(
    host: &H,
    blackhole: &mut HashMap<String, Traffic>,
) -> anyhow::Result<()> {
    let now = now_secs();

    for p in wg::peers(host).await? {
        // The hub owns the whole subnet and is never ours to touch.
        if p.allowed_ips.iter().any(|a| a.ends_with(SUBNET_SUFFIX)) {
            continue;
        }

        // A primed or probing peer: still parked on a junk address.
        if let Some(real) = p.allowed_ips.iter().find_map(|a| real_address(a)) {
            if !p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) {
                continue;
            }
            let Some(endpoint) = p.endpoint.as_deref() else {
                continue;
            };
            // A handshake proves the key; this proves the PATH. Without it a
            // peer that handshaked over the relay would be promoted as
            // "direct", relaying every byte while reporting that it does not.
            if endpoint_via_tunnel(endpoint).await {
                continue;
            }
            tracing::info!("mesh: promoting {real} — handshake at {endpoint}");
            wg::set_peer(host, &p.public_key, None, &real, 25).await?;
            continue;
        }

        // Already promoted. Demote once the direct path stops answering, which
        // hands the address back to the hub on the very next packet.
        if !p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) {
            tracing::info!(
                "mesh: {} went stale — falling back to the relay",
                p.allowed_ips.join(",")
            );
            wg::remove_peer(host, &p.public_key).await?;
            blackhole.remove(&p.public_key);
            continue;
        }

        // Alive by handshake, and STILL possibly a blackhole. See
        // `is_blackholed`.
        if is_blackholed(blackhole.get(&p.public_key), &p, now) {
            tracing::warn!(
                "mesh: {} handshakes but carries no traffic — falling back to the relay",
                p.allowed_ips.join(",")
            );
            wg::remove_peer(host, &p.public_key).await?;
            blackhole.remove(&p.public_key);
            continue;
        }
        blackhole.insert(p.public_key.clone(), Traffic::from(&p, now));
    }
    Ok(())
}

/// What a peer's counters looked like last tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Traffic {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub at: u64,
}

impl Traffic {
    fn from(p: &wg::Peer, now: u64) -> Self {
        Self {
            rx_bytes: p.rx_bytes,
            tx_bytes: p.tx_bytes,
            at: now,
        }
    }
}

/// How long a promoted peer may send without receiving before it is demoted.
///
/// Generous on purpose: an idle pair legitimately exchanges almost nothing, and
/// the keepalives that do flow are counted, so this only fires when we are
/// actively pushing bytes at a peer that answers none of them.
const BLACKHOLE_SECS: u64 = 90;

/// Whether a promoted peer is sending into a void.
///
/// THE CASE HANDSHAKE-FRESHNESS CANNOT SEE, and the reason this exists at all.
/// Handshakes are authenticated per-key and never consult allowed-ips, so two
/// nodes can handshake perfectly while every data packet between them is
/// dropped — which is exactly what a half-promoted pair does. On 2026-09-08
/// node2 had sent 302 KiB to node1 while node1 counted 900 B received, and the
/// liveness check saw a healthy 55-second-old handshake the entire time.
///
/// rx_bytes is the signal because the kernel only increments it for packets
/// that PASSED the allowed-ips check — a dropped packet takes the "dishonest
/// packet" path and is never counted. So tx climbing while rx stands still is
/// the precise shape of "the far side is discarding us".
///
/// Requires tx to have MOVED. Without that an idle peer — nothing to send,
/// nothing to receive — would be demoted every 90 seconds forever, flapping a
/// working path for no reason.
fn is_blackholed(previous: Option<&Traffic>, now_peer: &wg::Peer, now: u64) -> bool {
    let Some(prev) = previous else {
        return false;
    };
    if now.saturating_sub(prev.at) < BLACKHOLE_SECS {
        return false;
    }
    let sent = now_peer.tx_bytes.saturating_sub(prev.tx_bytes);
    let received = now_peer.rx_bytes.saturating_sub(prev.rx_bytes);
    sent > 0 && received == 0
}

/// The address out of a WireGuard `host:port` endpoint.
///
/// Split out from the routing question purely so it can be tested: an IPv6
/// endpoint is `[fd00::1]:51821`, which is full of colons, and getting this
/// wrong would hand `routes_via_tunnel` a malformed address. That fails closed
/// — an unparseable address is treated as tunnelled and the peer is never
/// promoted — so the bug would present as "direct paths silently never happen"
/// rather than as anything pointing here.
///
/// `rsplit_once` because the port is after the LAST colon; splitting on the
/// first would return `[fd00` for every v6 endpoint.
fn endpoint_host(endpoint: &str) -> &str {
    match endpoint.rsplit_once(':') {
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => endpoint,
    }
}

/// Whether a WireGuard endpoint's address routes through the tunnel.
async fn endpoint_via_tunnel(endpoint: &str) -> bool {
    routes_via_tunnel(endpoint_host(endpoint)).await
}

async fn tick<H: Host>(host: &H, last_probe: &mut HashMap<String, Instant>) -> anyhow::Result<()> {
    let cfg = crate::config::Config::from_env();
    let self_ip = cfg.node_ipv6.clone();
    let token = cfg.cluster_token();
    let now = now_secs();
    // Sent on every candidates request so the responder can prime itself to
    // accept our handshake — see MESH_PUBKEY_HEADER's doc comment.
    let self_key = wg::self_public_key(host).await?;

    for peer_addr in peer_addresses(&self_ip).await {
        let Some(probe_addr) = probe_address(&peer_addr) else {
            continue;
        };
        let real_addr = format!("{peer_addr}/128");

        // Ask the peer who it is and where it can be reached — over the relay,
        // which is working, to build the path that replaces it.
        let cand = match fetch_candidates(&peer_addr, cfg.port, &token, &self_key).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("mesh: {peer_addr} did not answer: {e:#}");
                continue;
            }
        };

        let existing = wg::peers(host)
            .await?
            .into_iter()
            .find(|p| p.public_key == cand.public_key);

        let promoted = existing
            .as_ref()
            .is_some_and(|p| p.allowed_ips.iter().any(|a| a == &real_addr));

        if promoted {
            let alive = existing
                .as_ref()
                .is_some_and(|p| p.is_alive(now, HANDSHAKE_MAX_AGE_SECS));
            if alive {
                continue;
            }
            tracing::info!("mesh: {peer_addr} direct path went stale — falling back to relay");
            wg::remove_peer(host, &cand.public_key).await?;
            last_probe.insert(peer_addr.clone(), Instant::now());
            continue;
        }

        // Relayed. Probe occasionally rather than every tick.
        if last_probe
            .get(&peer_addr)
            .is_some_and(|t| t.elapsed() < PROBE_RETRY)
        {
            continue;
        }
        last_probe.insert(peer_addr.clone(), Instant::now());

        if let Some(endpoint) = probe(host, &cand, &probe_addr).await {
            tracing::info!("mesh: {peer_addr} reachable directly at {endpoint} — promoting");
            wg::set_peer(host, &cand.public_key, Some(&endpoint), &real_addr, 25).await?;
        } else {
            // Logged even on the ordinary case (a peer this node cannot reach
            // directly, which is most peers most of the time): the alternative
            // is total silence on failure, which is exactly what made the
            // priming bug this comment sits next to invisible for as long as it
            // was. A relayed peer staying relayed should be visible, not mute.
            tracing::debug!("mesh: {peer_addr} not reachable directly — staying relayed");
            // Leave nothing behind: a peer with a junk allowed-ips is harmless
            // but confusing, and it would look like a direct path in `wg show`.
            let _ = wg::remove_peer(host, &cand.public_key).await;
        }
    }
    Ok(())
}

/// Tries each candidate; returns the first endpoint that completes a handshake.
async fn probe<H: Host>(host: &H, cand: &Candidates, probe_addr: &str) -> Option<String> {
    for addr in &cand.addresses {
        if routes_via_tunnel(addr).await {
            tracing::debug!("mesh: skipping {addr}, it routes through the tunnel");
            continue;
        }
        let endpoint = if addr.contains(':') {
            format!("[{}]:{}", addr, cand.listen_port)
        } else {
            format!("{}:{}", addr, cand.listen_port)
        };

        // Junk allowed-ips and a tight keepalive: the keepalive is what forces a
        // handshake attempt when no traffic routes to this peer.
        if wg::set_peer(host, &cand.public_key, Some(&endpoint), probe_addr, 5)
            .await
            .is_err()
        {
            continue;
        }

        let deadline = Instant::now() + PROBE_TIMEOUT;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let alive = wg::peers(host)
                .await
                .unwrap_or_default()
                .iter()
                .find(|p| p.public_key == cand.public_key)
                .is_some_and(|p| p.is_alive(now_secs(), PROBE_TIMEOUT.as_secs() + 2));
            if alive {
                return Some(endpoint);
            }
        }
    }
    None
}

async fn fetch_candidates(
    peer: &str,
    port: u16,
    token: &str,
    self_key: &str,
) -> anyhow::Result<Candidates> {
    Ok(reqwest::Client::new()
        .get(format!(
            "http://[{peer}]:{port}/api/cluster/mesh-candidates"
        ))
        .header(CLUSTER_AUTH_HEADER, token)
        // Primes the peer to accept OUR handshake — see MESH_PUBKEY_HEADER.
        .header(MESH_PUBKEY_HEADER, self_key)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json::<Candidates>()
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const HUB: &str = "HUBKEY=";
    const PEER: &str = "PEERKEY=";

    /// One `wg show wg1 dump` line. The interface line is prepended by
    /// `dump_of`, since parse_dump skips it (it holds the private key).
    fn peer_line(key: &str, endpoint: &str, allowed: &str, hs: u64, rx: u64, tx: u64) -> String {
        format!("{key}\t(none)\t{endpoint}\t{allowed}\t{hs}\t{rx}\t{tx}\t25\n")
    }

    fn dump_of(lines: &[String]) -> String {
        format!("PRIV=\tPUBSELF=\t51821\toff\n{}", lines.concat())
    }

    /// The hub peer, which reconcile_local must never touch.
    fn hub_line() -> String {
        peer_line(HUB, "78.47.104.10:51820", "fd00:cafe::/112", 1_000, 9, 9)
    }

    fn host_with(dump: &str) -> FakeHost {
        FakeHost::new()
            .ok("wg show wg1 dump", dump)
            // A LAN address: `ip route get` shows a physical device, so
            // endpoint_via_tunnel says false and promotion is allowed.
            .ok("ip -6 route get", "fd00:cafe::6 dev enp5s0 src fd00:cafe::5")
            .ok("ip route get", "192.168.1.141 dev enp5s0 src 192.168.1.132")
            .ok("wg set", "")
    }

    #[tokio::test]
    async fn a_primed_peer_with_a_lan_handshake_is_promoted_to_its_real_address() {
        // The exact state node1 was stuck in on 2026-09-08: primed against the
        // junk address, handshaking fine over the LAN, and never promoted —
        // which half-promoted the pair and blackholed the cluster.
        let dump = dump_of(&[
            hub_line(),
            peer_line(
                PEER,
                "192.168.1.141:51821",
                "fd00:cafe::dead:6/128",
                now_secs(),
                100,
                100,
            ),
        ]);
        let host = host_with(&dump);
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(
            host.ran("wg set wg1 peer PEERKEY= allowed-ips fd00:cafe::6/128"),
            "calls were: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn the_hub_peer_is_never_touched() {
        // It owns the whole /112 and is the fallback. Removing or rewriting it
        // would take the node off the mesh entirely.
        let host = host_with(&dump_of(&[hub_line()]));
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(
            !host.calls().iter().any(|c| c.contains(HUB)),
            "calls were: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_primed_peer_that_never_handshaked_is_left_alone() {
        // Handshake 0 means the far side has never answered. Promoting on that
        // would point production traffic at an unproven endpoint.
        let dump = dump_of(&[
            hub_line(),
            peer_line(PEER, "192.168.1.141:51821", "fd00:cafe::dead:6/128", 0, 0, 0),
        ]);
        let host = host_with(&dump);
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(!host.calls().iter().any(|c| c.starts_with("wg set")));
    }

    #[tokio::test]
    async fn a_promoted_peer_whose_handshake_went_stale_is_demoted() {
        // Stale handshake = the direct path stopped answering. Removing the
        // peer hands the address back to the hub on the next packet; leaving it
        // would blackhole, because the /128 still wins longest-prefix match.
        let dump = dump_of(&[
            hub_line(),
            peer_line(
                PEER,
                "192.168.1.141:51821",
                "fd00:cafe::6/128",
                now_secs() - (HANDSHAKE_MAX_AGE_SECS + 60),
                100,
                100,
            ),
        ]);
        let host = host_with(&dump);
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(
            host.ran("wg set wg1 peer PEERKEY= remove"),
            "calls were: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_healthy_promoted_peer_is_left_exactly_as_it_is() {
        let dump = dump_of(&[
            hub_line(),
            peer_line(
                PEER,
                "192.168.1.141:51821",
                "fd00:cafe::6/128",
                now_secs(),
                500,
                500,
            ),
        ]);
        let host = host_with(&dump);
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(
            !host.calls().iter().any(|c| c.starts_with("wg set")),
            "a working direct path must not be disturbed, calls were: {:?}",
            host.calls()
        );
    }

    // ── prime_caller ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_unknown_caller_is_primed_as_a_listen_only_peer() {
        // No endpoint, and a junk allowed-ips derived from the caller's own
        // address — enough for WireGuard to accept the handshake it is about to
        // send, and not enough to divert any production traffic.
        let host = host_with(&dump_of(&[hub_line()]));
        prime_caller(&host, PEER, "fd00:cafe::6".parse().unwrap()).await;

        assert!(
            host.ran("wg set wg1 peer PEERKEY= allowed-ips fd00:cafe::dead:6/128"),
            "calls were: {:?}",
            host.calls()
        );
        assert!(
            !host.calls().iter().any(|c| c.contains("endpoint")),
            "priming must not set an endpoint — WireGuard learns it from the \
             first valid packet: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_caller_we_already_know_is_left_completely_alone() {
        // THE REGRESSION THIS GUARDS. mesh_candidates is called on every tick
        // regardless of promotion state, so re-priming an already-promoted peer
        // would overwrite its real allowed-ips with the junk one once a minute,
        // silently breaking a working direct path forever.
        let promoted = peer_line(
            PEER,
            "192.168.1.141:51821",
            "fd00:cafe::6/128",
            now_secs(),
            10,
            10,
        );
        let host = host_with(&dump_of(&[hub_line(), promoted]));
        prime_caller(&host, PEER, "fd00:cafe::6".parse().unwrap()).await;

        assert!(
            !host.calls().iter().any(|c| c.starts_with("wg set")),
            "calls were: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_caller_from_an_ipv4_address_is_not_primed() {
        // probe_address only makes sense for the v6 cluster subnet; a v4 source
        // yields no probe address, and inventing one would produce an
        // allowed-ips that never matches anything.
        let host = host_with(&dump_of(&[hub_line()]));
        prime_caller(&host, PEER, "192.168.1.141".parse().unwrap()).await;

        assert!(!host.calls().iter().any(|c| c.starts_with("wg set")));
    }

    // ── path_statuses ─────────────────────────────────────────────────────────

    #[test]
    fn a_promoted_and_live_peer_reports_direct_with_its_endpoint() {
        let peers = vec![wg::Peer {
            public_key: PEER.into(),
            endpoint: Some("192.168.1.141:51821".into()),
            allowed_ips: vec!["fd00:cafe::6/128".into()],
            last_handshake: 1_000,
            rx_bytes: 10,
            tx_bytes: 10,
        }];
        let out = path_statuses(&peers, &["fd00:cafe::6".to_string()], 1_030);
        assert_eq!(out[0].path, "direct");
        assert_eq!(out[0].endpoint.as_deref(), Some("192.168.1.141:51821"));
        assert_eq!(out[0].handshake_age_secs, Some(30));
    }

    #[test]
    fn a_peer_still_on_the_probe_address_reports_relayed() {
        // It carries no production traffic — the real address still belongs to
        // the hub — so reporting "direct" would claim a saving that is not real.
        let peers = vec![wg::Peer {
            public_key: PEER.into(),
            endpoint: Some("192.168.1.141:51821".into()),
            allowed_ips: vec!["fd00:cafe::dead:6/128".into()],
            last_handshake: 1_000,
            rx_bytes: 10,
            tx_bytes: 10,
        }];
        let out = path_statuses(&peers, &["fd00:cafe::6".to_string()], 1_030);
        assert_eq!(out[0].path, "relayed");
        assert_eq!(out[0].endpoint, None);
    }

    #[test]
    fn a_promoted_peer_with_a_stale_handshake_reports_relayed_not_direct() {
        // It is promoted but blackholing. Calling that "direct" would advertise
        // a saving that is in fact an outage.
        let peers = vec![wg::Peer {
            public_key: PEER.into(),
            endpoint: Some("192.168.1.141:51821".into()),
            allowed_ips: vec!["fd00:cafe::6/128".into()],
            last_handshake: 1_000,
            rx_bytes: 10,
            tx_bytes: 10,
        }];
        let out = path_statuses(
            &peers,
            &["fd00:cafe::6".to_string()],
            1_000 + HANDSHAKE_MAX_AGE_SECS + 1,
        );
        assert_eq!(out[0].path, "relayed");
    }

    #[test]
    fn a_node_with_no_peer_entry_at_all_reports_relayed() {
        let out = path_statuses(&[], &["fd00:cafe::6".to_string()], 1_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].node, "fd00:cafe::6");
        assert_eq!(out[0].path, "relayed");
    }

    #[test]
    fn every_address_gets_exactly_one_row_in_order() {
        let addrs = vec!["fd00:cafe::6".to_string(), "fd00:cafe::7".to_string()];
        let out = path_statuses(&[], &addrs, 1_000);
        assert_eq!(
            out.iter().map(|p| p.node.as_str()).collect::<Vec<_>>(),
            vec!["fd00:cafe::6", "fd00:cafe::7"]
        );
    }

    // ── blackhole detection ───────────────────────────────────────────────────

    fn peer(rx: u64, tx: u64) -> wg::Peer {
        wg::Peer {
            public_key: PEER.into(),
            endpoint: Some("192.168.1.141:51821".into()),
            allowed_ips: vec!["fd00:cafe::6/128".into()],
            last_handshake: 1_000,
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    #[test]
    fn sending_without_receiving_is_a_blackhole() {
        // The 2026-09-08 shape: node2 sent 302 KiB, node1 received 900 B, and
        // the handshake looked healthy throughout.
        let before = Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        };
        assert!(is_blackholed(
            Some(&before),
            &peer(900, 310_000),
            BLACKHOLE_SECS + 1
        ));
    }

    #[test]
    fn an_idle_peer_is_not_a_blackhole() {
        // Nothing sent, nothing received. Demoting here would flap a perfectly
        // good path every 90 seconds for the crime of being quiet.
        let before = Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        };
        assert!(!is_blackholed(
            Some(&before),
            &peer(900, 1_000),
            BLACKHOLE_SECS + 1
        ));
    }

    #[test]
    fn a_peer_that_is_answering_is_not_a_blackhole() {
        let before = Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        };
        assert!(!is_blackholed(
            Some(&before),
            &peer(5_000, 9_000),
            BLACKHOLE_SECS + 1
        ));
    }

    #[test]
    fn the_window_must_elapse_before_anything_counts() {
        // Sampling too soon would demote on one busy instant of asymmetry.
        let before = Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        };
        assert!(!is_blackholed(
            Some(&before),
            &peer(900, 310_000),
            BLACKHOLE_SECS - 1
        ));
    }

    #[test]
    fn a_peer_seen_for_the_first_time_is_never_a_blackhole() {
        // No previous sample means no delta to judge by.
        assert!(!is_blackholed(None, &peer(0, 999_999), u64::MAX));
    }

    #[test]
    fn counters_that_went_backwards_do_not_read_as_a_blackhole() {
        // wg counters reset when an interface is recreated. saturating_sub
        // makes that read as "no traffic", not as a huge phantom send.
        let before = Traffic {
            rx_bytes: 9_000,
            tx_bytes: 9_000,
            at: 0,
        };
        assert!(!is_blackholed(
            Some(&before),
            &peer(10, 10),
            BLACKHOLE_SECS + 1
        ));
    }

    #[test]
    fn probe_address_lands_outside_the_cluster_subnet() {
        // fd00:cafe::/112 covers fd00:cafe:: through fd00:cafe::ffff. Stamping
        // the seventh hextet leaves that range, so probing cannot steal a real
        // node address away from the hub peer.
        let p = probe_address("fd00:cafe::6").unwrap();
        assert_eq!(p, "fd00:cafe::dead:6/128");
        let ip: Ipv6Addr = p.trim_end_matches("/128").parse().unwrap();
        assert_ne!(ip.segments()[6], 0, "would fall inside fd00:cafe::/112");
    }

    #[test]
    fn probe_addresses_differ_per_peer_so_two_probes_cannot_collide() {
        assert_ne!(
            probe_address("fd00:cafe::6").unwrap(),
            probe_address("fd00:cafe::7").unwrap()
        );
    }

    #[test]
    fn a_non_address_yields_no_probe_rather_than_a_malformed_one() {
        assert_eq!(probe_address("not-an-ip"), None);
        assert_eq!(probe_address("192.168.1.1"), None);
    }

    #[test]
    fn an_ipv6_endpoint_keeps_its_colons_and_loses_its_brackets() {
        // The case that motivated splitting this out: splitting on the FIRST
        // colon would return "[fd00" for every v6 endpoint, and the resulting
        // unparseable address fails closed — so the symptom would be "direct
        // paths silently never happen", pointing nowhere near here.
        assert_eq!(endpoint_host("[fd00:cafe::6]:51821"), "fd00:cafe::6");
        assert_eq!(
            endpoint_host("[2a01:e0a:80a:ba90::1]:51820"),
            "2a01:e0a:80a:ba90::1"
        );
    }

    #[test]
    fn an_ipv4_endpoint_loses_only_its_port() {
        assert_eq!(endpoint_host("192.168.1.141:51821"), "192.168.1.141");
        assert_eq!(endpoint_host("78.47.104.10:51820"), "78.47.104.10");
    }

    #[test]
    fn an_endpoint_with_no_port_is_returned_whole() {
        // Not a shape wg emits, but returning something malformed here would
        // fail closed and silently stop promotion, so it is pinned.
        assert_eq!(endpoint_host("192.168.1.141"), "192.168.1.141");
    }

    #[test]
    fn a_probe_address_round_trips_back_to_the_node_it_stands_for() {
        // The property promotion depends on: a node that only ever learned of a
        // peer by being CALLED can still work out which address to promote,
        // without asking anyone. Half-promotion breaks the asking.
        for node in ["fd00:cafe::5", "fd00:cafe::6", "fd00:cafe::ffff"] {
            let probe = probe_address(node).unwrap();
            assert_eq!(real_address(&probe), Some(format!("{node}/128")));
        }
    }

    #[test]
    fn a_real_address_is_not_mistaken_for_a_probe_one() {
        // Guards the branch in reconcile_local: treating an already-promoted
        // peer as still-probing would rewrite its allowed-ips every 5 seconds.
        assert_eq!(real_address("fd00:cafe::6/128"), None);
        assert_eq!(real_address("fd00:cafe::/112"), None);
        assert_eq!(real_address("not-an-ip"), None);
    }

    #[test]
    fn real_address_accepts_the_form_wg_actually_prints() {
        // `wg show` reports allowed-ips with the prefix attached, so the
        // suffix has to be tolerated rather than assumed away.
        assert_eq!(
            real_address("fd00:cafe::dead:6/128"),
            Some("fd00:cafe::6/128".into())
        );
        assert_eq!(
            real_address("fd00:cafe::dead:6"),
            Some("fd00:cafe::6/128".into())
        );
    }

    fn node(ip: &str) -> serde_json::Value {
        serde_json::json!({"status":{"addresses":[
            {"type":"Hostname","address":"whatever"},
            {"type":"InternalIP","address":ip}]}})
    }

    #[test]
    fn this_node_is_never_its_own_peer() {
        let nodes = vec![node("fd00:cafe::5"), node("fd00:cafe::6")];
        assert_eq!(
            parse_peer_addresses(&nodes, "fd00:cafe::5"),
            vec!["fd00:cafe::6"]
        );
    }

    #[test]
    fn an_ipv4_internal_ip_is_ignored_rather_than_promoted_to_a_dead_allowed_ips() {
        let nodes = vec![node("10.0.0.7")];
        assert!(parse_peer_addresses(&nodes, "fd00:cafe::5").is_empty());
    }

    #[test]
    fn a_single_node_cluster_yields_no_peers_without_erroring() {
        let nodes = vec![node("fd00:cafe::5")];
        assert!(parse_peer_addresses(&nodes, "fd00:cafe::5").is_empty());
    }
}
