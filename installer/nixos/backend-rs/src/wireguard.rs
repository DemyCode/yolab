use std::net::Ipv6Addr;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

pub const PLATFORM_API: &str = "https://api.demycode.ovh";

#[derive(Debug, Serialize, Clone)]
pub struct TunnelResult {
    pub enabled: bool,
    pub platform_api_url: String,
    pub account_token: String,
    pub tunnel_id: String,
    pub node_id: String,
    pub wg_private_key: String,
    pub wg_public_key: String,
    pub sub_ipv6: String,
    pub sub_ipv6_private: String,
    pub sub_ipv6_private_subnet: String,
    pub dns_url: String,
    pub wg_server_endpoint: String,
    pub wg_server_public_key: String,
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
    let re = regex::Regex::new(r"^node(\d+)$").unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{PLATFORM_API}/tunnels"))
        .bearer_auth(account_token)
        .send()
        .await
        .context("GET /tunnels")?
        .json::<serde_json::Value>()
        .await?;

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
    Ok(format!("node{}", max_n + 1))
}

pub async fn register_and_bring_up_tunnel(
    account_token: &str,
    service_name: &str,
) -> anyhow::Result<TunnelResult> {
    let (private_key, public_key) = generate_wg_keypair().await?;

    let client = reqwest::Client::new();
    let auth = format!("Bearer {account_token}");

    // Step 1: create tunnel
    let tunnel_resp = client
        .post(format!("{PLATFORM_API}/tunnels"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({ "wg_public_key": public_key }))
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

    // Step 3: register node (private cluster IP)
    let node_resp = client
        .post(format!("{PLATFORM_API}/nodes"))
        .header("Authorization", &auth)
        .json(&serde_json::json!({ "wg_public_key": public_key }))
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
    let sub_ipv6_private_subnet = mask_to_112(&sub_ipv6_private)?;

    // Step 4: write wg0.conf and bring up tunnel
    let conf = format!(
        "[Interface]\n\
         PrivateKey = {private_key}\n\
         Address = {sub_ipv6}/128, {sub_ipv6_private}/128\n\
         Table = off\n\
         PostUp = ip -6 rule add from {sub_ipv6} lookup 51820 priority 100; \
                  ip -6 route add ::/0 dev wg0 table 51820; \
                  ip -6 route add {sub_ipv6_private_subnet} dev wg0\n\
         PreDown = ip -6 rule del from {sub_ipv6} lookup 51820 priority 100; \
                   ip -6 route del ::/0 dev wg0 table 51820; \
                   ip -6 route del {sub_ipv6_private_subnet} dev wg0\n\
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
    // 0600 permissions
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
        node_id,
        wg_private_key: private_key,
        wg_public_key: public_key,
        sub_ipv6,
        sub_ipv6_private,
        sub_ipv6_private_subnet,
        dns_url,
        wg_server_endpoint,
        wg_server_public_key,
    })
}
