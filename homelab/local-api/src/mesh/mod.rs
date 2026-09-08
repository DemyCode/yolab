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

use crate::{auth::CLUSTER_AUTH_HEADER, error::Result, kubectl, AppState};

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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── What this node offers peers ───────────────────────────────────────────────

/// `GET /api/cluster/mesh-candidates` — cluster-authed, node→node.
pub async fn mesh_candidates() -> Result<Json<Candidates>> {
    Ok(Json(Candidates {
        public_key: wg::self_public_key().await?,
        listen_port: wg::listen_port().await?,
        addresses: candidates::local_addresses().await?,
    }))
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
    let peers = wg::peers().await.unwrap_or_default();
    let now = now_secs();
    let mut out = Vec::new();

    for addr in peer_addresses(&state.config.node_ipv6).await {
        let direct = peers
            .iter()
            .find(|p| p.allowed_ips.iter().any(|a| a == &format!("{addr}/128")));
        out.push(match direct {
            Some(p) if p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) => PathStatus {
                node: addr,
                path: "direct".into(),
                endpoint: p.endpoint.clone(),
                handshake_age_secs: Some(now.saturating_sub(p.last_handshake)),
            },
            _ => PathStatus {
                node: addr,
                path: "relayed".into(),
                endpoint: None,
                handshake_age_secs: None,
            },
        });
    }
    Ok(Json(out))
}

// ── Peer enumeration ──────────────────────────────────────────────────────────

/// Every other node's cluster address, from the same source `update_all` uses.
async fn peer_addresses(self_ip: &str) -> Vec<String> {
    kubectl::get_nodes()
        .await
        .unwrap_or_default()
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
    let mut last_probe: HashMap<String, Instant> = HashMap::new();
    loop {
        if let Err(e) = tick(&mut last_probe).await {
            tracing::warn!("mesh: {e:#}");
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn tick(last_probe: &mut HashMap<String, Instant>) -> anyhow::Result<()> {
    let cfg = crate::config::Config::from_env();
    let self_ip = cfg.node_ipv6.clone();
    let token = cfg.cluster_token();
    let now = now_secs();

    for peer_addr in peer_addresses(&self_ip).await {
        let Some(probe_addr) = probe_address(&peer_addr) else {
            continue;
        };
        let real_addr = format!("{peer_addr}/128");

        // Ask the peer who it is and where it can be reached — over the relay,
        // which is working, to build the path that replaces it.
        let cand = match fetch_candidates(&peer_addr, cfg.port, &token).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("mesh: {peer_addr} did not answer: {e:#}");
                continue;
            }
        };

        let existing = wg::peers()
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
            wg::remove_peer(&cand.public_key).await?;
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

        if let Some(endpoint) = probe(&cand, &probe_addr).await {
            tracing::info!("mesh: {peer_addr} reachable directly at {endpoint} — promoting");
            wg::set_peer(&cand.public_key, &endpoint, &real_addr, 25).await?;
        } else {
            // Leave nothing behind: a peer with a junk allowed-ips is harmless
            // but confusing, and it would look like a direct path in `wg show`.
            let _ = wg::remove_peer(&cand.public_key).await;
        }
    }
    Ok(())
}

/// Tries each candidate; returns the first endpoint that completes a handshake.
async fn probe(cand: &Candidates, probe_addr: &str) -> Option<String> {
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
        if wg::set_peer(&cand.public_key, &endpoint, probe_addr, 5)
            .await
            .is_err()
        {
            continue;
        }

        let deadline = Instant::now() + PROBE_TIMEOUT;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let alive = wg::peers()
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

async fn fetch_candidates(peer: &str, port: u16, token: &str) -> anyhow::Result<Candidates> {
    Ok(reqwest::Client::new()
        .get(format!(
            "http://[{peer}]:{port}/api/cluster/mesh-candidates"
        ))
        .header(CLUSTER_AUTH_HEADER, token)
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
}
