pub mod candidates;
pub mod wg;

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{
    auth::CLUSTER_AUTH_HEADER,
    error::{Outcome, Result},
    host::{Host, RealHost},
    kubectl, AppState,
};

pub use candidates::Candidates;

const HANDSHAKE_MAX_AGE_SECS: u64 = 180;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

const PROBE_RETRY: Duration = Duration::from_secs(300);

const TICK: Duration = Duration::from_secs(60);

const FAST_TICK: Duration = Duration::from_secs(5);

const SUBNET_SUFFIX: &str = "/112";

const MESH_PUBKEY_HEADER: &str = "x-yolab-mesh-pubkey";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
        addresses: candidates::local_addresses(host).await?,
    }))
}

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

#[derive(Serialize)]
pub struct PathStatus {
    pub node: String,
    pub path: String,
    pub endpoint: Option<String>,
    pub handshake_age_secs: Option<u64>,
}

pub async fn paths(State(state): State<AppState>) -> Result<Json<Vec<PathStatus>>> {
    let host = &RealHost;
    let peers = wg::peers(host).await.unwrap_or_default();
    let addrs = peer_addresses(&state.config.node_ipv6).await;
    Ok(Json(path_statuses(&peers, &addrs, now_secs())))
}

fn path_statuses(peers: &[wg::Peer], addrs: &[String], now: u64) -> Vec<PathStatus> {
    addrs
        .iter()
        .map(|addr| {
            let want = format!("{addr}/128");
            let direct = peers
                .iter()
                .find(|p| p.allowed_ips.iter().any(|a| a == &want));
            match direct {
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

const PEER_CACHE: &str = "/var/lib/yolab/mesh-peers.json";

async fn peer_addresses(self_ip: &str) -> Vec<String> {
    match live_peer_addresses(self_ip).await {
        Some(peers) => {
            if let Ok(json) = serde_json::to_string(&peers) {
                let written = std::path::Path::new(PEER_CACHE)
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| std::fs::write(PEER_CACHE, json));
                written.warn_on_err("mesh: cache the peer list");
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
            cached.into_iter().filter(|a| a != self_ip).collect()
        }
    }
}

async fn live_peer_addresses(self_ip: &str) -> Option<Vec<String>> {
    Some(parse_peer_addresses(
        &kubectl::get_nodes().await.ok()?,
        self_ip,
    ))
}

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

pub fn probe_address(peer: &str) -> Option<String> {
    let ip: Ipv6Addr = peer.parse().ok()?;
    let mut seg = ip.segments();
    seg[6] = 0xdead;
    Some(format!("{}/128", Ipv6Addr::from(seg)))
}

pub fn real_address(probe: &str) -> Option<String> {
    let ip: Ipv6Addr = probe.trim_end_matches("/128").parse().ok()?;
    let mut seg = ip.segments();
    if seg[6] != 0xdead {
        return None;
    }
    seg[6] = 0;
    Some(format!("{}/128", Ipv6Addr::from(seg)))
}

async fn routes_via_tunnel<H: Host>(host: &H, addr: &str) -> bool {
    let args: Vec<&str> = if addr.contains(':') {
        vec!["-6", "route", "get", addr]
    } else {
        vec!["route", "get", addr]
    };
    let Ok(out) = host.run_cmd("ip", &args).await else {
        return true;
    };
    out.stdout.contains(&format!("dev {}", wg::IFACE))
}

pub struct MeshPathsController {
    blackhole: tokio::sync::Mutex<HashMap<String, Traffic>>,
}

impl MeshPathsController {
    pub fn new() -> Self {
        Self {
            blackhole: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl crate::runtime::Controller for MeshPathsController {
    fn name(&self) -> &'static str {
        "mesh-paths"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> Duration {
        FAST_TICK
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let mut blackhole = self.blackhole.lock().await;
        reconcile_local(&RealHost, &mut blackhole).await?;
        Ok(crate::runtime::Tick::Done)
    }
}

pub struct MeshDiscoveryController {
    last_probe: tokio::sync::Mutex<HashMap<String, Instant>>,
}

impl MeshDiscoveryController {
    pub fn new() -> Self {
        Self {
            last_probe: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl crate::runtime::Controller for MeshDiscoveryController {
    fn name(&self) -> &'static str {
        "mesh-discovery"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> Duration {
        TICK
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let mut last_probe = self.last_probe.lock().await;
        tick(&RealHost, &mut last_probe).await?;
        Ok(crate::runtime::Tick::Done)
    }
}

async fn reconcile_local<H: Host>(
    host: &H,
    blackhole: &mut HashMap<String, Traffic>,
) -> anyhow::Result<()> {
    let now = now_secs();

    for p in wg::peers(host).await? {
        if p.allowed_ips.iter().any(|a| a.ends_with(SUBNET_SUFFIX)) {
            continue;
        }

        if let Some(real) = p.allowed_ips.iter().find_map(|a| real_address(a)) {
            if !p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) {
                continue;
            }
            let Some(endpoint) = p.endpoint.as_deref() else {
                continue;
            };
            if endpoint_via_tunnel(host, endpoint).await {
                continue;
            }
            tracing::info!("mesh: promoting {real} — handshake at {endpoint}");
            wg::set_peer(host, &p.public_key, None, &real, 25).await?;
            continue;
        }

        if !p.is_alive(now, HANDSHAKE_MAX_AGE_SECS) {
            tracing::info!(
                "mesh: {} went stale — falling back to the relay",
                p.allowed_ips.join(",")
            );
            wg::remove_peer(host, &p.public_key).await?;
            blackhole.remove(&p.public_key);
            continue;
        }

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

const BLACKHOLE_SECS: u64 = 90;

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

fn endpoint_host(endpoint: &str) -> &str {
    match endpoint.rsplit_once(':') {
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => endpoint,
    }
}

async fn endpoint_via_tunnel<H: Host>(host: &H, endpoint: &str) -> bool {
    routes_via_tunnel(host, endpoint_host(endpoint)).await
}

async fn tick<H: Host>(host: &H, last_probe: &mut HashMap<String, Instant>) -> anyhow::Result<()> {
    let cfg = crate::config::Config::from_env();
    let self_ip = cfg.node_ipv6.clone();
    let token = cfg.cluster_token();
    let now = now_secs();
    let self_key = wg::self_public_key(host).await?;

    for peer_addr in peer_addresses(&self_ip).await {
        let Some(probe_addr) = probe_address(&peer_addr) else {
            continue;
        };
        let real_addr = format!("{peer_addr}/128");

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
            tracing::debug!("mesh: {peer_addr} not reachable directly — staying relayed");
            wg::remove_peer(host, &cand.public_key)
                .await
                .warn_on_err("mesh: remove the unproven probe peer");
        }
    }
    Ok(())
}

async fn probe<H: Host>(host: &H, cand: &Candidates, probe_addr: &str) -> Option<String> {
    for addr in &cand.addresses {
        if routes_via_tunnel(host, addr).await {
            tracing::debug!("mesh: skipping {addr}, it routes through the tunnel");
            continue;
        }
        let endpoint = if addr.contains(':') {
            format!("[{}]:{}", addr, cand.listen_port)
        } else {
            format!("{}:{}", addr, cand.listen_port)
        };

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

    fn peer_line(key: &str, endpoint: &str, allowed: &str, hs: u64, rx: u64, tx: u64) -> String {
        format!("{key}\t(none)\t{endpoint}\t{allowed}\t{hs}\t{rx}\t{tx}\t25\n")
    }

    fn dump_of(lines: &[String]) -> String {
        format!("PRIV=\tPUBSELF=\t51821\toff\n{}", lines.concat())
    }

    fn hub_line() -> String {
        peer_line(HUB, "78.47.104.10:51820", "fd00:cafe::/112", 1_000, 9, 9)
    }

    fn host_with(dump: &str) -> FakeHost {
        FakeHost::new()
            .ok("wg show wg1 dump", dump)
            .ok(
                "ip -6 route get",
                "fd00:cafe::6 dev enp5s0 src fd00:cafe::5",
            )
            .ok("ip route get", "192.168.1.141 dev enp5s0 src 192.168.1.132")
            .ok("wg set", "")
    }

    #[tokio::test]
    async fn a_primed_peer_with_a_lan_handshake_is_promoted_to_its_real_address() {
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
        let dump = dump_of(&[
            hub_line(),
            peer_line(
                PEER,
                "192.168.1.141:51821",
                "fd00:cafe::dead:6/128",
                0,
                0,
                0,
            ),
        ]);
        let host = host_with(&dump);
        let mut bh = HashMap::new();
        reconcile_local(&host, &mut bh).await.unwrap();

        assert!(!host.calls().iter().any(|c| c.starts_with("wg set")));
    }

    #[tokio::test]
    async fn a_promoted_peer_whose_handshake_went_stale_is_demoted() {
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

    #[tokio::test]
    async fn an_unknown_caller_is_primed_as_a_listen_only_peer() {
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
        let host = host_with(&dump_of(&[hub_line()]));
        prime_caller(&host, PEER, "192.168.1.141".parse().unwrap()).await;

        assert!(!host.calls().iter().any(|c| c.starts_with("wg set")));
    }

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
        assert!(!is_blackholed(None, &peer(0, 999_999), u64::MAX));
    }

    #[test]
    fn counters_that_went_backwards_do_not_read_as_a_blackhole() {
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
        assert_eq!(endpoint_host("192.168.1.141"), "192.168.1.141");
    }

    #[test]
    fn a_probe_address_round_trips_back_to_the_node_it_stands_for() {
        for node in ["fd00:cafe::5", "fd00:cafe::6", "fd00:cafe::ffff"] {
            let probe = probe_address(node).unwrap();
            assert_eq!(real_address(&probe), Some(format!("{node}/128")));
        }
    }

    #[test]
    fn a_real_address_is_not_mistaken_for_a_probe_one() {
        assert_eq!(real_address("fd00:cafe::6/128"), None);
        assert_eq!(real_address("fd00:cafe::/112"), None);
        assert_eq!(real_address("not-an-ip"), None);
    }

    #[test]
    fn real_address_accepts_the_form_wg_actually_prints() {
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
