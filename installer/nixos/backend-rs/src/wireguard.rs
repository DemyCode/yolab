use std::net::Ipv6Addr;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

pub const PLATFORM_API: &str = "https://api.demycode.ovh";

#[derive(Debug, Serialize, Clone)]
pub struct TunnelResult {
    // Tunnel WireGuard (wg0) — public IP, DNS, Caddy
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
    // Node WireGuard (wg1) — private cluster IP, K3s, inter-node mesh
    pub node_id: String,
    pub node_wg_private_key: String,
    pub node_wg_public_key: String,
    pub sub_ipv6_private: String,
    pub sub_ipv6_private_subnet: String,
    pub node_wg_server_endpoint: String,
    pub node_wg_server_public_key: String,
}

pub async fn generate_wg_keypair() -> anyhow::Result<(String, String)> {
    let priv_out = tokio::process::Command::new("wg")
        .arg("genkey")
        .output()
        .await
        .context("wg genkey")?;
    anyhow::ensure!(priv_out.status.success(), "wg genkey failed");
    let private_key = String::from_utf8(priv_out.stdout)?.trim().to_string();

    let mut child = tokio::process::Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("wg pubkey")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(private_key.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
    }

    let pub_out = child.wait_with_output().await?;
    anyhow::ensure!(pub_out.status.success(), "wg pubkey failed");
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

pub async fn next_node_name(account_token: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{PLATFORM_API}/tunnels"))
        .bearer_auth(account_token)
        .send()
        .await
        .context("GET /tunnels")?
        .json::<serde_json::Value>()
        .await?;
    Ok(next_node_name_from(&resp))
}

/// Picks the next free `nodeN` name from the platform's tunnel listing.
///
/// Split from the HTTP call so the naming rule can be tested. The name becomes
/// the machine's hostname and its DNS record, so a collision means two machines
/// fighting over one name.
fn next_node_name_from(resp: &serde_json::Value) -> String {
    let re = regex::Regex::new(r"^node(\d+)$").unwrap();
    let mut max_n: u32 = 0;
    if let Some(tunnels) = resp.as_array() {
        for t in tunnels {
            if let Some(records) = t["dns_records"].as_array() {
                for r in records {
                    if let Some(name) = r["name"].as_str() {
                        if let Some(caps) = re.captures(name) {
                            if let Ok(n) = caps[1].parse::<u32>() {
                                max_n = max_n.max(n);
                            }
                        }
                    }
                }
            }
        }
    }
    format!("node{}", max_n + 1)
}

pub async fn register_and_bring_up_tunnel(
    account_token: &str,
    service_name: &str,
) -> anyhow::Result<TunnelResult> {
    // Generate independent keypairs for the public tunnel and the private node mesh.
    let (tunnel_priv, tunnel_pub) = generate_wg_keypair().await?;
    let (node_priv, node_pub) = generate_wg_keypair().await?;

    let client = reqwest::Client::new();
    let auth = format!("Bearer {account_token}");

    // Step 1: create tunnel (public IP, DNS)
    let tunnel_resp = client
        .post(format!("{PLATFORM_API}/tunnels"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({ "wg_public_key": tunnel_pub }))
        .send()
        .await
        .context("POST /tunnels")?
        .json::<serde_json::Value>()
        .await?;

    let tunnel_id = tunnel_resp["tunnel_id"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing tunnel_id in response: {tunnel_resp}"))?
        .to_string();
    let sub_ipv6 = tunnel_resp["sub_ipv6"]
        .as_str()
        .ok_or_else(|| anyhow!("missing sub_ipv6"))?
        .to_string();
    let wg_server_endpoint = tunnel_resp["wg_server_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow!("missing wg_server_endpoint"))?
        .to_string();
    let wg_server_public_key = tunnel_resp["wg_server_public_key"]
        .as_str()
        .ok_or_else(|| anyhow!("missing wg_server_public_key"))?
        .to_string();

    // Step 2: attach DNS record
    let record_resp = client
        .post(format!("{PLATFORM_API}/tunnels/{tunnel_id}/records"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({
            "record_type": "AAAA",
            "name": service_name,
            "value": sub_ipv6,
        }))
        .send()
        .await
        .context("POST /tunnels/{id}/records")?
        .json::<serde_json::Value>()
        .await?;

    let fqdn = record_resp["fqdn"]
        .as_str()
        .ok_or_else(|| anyhow!("missing fqdn in record response"))?;
    let dns_url = format!("https://{fqdn}");

    // Step 3: register node peer with its own keypair (private cluster IP, K3s mesh)
    let node_resp = client
        .post(format!("{PLATFORM_API}/nodes"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({ "wg_public_key": node_pub }))
        .send()
        .await
        .context("POST /nodes")?
        .json::<serde_json::Value>()
        .await?;

    let sub_ipv6_private = node_resp["sub_ipv6"]
        .as_str()
        .ok_or_else(|| anyhow!("missing sub_ipv6 in node response"))?
        .to_string();
    let node_id = node_resp["node_id"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing node_id in response: {node_resp}"))?
        .to_string();
    let node_wg_server_endpoint = node_resp["wg_server_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow!("missing wg_server_endpoint in node response"))?
        .to_string();
    let node_wg_server_public_key = node_resp["wg_server_public_key"]
        .as_str()
        .ok_or_else(|| anyhow!("missing wg_server_public_key in node response"))?
        .to_string();
    let sub_ipv6_private_subnet = mask_to_112(&sub_ipv6_private)?;

    // Step 4: write wg0.conf (tunnel only) and bring up the public interface.
    // wg1 (node mesh) is configured by NixOS at boot via networking.wireguard.interfaces.
    let conf = format!(
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
         PublicKey = {wg_server_public_key}\n\
         Endpoint = {wg_server_endpoint}\n\
         AllowedIPs = ::/0\n\
         PersistentKeepalive = 25\n"
    );

    tokio::fs::create_dir_all("/etc/wireguard").await?;
    let conf_path = "/etc/wireguard/wg0.conf";
    tokio::fs::write(conf_path, &conf).await?;
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o600)).await?;

    let wg_out = tokio::process::Command::new("wg-quick")
        .args(["up", "wg0"])
        .output()
        .await
        .context("wg-quick up")?;
    if !wg_out.status.success() {
        bail!(
            "wg-quick up failed: {}",
            String::from_utf8_lossy(&wg_out.stderr)
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── mask_to_112 ───────────────────────────────────────────────────────────

    /// The tunnel hands out a single /128 address; the node needs the /112 it
    /// sits in. Zeroing the last two octets is what turns one into the other.
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
        // Feed the network part back in — it must not shift again.
        assert_eq!(mask_to_112("fd00:42:1::").unwrap(), once);
    }

    #[test]
    fn masking_preserves_every_octet_above_the_prefix() {
        assert_eq!(mask_to_112("fd00::ffff").unwrap(), "fd00::/112");
        assert_eq!(mask_to_112("fd00::1:ffff").unwrap(), "fd00::1:0/112");
    }

    /// A silently-wrong prefix would misroute the whole cluster mesh, so bad
    /// input has to fail loudly rather than default to something plausible.
    #[test]
    fn masking_rejects_anything_that_is_not_an_ipv6_address() {
        assert!(mask_to_112("").is_err());
        assert!(mask_to_112("192.168.1.1").is_err());
        assert!(mask_to_112("not-an-address").is_err());
        assert!(mask_to_112("fd00::1/64").is_err()); // already has a prefix
        assert!(mask_to_112("fd00::gggg").is_err());
    }

    #[test]
    fn masking_errors_name_the_offending_address() {
        let err = mask_to_112("nonsense").unwrap_err().to_string();
        assert!(err.contains("nonsense"), "got: {err}");
    }

    // ── next_node_name_from ───────────────────────────────────────────────────

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
        assert_eq!(next_node_name_from(&tunnels(&[&["node1"], &["node2"]])), "node3");
    }

    /// The hostname must be free, not merely next in sequence: after node2 is
    /// removed, reusing "node2" would collide with its leftover DNS record and
    /// with any cluster state still referring to it.
    #[test]
    fn a_gap_in_the_sequence_is_not_reused() {
        assert_eq!(next_node_name_from(&tunnels(&[&["node1"], &["node3"]])), "node4");
    }

    #[test]
    fn names_that_are_not_nodes_are_ignored() {
        let resp = tunnels(&[&["gitea", "node1", "www", "node-2", "node2x", "NODE9"]]);
        assert_eq!(next_node_name_from(&resp), "node2");
    }

    #[test]
    fn several_records_on_one_tunnel_are_all_considered() {
        assert_eq!(next_node_name_from(&tunnels(&[&["node1", "node7", "node3"]])), "node8");
    }

    #[test]
    fn double_digit_node_names_are_compared_numerically() {
        // Lexical comparison would rank "node9" above "node10" and hand out a
        // name that is already taken.
        assert_eq!(next_node_name_from(&tunnels(&[&["node9"], &["node10"]])), "node11");
    }

    #[test]
    fn a_malformed_or_error_response_still_yields_a_usable_name() {
        assert_eq!(next_node_name_from(&json!({"detail": "Unauthorized"})), "node1");
        assert_eq!(next_node_name_from(&json!(null)), "node1");
        assert_eq!(next_node_name_from(&json!([{"dns_records": "nope"}])), "node1");
        assert_eq!(next_node_name_from(&json!([{}])), "node1");
        assert_eq!(next_node_name_from(&json!([{"dns_records": [{}]}])), "node1");
    }
}
