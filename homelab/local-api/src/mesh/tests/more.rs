use super::*;
use crate::runtime::{Resource, Scope, Tick};

fn ctx(host: FakeHost) -> crate::runtime::Ctx {
    crate::runtime::Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools {
            host,
            ..crate::tools::Tools::unreachable()
        },
    }
}

fn config() -> (tempfile::TempDir, crate::config::Config) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "").unwrap();
    let cfg = crate::config::Config::for_test(&file);
    (dir, cfg)
}

#[tokio::test]
async fn the_path_controller_checks_this_machines_peers_every_few_seconds() {
    let c = MeshPathsController::new();
    assert_eq!(
        (c.name(), c.scope(), c.interval()),
        ("mesh-paths", Scope::Node, FAST_TICK)
    );
    let host = host_with(&dump_of(&[hub_line()]));
    assert_eq!(c.converge(&ctx(host)).await.unwrap(), Tick::Done);
    assert!(c.converge(&ctx(FakeHost::new())).await.is_err());
}

#[tokio::test]
async fn the_discovery_controller_needs_its_own_key_even_with_no_peer_to_ask() {
    let (_dir, cfg) = config();
    let c = MeshDiscoveryController::new(cfg);
    assert_eq!(
        (c.name(), c.scope(), c.interval(), c.depends_on()),
        (
            "mesh-discovery",
            Scope::Node,
            TICK,
            &[crate::runtime::KUBE_API][..]
        )
    );
    let host = FakeHost::new().ok("wg show wg1 public-key", "SELFKEY=");
    assert_eq!(c.converge(&ctx(host)).await.unwrap(), Tick::Done);
    assert!(c.converge(&ctx(FakeHost::new())).await.is_err());
}

#[tokio::test]
async fn a_garbled_remembered_peer_list_counts_as_none() {
    let cache = crate::storage::on_machine(PEER_CACHE);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, "not json").unwrap();
    assert_eq!(
        peer_addresses(&crate::k8s::Kube::from_environment(), "::1").await,
        Vec::<String>::new()
    );
}

#[test]
fn a_node_without_addresses_is_no_peer() {
    assert_eq!(
        parse_peer_addresses(
            &[serde_json::json!({ "metadata": { "name": "node2" } })],
            "::1"
        ),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn an_address_that_cannot_be_routed_is_taken_for_the_tunnel() {
    assert!(routes_via_tunnel(&FakeHost::new().unstartable("ip"), LAN).await);
}

#[tokio::test]
async fn a_peer_named_by_something_other_than_an_address_is_skipped() {
    let host = discovering(&[dump_of(&[hub_line()])], "unused");
    tick(&host, &["node2".to_string()], 1, TOKEN, &mut HashMap::new())
        .await
        .unwrap();
    assert!(!host.ran("wg set"));
}

#[tokio::test]
async fn wireguard_failing_during_discovery_fails_the_tick() {
    let (_server, port) = candidates_peer().await;
    let unreadable = FakeHost::new().ok("wg show wg1 public-key", "SELFKEY=");
    assert!(
        tick(&unreadable, &peers(), port, TOKEN, &mut HashMap::new())
            .await
            .is_err()
    );

    let stale = dump_of(&[
        hub_line(),
        peer_line(PEER, &format!("{LAN}:51821"), "::1/128", 1_000, 5, 5),
    ]);
    let refusing = FakeHost::new()
        .ok("wg show wg1 public-key", "SELFKEY=")
        .ok("wg show wg1 dump", &stale)
        .fail("wg set", "busy");
    assert!(tick(&refusing, &peers(), port, TOKEN, &mut HashMap::new())
        .await
        .is_err());
}

#[tokio::test]
async fn a_promotion_wireguard_refuses_fails_the_tick() {
    let (_server, port) = candidates_peer().await;
    let answered = dump_of(&[
        hub_line(),
        peer_line(
            PEER,
            &format!("{LAN}:51821"),
            "::dead:1/128",
            now_secs(),
            1,
            1,
        ),
    ]);
    let host = FakeHost::new()
        .ok("wg show wg1 public-key", "SELFKEY=")
        .ok("wg show wg1 dump", &dump_of(&[hub_line()]))
        .ok("wg show wg1 dump", &answered)
        .ok(
            &format!("ip route get {LAN}"),
            "192.168.1.141 dev enp5s0 src 192.168.1.132",
        )
        .ok("wg set", "")
        .fail("wg set", "busy");
    assert!(tick(&host, &peers(), port, TOKEN, &mut HashMap::new())
        .await
        .is_err());
}

fn candidate(addresses: &[&str]) -> Candidates {
    Candidates {
        public_key: PEER.into(),
        listen_port: 51821,
        addresses: addresses.iter().map(|a| a.to_string()).collect(),
    }
}

#[tokio::test]
async fn an_ipv6_candidate_wireguard_will_not_take_is_passed_over() {
    let host = FakeHost::new()
        .ok("ip -6 route get", "2a01:e0a:1::42 dev enp5s0")
        .fail("wg set", "busy");
    assert_eq!(
        probe(&host, &candidate(&["2a01:e0a:1::42"]), "::dead:1/128").await,
        None
    );
    assert!(host.ran("endpoint [2a01:e0a:1::42]:51821"));
}

#[tokio::test(start_paused = true)]
async fn a_candidate_that_never_handshakes_is_given_up_after_the_probe_timeout() {
    let host = FakeHost::new()
        .ok(
            &format!("ip route get {LAN}"),
            "192.168.1.141 dev enp5s0 src 192.168.1.132",
        )
        .ok("wg set", "")
        .ok("wg show wg1 dump", &dump_of(&[hub_line()]));
    assert_eq!(probe(&host, &candidate(&[LAN]), "::dead:1/128").await, None);
    let looks = host
        .calls()
        .iter()
        .filter(|c| c.contains("wg show wg1 dump"))
        .count();
    assert_eq!(looks, 8, "one look per second of the probe timeout");
}

#[tokio::test]
async fn candidates_from_a_peer_that_errs_or_garbles_are_refused() {
    use wiremock::{matchers, Mock, ResponseTemplate};
    for answer in [
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_body_string("nope"),
    ] {
        let (server, port) = crate::testkit::peer().await;
        Mock::given(matchers::any())
            .respond_with(answer)
            .mount(&server)
            .await;
        assert!(
            fetch_candidates(crate::testkit::PEER, port, TOKEN, "SELFKEY=")
                .await
                .is_err()
        );
    }
}

fn lan_routes(dump: &str) -> FakeHost {
    FakeHost::new()
        .ok("wg show wg1 dump", dump)
        .ok("ip route get", "192.168.1.141 dev enp5s0 src 192.168.1.132")
}

fn primed(endpoint: &str) -> String {
    dump_of(&[
        hub_line(),
        peer_line(PEER, endpoint, "fd00:cafe::dead:6/128", now_secs(), 1, 1),
    ])
}

#[tokio::test]
async fn a_promotion_wireguard_refuses_fails_the_local_pass() {
    let host = lan_routes(&primed("192.168.1.141:51821")).fail("wg set", "busy");
    assert!(reconcile_local(&host, &mut HashMap::new()).await.is_err());
}

#[tokio::test]
async fn a_primed_peer_without_an_endpoint_or_reached_through_the_tunnel_stays_primed() {
    let host = lan_routes(&primed("(none)")).ok("wg set", "");
    reconcile_local(&host, &mut HashMap::new()).await.unwrap();
    assert!(!host.ran("wg set"));

    let tunnelled = FakeHost::new()
        .ok("wg show wg1 dump", &primed("192.168.1.141:51821"))
        .ok("ip route get", "192.168.1.141 dev wg1 src fd00:cafe::5")
        .ok("wg set", "");
    reconcile_local(&tunnelled, &mut HashMap::new())
        .await
        .unwrap();
    assert!(!tunnelled.ran("wg set"));
}

#[tokio::test]
async fn a_stale_removal_wireguard_refuses_fails_the_local_pass() {
    let stale = dump_of(&[
        hub_line(),
        peer_line(PEER, "192.168.1.141:51821", "fd00:cafe::6/128", 1_000, 5, 5),
    ]);
    let host = lan_routes(&stale).fail("wg set", "busy");
    assert!(reconcile_local(&host, &mut HashMap::new()).await.is_err());
}

#[tokio::test]
async fn a_direct_path_that_only_sends_falls_back_to_the_relay() {
    let sending = dump_of(&[
        hub_line(),
        peer_line(
            PEER,
            "192.168.1.141:51821",
            "fd00:cafe::6/128",
            now_secs(),
            900,
            310_000,
        ),
    ]);
    let host = lan_routes(&sending).ok("wg set", "");
    let mut bh = HashMap::from([(
        PEER.to_string(),
        Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        },
    )]);
    reconcile_local(&host, &mut bh).await.unwrap();
    assert!(host.ran("wg set wg1 peer PEERKEY= remove"));
    assert_eq!(bh, HashMap::new());
}

#[tokio::test]
async fn a_caller_is_told_this_machines_key_port_and_addresses() {
    let host = FakeHost::new()
        .ok("wg show wg1 dump", &dump_of(&[hub_line()]))
        .ok("wg show wg1 public-key", "SELFKEY=")
        .ok("wg show wg1 listen-port", "51821")
        .ok(
            "ip -j addr show",
            r#"[{"ifname":"enp0s31f6","addr_info":[{"local":"192.168.1.50","scope":"global"}]}]"#,
        )
        .fail("wg set", "busy");
    let res = crate::testkit::TestApi::with_kube_and_host(
        crate::k8s::testing::unreachable(),
        host.clone(),
    )
    .with_peer_token()
    .with_header(MESH_PUBKEY_HEADER, PEER)
    .get("/api/cluster/mesh-candidates")
    .await;
    assert_eq!(res.status, axum::http::StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.json(),
        serde_json::json!({
            "public_key": "SELFKEY=",
            "listen_port": 51821,
            "addresses": ["192.168.1.50"],
        })
    );
    assert!(host.ran("wg set wg1 peer PEERKEY= allowed-ips fd00:cafe::dead:9/128"));
}

async fn candidates_from(host: FakeHost) -> axum::http::StatusCode {
    crate::testkit::TestApi::with_kube_and_host(crate::k8s::testing::unreachable(), host)
        .with_peer_token()
        .get("/api/cluster/mesh-candidates")
        .await
        .status
}

#[tokio::test]
async fn a_caller_gets_an_error_when_this_machine_cannot_name_its_port_or_addresses() {
    let no_port = FakeHost::new()
        .ok("wg show wg1 public-key", "SELFKEY=")
        .fail("wg show wg1 listen-port", "no such device");
    assert!(!candidates_from(no_port).await.is_success());
    let no_addresses = FakeHost::new()
        .ok("wg show wg1 public-key", "SELFKEY=")
        .ok("wg show wg1 listen-port", "51821")
        .unstartable("ip -j addr show");
    assert!(!candidates_from(no_addresses).await.is_success());
}

#[tokio::test]
async fn a_blackholed_path_wireguard_will_not_drop_fails_the_local_pass() {
    let sending = dump_of(&[
        hub_line(),
        peer_line(
            PEER,
            "192.168.1.141:51821",
            "fd00:cafe::6/128",
            now_secs(),
            900,
            310_000,
        ),
    ]);
    let host = lan_routes(&sending).fail("wg set", "busy");
    let mut bh = HashMap::from([(
        PEER.to_string(),
        Traffic {
            rx_bytes: 900,
            tx_bytes: 1_000,
            at: 0,
        },
    )]);
    assert!(reconcile_local(&host, &mut bh).await.is_err());
}
