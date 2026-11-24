use std::net::Ipv6Addr;

use anyhow::{anyhow, Context};
use serde::Serialize;

use crate::app::AppEvent;
use crate::seam::{Events, Machine, Method, Request};

pub const PLATFORM_API: &str = "https://api.yolab.io";
pub const WG_CONF: &str = "/etc/wireguard/wg0.conf";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct TunnelResult {
    pub enabled: bool,
    pub platform_api_url: String,
    pub account_token: String,
    pub tunnel_id: String,
    pub wg_private_key: String,
    pub wg_public_key: String,
    pub sub_ipv6: String,
    pub dns_url: String,
    pub wg_server_endpoint: String,
    pub wg_server_public_key: String,
    pub node_id: String,
    pub node_wg_private_key: String,
    pub node_wg_public_key: String,
    pub sub_ipv6_private: String,
    pub sub_ipv6_private_subnet: String,
    pub node_wg_server_endpoint: String,
    pub node_wg_server_public_key: String,
}

pub async fn generate_wg_keypair<M: Machine>(m: &M) -> anyhow::Result<(String, String)> {
    let priv_out = m.run("wg", &["genkey"], None).await.context("wg genkey")?;
    anyhow::ensure!(priv_out.success, "wg genkey failed");
    let private_key = String::from_utf8(priv_out.stdout)?.trim().to_string();

    let pub_out = m
        .run("wg", &["pubkey"], Some(&format!("{private_key}\n")))
        .await
        .context("wg pubkey")?;
    anyhow::ensure!(pub_out.success, "wg pubkey failed");
    let public_key = String::from_utf8(pub_out.stdout)?.trim().to_string();

    Ok((private_key, public_key))
}

fn mask_to_112(addr: &str) -> anyhow::Result<String> {
    let ip: Ipv6Addr = addr.parse().map_err(|e| anyhow!("bad ipv6 {addr}: {e}"))?;
    let mut octs = ip.octets();
    octs[14] = 0;
    octs[15] = 0;
    Ok(format!("{}/112", Ipv6Addr::from(octs)))
}

async fn platform<M: Machine>(
    m: &M,
    req: Request,
    what: &str,
) -> anyhow::Result<serde_json::Value> {
    let reply = m.http(req).await.with_context(|| what.to_string())?;
    anyhow::ensure!(
        reply.ok(),
        "{what} returned HTTP {}: {}",
        reply.status,
        reply.text
    );
    serde_json::from_str(&reply.text)
        .with_context(|| format!("{what} sent a reply that is not JSON"))
}

pub async fn next_node_name<M: Machine>(m: &M, account_token: &str) -> anyhow::Result<String> {
    let resp = platform(
        m,
        Request::new(Method::Get, format!("{PLATFORM_API}/tunnels")).bearer(account_token),
        "GET /tunnels",
    )
    .await?;
    Ok(next_node_name_from(&resp))
}

fn next_node_name_from(resp: &serde_json::Value) -> String {
    let re = regex::Regex::new(r"^node(\d+)$").unwrap();
    let max_n = resp
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["dns_records"].as_array())
        .flatten()
        .filter_map(|r| r["name"].as_str())
        .filter_map(|name| re.captures(name)?[1].parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("node{}", max_n + 1)
}

fn text(v: &serde_json::Value, key: &str, what: &str) -> anyhow::Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing {key} in {what} response: {v}"))
}

fn number(v: &serde_json::Value, key: &str, what: &str) -> anyhow::Result<String> {
    v[key]
        .as_u64()
        .map(|n| n.to_string())
        .ok_or_else(|| anyhow!("missing {key} in {what} response: {v}"))
}

fn wg_conf(tunnel_priv: &str, sub_ipv6: &str, server_pub: &str, endpoint: &str) -> String {
    format!(
        "[Interface]\n\
         PrivateKey = {tunnel_priv}\n\
         Address = {sub_ipv6}/128\n\
         Table = off\n\
         PostUp = ip -6 rule add from {sub_ipv6} lookup 51820 priority 100; \
                  ip -6 route add ::/0 dev wg0 table 51820\n\
         PreDown = ip -6 rule del from {sub_ipv6} lookup 51820 priority 100; \
                   ip -6 route del ::/0 dev wg0 table 51820\n\
         \n\
         [Peer]\n\
         PublicKey = {server_pub}\n\
         Endpoint = {endpoint}\n\
         AllowedIPs = ::/0\n\
         PersistentKeepalive = 25\n"
    )
}

pub async fn register_and_bring_up_tunnel<M: Machine>(
    m: &M,
    account_token: &str,
    service_name: &str,
) -> anyhow::Result<TunnelResult> {
    let (tunnel_priv, tunnel_pub) = generate_wg_keypair(m).await?;
    let (node_priv, node_pub) = generate_wg_keypair(m).await?;

    let post = |path: &str, body: serde_json::Value| {
        Request::new(Method::Post, format!("{PLATFORM_API}{path}"))
            .bearer(account_token)
            .json(body)
    };

    let t = platform(
        m,
        post(
            "/tunnels",
            serde_json::json!({ "wg_public_key": tunnel_pub }),
        ),
        "POST /tunnels",
    )
    .await?;
    let tunnel_id = number(&t, "tunnel_id", "tunnel")?;
    let sub_ipv6 = text(&t, "sub_ipv6", "tunnel")?;
    let wg_server_endpoint = text(&t, "wg_server_endpoint", "tunnel")?;
    let wg_server_public_key = text(&t, "wg_server_public_key", "tunnel")?;

    let r = platform(
        m,
        post(
            &format!("/tunnels/{tunnel_id}/records"),
            serde_json::json!({ "record_type": "AAAA", "name": service_name, "value": sub_ipv6 }),
        ),
        "POST /tunnels/{id}/records",
    )
    .await?;
    let dns_url = format!("https://{}", text(&r, "fqdn", "record")?);

    let n = platform(
        m,
        post("/nodes", serde_json::json!({ "wg_public_key": node_pub })),
        "POST /nodes",
    )
    .await?;
    let sub_ipv6_private = text(&n, "sub_ipv6", "node")?;
    let node_id = number(&n, "node_id", "node")?;
    let node_wg_server_endpoint = text(&n, "wg_server_endpoint", "node")?;
    let node_wg_server_public_key = text(&n, "wg_server_public_key", "node")?;
    let sub_ipv6_private_subnet = mask_to_112(&sub_ipv6_private)?;

    m.create_dir_all("/etc/wireguard").await?;
    m.write_file(
        WG_CONF,
        wg_conf(
            &tunnel_priv,
            &sub_ipv6,
            &wg_server_public_key,
            &wg_server_endpoint,
        )
        .as_bytes(),
        Some(0o600),
    )
    .await?;

    let up = m
        .run("wg-quick", &["up", "wg0"], None)
        .await
        .context("wg-quick up")?;
    anyhow::ensure!(up.success, "wg-quick up failed: {}", up.stderr);

    Ok(TunnelResult {
        enabled: true,
        platform_api_url: PLATFORM_API.to_string(),
        account_token: account_token.to_string(),
        tunnel_id,
        wg_private_key: tunnel_priv,
        wg_public_key: tunnel_pub,
        sub_ipv6,
        dns_url,
        wg_server_endpoint,
        wg_server_public_key,
        node_id,
        node_wg_private_key: node_priv,
        node_wg_public_key: node_pub,
        sub_ipv6_private,
        sub_ipv6_private_subnet,
        node_wg_server_endpoint,
        node_wg_server_public_key,
    })
}

pub async fn deregister<M: Machine>(
    m: &M,
    account_token: &str,
    tunnel: &TunnelResult,
    tx: &Events,
) {
    for (what, path, id) in [
        ("node", "nodes", &tunnel.node_id),
        ("tunnel", "tunnels", &tunnel.tunnel_id),
    ] {
        let req = Request::new(Method::Delete, format!("{PLATFORM_API}/{path}/{id}"))
            .bearer(account_token);
        let problem = match m.http(req).await {
            Ok(r) if r.ok() => None,
            Ok(r) => Some(format!("HTTP {}", r.status)),
            Err(e) => Some(e.to_string()),
        };
        if let Some(p) = problem {
            let _ = tx.send(AppEvent::Log(format!(
                "cleanup: failed to deregister {what} {id}: {p}"
            )));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masking_to_112_zeroes_the_final_two_octets() {
        assert_eq!(
            mask_to_112("2001:db8:1234:5678:9abc:def0:1234:5678").unwrap(),
            "2001:db8:1234:5678:9abc:def0:1234:0/112"
        );
    }

    #[test]
    fn masking_an_address_already_on_the_boundary_is_idempotent() {
        let once = mask_to_112("fd00:42:1::0").unwrap();
        assert_eq!(once, "fd00:42:1::/112");
        assert_eq!(mask_to_112("fd00:42:1::").unwrap(), once);
    }

    #[test]
    fn masking_preserves_every_octet_above_the_prefix() {
        assert_eq!(mask_to_112("fd00::ffff").unwrap(), "fd00::/112");
        assert_eq!(mask_to_112("fd00::1:ffff").unwrap(), "fd00::1:0/112");
    }

    #[test]
    fn masking_rejects_anything_that_is_not_an_ipv6_address() {
        assert!(mask_to_112("").is_err());
        assert!(mask_to_112("192.168.1.1").is_err());
        assert!(mask_to_112("not-an-address").is_err());
        assert!(mask_to_112("fd00::1/64").is_err());
        assert!(mask_to_112("fd00::gggg").is_err());
    }

    #[test]
    fn masking_errors_name_the_offending_address() {
        let err = mask_to_112("nonsense").unwrap_err().to_string();
        assert!(err.contains("nonsense"), "got: {err}");
    }

    fn tunnels(names: &[&[&str]]) -> serde_json::Value {
        json!(names
            .iter()
            .map(|records| json!({
                "dns_records": records.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()
            }))
            .collect::<Vec<_>>())
    }

    #[test]
    fn the_first_machine_on_an_account_becomes_node1() {
        assert_eq!(next_node_name_from(&json!([])), "node1");
    }

    #[test]
    fn the_next_name_follows_the_highest_existing_node() {
        assert_eq!(
            next_node_name_from(&tunnels(&[&["node1"], &["node2"]])),
            "node3"
        );
    }

    #[test]
    fn a_gap_in_the_sequence_is_not_reused() {
        assert_eq!(
            next_node_name_from(&tunnels(&[&["node1"], &["node3"]])),
            "node4"
        );
    }

    #[test]
    fn names_that_are_not_nodes_are_ignored() {
        let resp = tunnels(&[&["gitea", "node1", "www", "node-2", "node2x", "NODE9"]]);
        assert_eq!(next_node_name_from(&resp), "node2");
    }

    #[test]
    fn several_records_on_one_tunnel_are_all_considered() {
        assert_eq!(
            next_node_name_from(&tunnels(&[&["node1", "node7", "node3"]])),
            "node8"
        );
    }

    #[test]
    fn double_digit_node_names_are_compared_numerically() {
        assert_eq!(
            next_node_name_from(&tunnels(&[&["node9"], &["node10"]])),
            "node11"
        );
    }

    #[test]
    fn a_malformed_or_error_response_still_yields_a_usable_name() {
        assert_eq!(
            next_node_name_from(&json!({"detail": "Unauthorized"})),
            "node1"
        );
        assert_eq!(next_node_name_from(&json!(null)), "node1");
        assert_eq!(
            next_node_name_from(&json!([{"dns_records": "nope"}])),
            "node1"
        );
        assert_eq!(next_node_name_from(&json!([{}])), "node1");
        assert_eq!(
            next_node_name_from(&json!([{"dns_records": [{}]}])),
            "node1"
        );
    }

    use crate::fake::{events, logs, FakeMachine};
    use crate::seam::{Output, Reply};
    use tokio::sync::mpsc;

    pub(crate) const API: &str = "https://api.yolab.io";

    pub(crate) fn script_keys(m: &FakeMachine) {
        for out in ["tunnel-priv\n", "tunnel-pub\n", "node-priv\n", "node-pub\n"] {
            m.output("wg", true, out);
        }
    }

    pub(crate) fn script_platform(m: &FakeMachine) {
        m.reply(
            Method::Post,
            &format!("{API}/tunnels"),
            201,
            r#"{"tunnel_id":42,"sub_ipv6":"2001:db8::1","wg_server_endpoint":"1.2.3.4:51820","wg_server_public_key":"server-pub"}"#,
        );
        m.reply(
            Method::Post,
            &format!("{API}/tunnels/42/records"),
            201,
            r#"{"fqdn":"node3.alice.yolab.io"}"#,
        );
        m.reply(
            Method::Post,
            &format!("{API}/nodes"),
            201,
            r#"{"node_id":7,"sub_ipv6":"fd00:42::5","wg_server_endpoint":"1.2.3.4:51821","wg_server_public_key":"node-server-pub"}"#,
        );
    }

    pub(crate) fn script_tunnel(m: &FakeMachine) {
        script_keys(m);
        script_platform(m);
        m.output("wg-quick", true, "");
    }

    #[tokio::test]
    async fn the_next_name_comes_from_the_accounts_tunnels() {
        let m = FakeMachine::default();
        m.reply(
            Method::Get,
            &format!("{API}/tunnels"),
            200,
            &tunnels(&[&["node2"]]).to_string(),
        );
        assert_eq!(next_node_name(&m, "tok").await.unwrap(), "node3");
        assert_eq!(m.state().requests[0].bearer.as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn a_refused_tunnel_list_is_an_error_not_node1() {
        let m = FakeMachine::default();
        m.reply(
            Method::Get,
            &format!("{API}/tunnels"),
            503,
            r#"{"detail":"down"}"#,
        );
        let e = next_node_name(&m, "tok").await.unwrap_err().to_string();
        assert_eq!(e, r#"GET /tunnels returned HTTP 503: {"detail":"down"}"#);
    }

    #[tokio::test]
    async fn a_tunnel_list_that_is_not_json_is_an_error() {
        let m = FakeMachine::default();
        m.reply(Method::Get, &format!("{API}/tunnels"), 200, "<html>");
        let e = next_node_name(&m, "tok").await.unwrap_err().to_string();
        assert_eq!(e, "GET /tunnels sent a reply that is not JSON");
    }

    #[tokio::test]
    async fn an_unreachable_platform_names_the_request_that_failed() {
        let m = FakeMachine::default();
        m.reply_with(
            Method::Get,
            &format!("{API}/tunnels"),
            Err("refused".into()),
        );
        let e = next_node_name(&m, "tok").await.unwrap_err();
        assert_eq!(e.to_string(), "GET /tunnels");
        assert_eq!(e.root_cause().to_string(), "refused");
    }

    #[test]
    fn a_node_number_too_large_to_count_is_ignored() {
        assert_eq!(
            next_node_name_from(&tunnels(&[&["node2", "node99999999999"]])),
            "node3"
        );
    }

    #[tokio::test]
    async fn a_keypair_derives_the_public_key_from_the_private_one() {
        let m = FakeMachine::default();
        m.output("wg", true, "priv\n").output("wg", true, "pub\n");
        assert_eq!(
            generate_wg_keypair(&m).await.unwrap(),
            ("priv".to_string(), "pub".to_string())
        );
        assert_eq!(m.calls(), vec!["run wg genkey", "run wg pubkey"]);
        assert_eq!(
            m.state().stdin,
            vec![("wg".to_string(), "priv\n".to_string())]
        );
    }

    fn not_text() -> Result<Output, String> {
        Ok(Output {
            success: true,
            stdout: vec![0xff, 0xfe],
            stderr: String::new(),
        })
    }

    #[tokio::test]
    async fn every_way_a_keypair_can_fail_is_an_error() {
        let m = FakeMachine::default();
        m.output_with("wg", Err("no wg".into()));
        assert_eq!(
            generate_wg_keypair(&m).await.unwrap_err().to_string(),
            "wg genkey"
        );

        let m = FakeMachine::default();
        m.output("wg", false, "");
        assert_eq!(
            generate_wg_keypair(&m).await.unwrap_err().to_string(),
            "wg genkey failed"
        );

        let m = FakeMachine::default();
        m.output_with("wg", not_text());
        assert!(generate_wg_keypair(&m).await.is_err());

        let m = FakeMachine::default();
        m.output("wg", true, "priv")
            .output_with("wg", Err("gone".into()));
        assert_eq!(
            generate_wg_keypair(&m).await.unwrap_err().to_string(),
            "wg pubkey"
        );

        let m = FakeMachine::default();
        m.output("wg", true, "priv").output("wg", false, "");
        assert_eq!(
            generate_wg_keypair(&m).await.unwrap_err().to_string(),
            "wg pubkey failed"
        );

        let m = FakeMachine::default();
        m.output("wg", true, "priv").output_with("wg", not_text());
        assert!(generate_wg_keypair(&m).await.is_err());
    }

    #[tokio::test]
    async fn registering_records_both_keypairs_the_dns_name_and_the_node() {
        let m = FakeMachine::default();
        script_tunnel(&m);
        let t = register_and_bring_up_tunnel(&m, "tok", "node3")
            .await
            .unwrap();
        assert_eq!(
            t,
            crate::install::tests::tunnel_for("node3.alice.yolab.io", "tok")
        );

        let s = m.state();
        let bodies: Vec<_> = s.requests.iter().map(|r| r.json.clone().unwrap()).collect();
        assert_eq!(
            bodies,
            vec![
                json!({"wg_public_key": "tunnel-pub"}),
                json!({"record_type": "AAAA", "name": "node3", "value": "2001:db8::1"}),
                json!({"wg_public_key": "node-pub"}),
            ]
        );
        assert!(s
            .requests
            .iter()
            .all(|r| r.bearer.as_deref() == Some("tok")));
        let (conf, mode) = s.files.get(WG_CONF).unwrap().clone();
        assert_eq!(mode, Some(0o600));
        assert_eq!(
            String::from_utf8(conf).unwrap(),
            wg_conf("tunnel-priv", "2001:db8::1", "server-pub", "1.2.3.4:51820")
        );
        assert!(s.dirs.contains("/etc/wireguard"));
        assert_eq!(s.calls.last().unwrap(), "run wg-quick up wg0");
    }

    #[test]
    fn the_tunnel_routes_only_its_own_address_through_wireguard() {
        let conf = wg_conf("PRIV", "2001:db8::1", "SPUB", "1.2.3.4:51820");
        assert_eq!(
            conf,
            "[Interface]\nPrivateKey = PRIV\nAddress = 2001:db8::1/128\nTable = off\n\
             PostUp = ip -6 rule add from 2001:db8::1 lookup 51820 priority 100; ip -6 route add ::/0 dev wg0 table 51820\n\
             PreDown = ip -6 rule del from 2001:db8::1 lookup 51820 priority 100; ip -6 route del ::/0 dev wg0 table 51820\n\
             \n[Peer]\nPublicKey = SPUB\nEndpoint = 1.2.3.4:51820\nAllowedIPs = ::/0\nPersistentKeepalive = 25\n"
        );
    }

    async fn register_error(m: &FakeMachine) -> String {
        format!(
            "{:#}",
            register_and_bring_up_tunnel(m, "tok", "node3")
                .await
                .unwrap_err()
        )
    }

    fn drop_field(m: &FakeMachine, url: &str, field: &str) {
        let k = crate::fake::key(Method::Post, url);
        let mut s = m.state();
        let reply = s
            .replies
            .get_mut(&k)
            .unwrap()
            .front_mut()
            .unwrap()
            .as_mut()
            .unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&reply.text).unwrap();
        v.as_object_mut().unwrap().remove(field);
        reply.text = v.to_string();
    }

    #[tokio::test]
    async fn a_platform_reply_missing_any_field_names_the_field() {
        let cases = [
            ("/tunnels", "tunnel_id"),
            ("/tunnels", "sub_ipv6"),
            ("/tunnels", "wg_server_endpoint"),
            ("/tunnels", "wg_server_public_key"),
            ("/tunnels/42/records", "fqdn"),
            ("/nodes", "sub_ipv6"),
            ("/nodes", "node_id"),
            ("/nodes", "wg_server_endpoint"),
            ("/nodes", "wg_server_public_key"),
        ];
        for (path, field) in cases {
            let m = FakeMachine::default();
            script_tunnel(&m);
            drop_field(&m, &format!("{API}{path}"), field);
            let e = register_error(&m).await;
            assert!(
                e.starts_with(&format!("missing {field} in ")),
                "{path} {field}: {e}"
            );
            assert!(!m.ran("wg-quick"), "{path} {field}");
        }
    }

    #[tokio::test]
    async fn a_node_address_that_is_not_ipv6_is_refused_before_the_tunnel_comes_up() {
        let m = FakeMachine::default();
        script_keys(&m);
        script_platform(&m);
        let k = crate::fake::key(Method::Post, &format!("{API}/nodes"));
        m.state().replies.insert(
            k,
            [Ok(Reply {
                status: 201,
                text: r#"{"node_id":7,"sub_ipv6":"10.0.0.1","wg_server_endpoint":"e","wg_server_public_key":"k"}"#.into(),
                set_cookie: None,
            })]
            .into(),
        );
        assert!(register_error(&m).await.starts_with("bad ipv6 10.0.0.1"));
        assert!(!m.state().files.contains_key(WG_CONF));
    }

    #[tokio::test]
    async fn the_tunnel_is_not_brought_up_when_its_config_cannot_be_written() {
        for broken in ["/etc/wireguard", WG_CONF] {
            let m = FakeMachine::default();
            script_tunnel(&m);
            m.state().broken_paths.insert(broken.to_string());
            assert_eq!(register_error(&m).await, format!("{broken} is broken"));
            assert!(!m.ran("wg-quick"));
        }
    }

    #[tokio::test]
    async fn a_failing_wg_quick_is_reported_with_its_errors() {
        let m = FakeMachine::default();
        script_keys(&m);
        script_platform(&m);
        m.output("wg-quick", false, "");
        assert_eq!(
            register_error(&m).await,
            "wg-quick up failed: wg-quick broke"
        );

        let m = FakeMachine::default();
        script_keys(&m);
        script_platform(&m);
        m.output_with("wg-quick", Err("missing".into()));
        assert_eq!(register_error(&m).await, "wg-quick up: missing");
    }

    #[tokio::test]
    async fn registration_stops_at_the_first_keypair_that_fails() {
        let m = FakeMachine::default();
        m.output("wg", false, "");
        assert_eq!(register_error(&m).await, "wg genkey failed");

        let m = FakeMachine::default();
        m.output("wg", true, "a")
            .output("wg", true, "b")
            .output("wg", false, "");
        assert_eq!(register_error(&m).await, "wg genkey failed");
        assert!(m.state().requests.is_empty());
    }

    #[tokio::test]
    async fn registration_stops_at_the_first_platform_call_that_fails() {
        for (i, path) in ["/tunnels", "/tunnels/42/records", "/nodes"]
            .iter()
            .enumerate()
        {
            let m = FakeMachine::default();
            script_keys(&m);
            script_platform(&m);
            let k = crate::fake::key(Method::Post, &format!("{API}{path}"));
            m.state()
                .replies
                .insert(k, [Err("refused".to_string())].into());
            let e = register_error(&m).await;
            assert!(e.ends_with(": refused"), "{e}");
            assert_eq!(m.state().requests.len(), i + 1);
        }
    }

    async fn deregister_logs(m: &FakeMachine) -> Vec<String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        deregister(m, "tok", &crate::install::tests::tunnel(), &tx).await;
        logs(&events(&mut rx))
    }

    #[tokio::test]
    async fn deregistering_deletes_the_node_then_the_tunnel_quietly() {
        let m = FakeMachine::default();
        m.reply(Method::Delete, &format!("{API}/nodes/7"), 204, "");
        m.reply(Method::Delete, &format!("{API}/tunnels/42"), 204, "");
        assert!(deregister_logs(&m).await.is_empty());
        assert_eq!(
            m.calls(),
            vec![
                format!("http Delete {API}/nodes/7"),
                format!("http Delete {API}/tunnels/42"),
            ]
        );
        assert!(m
            .state()
            .requests
            .iter()
            .all(|r| r.bearer.as_deref() == Some("tok")));
    }

    #[tokio::test]
    async fn a_failed_cleanup_is_logged_and_the_other_cleanup_still_runs() {
        let m = FakeMachine::default();
        m.reply(Method::Delete, &format!("{API}/nodes/7"), 500, "");
        m.reply_with(
            Method::Delete,
            &format!("{API}/tunnels/42"),
            Err("refused".into()),
        );
        assert_eq!(
            deregister_logs(&m).await,
            vec![
                "cleanup: failed to deregister node 7: HTTP 500",
                "cleanup: failed to deregister tunnel 42: refused",
            ]
        );
    }
}
