use std::time::Duration;

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{error::Result, kubectl, AppState};

#[derive(Serialize)]
pub struct NodeLink {
    pub name: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub ip: String,
    pub ready: bool,
    pub roles: Vec<String>,
    pub joined_at: String,
}

#[derive(Serialize, Debug)]
pub struct JoinInfo {
    pub k3s_token: String,
    pub server_addr: String,
    pub account_token: String,
    pub platform_api_url: String,
    /// The cluster's Ceph fsid, so a joining node builds against the same
    /// storage cluster rather than bootstrapping a second, isolated one.
    pub ceph_fsid: String,
}

/// Returns an error (not an empty list) when the cluster can't be reached.
///
/// This used to `unwrap_or_default()`, so "kubectl failed" and "there are no nodes"
/// both came back as `200 []` — leaving the UI no way to tell a real answer from a
/// broken one, which it then guessed at by assuming any empty list meant the control
/// plane was down.
pub async fn nodes() -> Result<Json<Vec<NodeInfo>>> {
    let items = kubectl::get_nodes().await?;
    Ok(Json(
        items.iter().map(|item| {
            let meta = &item["metadata"];
            let roles = meta["labels"].as_object().map(|l| {
                l.keys()
                    .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/").map(String::from))
                    .collect()
            }).unwrap_or_default();
            let ip = item["status"]["addresses"].as_array()
                .and_then(|a| a.iter().find(|a| a["type"] == "InternalIP"))
                .and_then(|a| a["address"].as_str().map(String::from))
                .unwrap_or_default();
            let ready = item["status"]["conditions"].as_array()
                .map(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
                .unwrap_or(false);
            NodeInfo {
                name: meta["name"].as_str().unwrap_or("").to_string(),
                ip,
                ready,
                roles,
                joined_at: meta["creationTimestamp"].as_str().unwrap_or("").to_string(),
            }
        }).collect(),
    ))
}

pub async fn node_links(State(state): State<AppState>) -> Result<Json<Vec<NodeLink>>> {
    let text = std::fs::read_to_string(&state.config.config_path)?;
    let table: toml::Table = toml::from_str(&text)?;
    let tunnel = table["tunnel"].as_table()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))?;
    let account_token = tunnel.get("account_token")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();
    let platform_api_url = tunnel.get("platform_api_url")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();

    let resp = reqwest::Client::new()
        .get(format!("{platform_api_url}/tunnels"))
        .bearer_auth(&account_token)
        .send().await?
        .json::<serde_json::Value>().await?;

    let node_re = regex::Regex::new(r"^node\d+$").unwrap();
    let empty = vec![];
    let tunnels = resp.as_array().unwrap_or(&empty);
    let mut links: Vec<NodeLink> = tunnels.iter()
        .flat_map(|tunnel| {
            let records = tunnel["dns_records"].as_array().unwrap_or(&empty);
            records.iter().filter_map(|r| {
                let name = r["name"].as_str()?;
                if !node_re.is_match(name) { return None; }
                let fqdn = r["fqdn"].as_str()?;
                Some(NodeLink { name: name.to_string(), url: format!("https://{fqdn}") })
            }).collect::<Vec<_>>()
        })
        .collect();

    links.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(links))
}

pub async fn traffic(State(state): State<AppState>) -> Json<serde_json::Value> {
    let text = match std::fs::read_to_string(&state.config.config_path) {
        Ok(t) => t,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };
    let table: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };
    let Some(tunnel) = table.get("tunnel").and_then(|v| v.as_table()) else {
        return Json(serde_json::json!({ "error": "missing [tunnel] in config" }));
    };
    let token = tunnel.get("account_token").and_then(|v| v.as_str()).unwrap_or("");
    let base_url = tunnel.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or("");

    let url = format!("{base_url}/nodes/transfer");
    match reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        },
        Ok(r) => Json(serde_json::json!({ "error": format!("backend {}", r.status()) })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

pub async fn join_info(State(state): State<AppState>) -> Result<Json<JoinInfo>> {
    let text = std::fs::read_to_string(&state.config.config_path)?;
    Ok(Json(parse_join_info(&text)?))
}

/// Extract everything a joining node needs from this node's config.toml.
///
/// Split from the handler so it is testable without a filesystem or an
/// AppState. This is how a second machine learns which cluster to join, and two
/// of the four fields cannot be regenerated locally — get either wrong and the
/// new node either fails to join k3s or, worse, silently builds an isolated
/// second Ceph cluster.
///
/// Uses `.get()` throughout. The previous version indexed with `table["node"]`
/// and `node["k3s"]["token"]`, which *panics* on a missing key rather than
/// returning an error — so a hand-edited or truncated config.toml took the
/// request thread down instead of reporting what was wrong.
pub fn parse_join_info(text: &str) -> anyhow::Result<JoinInfo> {
    let table: toml::Table = toml::from_str(text)?;

    let node = table
        .get("node")
        .and_then(|v| v.as_table())
        .ok_or_else(|| anyhow::anyhow!("missing [node] in config"))?;

    let k3s_token = node
        .get("k3s")
        .and_then(|v| v.as_table())
        .and_then(|k| k.get("token"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing node.k3s.token"))?
        .to_string();

    let sub_ipv6_private = node
        .get("sub_ipv6_private")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing node.sub_ipv6_private"))?;

    let tunnel = table
        .get("tunnel")
        .and_then(|v| v.as_table())
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))?;

    let account_token = tunnel
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let platform_api_url = tunnel
        .get("platform_api_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Every node in a Ceph cluster shares one fsid. A joining node cannot
    // generate its own: it is baked into each OSD's BlueStore superblock, and a
    // node built with the wrong value cannot authenticate to the mons at all.
    // So it travels with the k3s token, which has exactly the same property.
    //
    // Empty rather than an error: a node installed before host-level Ceph has
    // no [ceph] section, and the installer refuses an empty value on the other
    // side rather than generating a fresh fsid. Failing here instead would stop
    // such a node from serving join-info at all.
    let ceph_fsid = table
        .get("ceph")
        .and_then(|c| c.as_table())
        .and_then(|c| c.get("fsid"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(JoinInfo {
        k3s_token,
        server_addr: format!("https://[{sub_ipv6_private}]:6443"),
        account_token,
        platform_api_url,
        ceph_fsid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config.toml with only the fields join-info reads.
    fn cfg(extra: &str) -> String {
        format!(
            r#"
[node]
sub_ipv6_private = "fd00:cafe::2f"
[node.k3s]
token = "deadbeef"
[tunnel]
account_token = "acct"
platform_api_url = "https://api.example"
{extra}
"#
        )
    }

    #[test]
    fn a_complete_config_yields_every_field() {
        let j = parse_join_info(&cfg("[ceph]\nfsid = \"11111111-2222-4333-8444-555555555555\"")).unwrap();
        assert_eq!(j.k3s_token, "deadbeef");
        assert_eq!(j.server_addr, "https://[fd00:cafe::2f]:6443");
        assert_eq!(j.account_token, "acct");
        assert_eq!(j.platform_api_url, "https://api.example");
        assert_eq!(j.ceph_fsid, "11111111-2222-4333-8444-555555555555");
    }

    /// The address is IPv6, so it MUST be bracketed. An unbracketed URL parses
    /// as host "fd00" port "cafe", and the joining node fails to reach the API
    /// server with an error that points nowhere near the cause.
    #[test]
    fn the_server_address_brackets_the_ipv6_literal() {
        let j = parse_join_info(&cfg("")).unwrap();
        assert!(j.server_addr.starts_with("https://[fd00:cafe::2f]:"), "{}", j.server_addr);
    }

    /// A node predating host-level Ceph has no [ceph] section. It must still be
    /// able to serve join-info; the installer refuses the empty value rather
    /// than generating a fresh fsid, which is where that case is caught.
    #[test]
    fn a_config_without_a_ceph_section_reports_an_empty_fsid() {
        let j = parse_join_info(&cfg("")).unwrap();
        assert_eq!(j.ceph_fsid, "");
    }

    #[test]
    fn an_empty_ceph_section_reports_an_empty_fsid() {
        assert_eq!(parse_join_info(&cfg("[ceph]")).unwrap().ceph_fsid, "");
    }

    // ── The panics this function used to have ─────────────────────────────────
    //
    // These indexed with table["node"], which panics on a missing key. A
    // truncated or hand-edited config.toml took down the request thread instead
    // of returning a message naming the missing field.

    #[test]
    fn a_config_with_no_node_section_errors_instead_of_panicking() {
        let e = parse_join_info("[tunnel]\naccount_token = \"x\"").unwrap_err();
        assert!(e.to_string().contains("[node]"), "{e}");
    }

    #[test]
    fn a_config_with_no_k3s_token_errors_instead_of_panicking() {
        let bad = r#"
[node]
sub_ipv6_private = "fd00:cafe::2f"
[tunnel]
"#;
        let e = parse_join_info(bad).unwrap_err();
        assert!(e.to_string().contains("node.k3s.token"), "{e}");
    }

    #[test]
    fn a_config_with_no_tunnel_section_errors() {
        let bad = r#"
[node]
sub_ipv6_private = "fd00:cafe::2f"
[node.k3s]
token = "deadbeef"
"#;
        let e = parse_join_info(bad).unwrap_err();
        assert!(e.to_string().contains("[tunnel]"), "{e}");
    }

    #[test]
    fn a_config_with_no_private_address_errors() {
        let bad = r#"
[node]
[node.k3s]
token = "deadbeef"
[tunnel]
"#;
        let e = parse_join_info(bad).unwrap_err();
        assert!(e.to_string().contains("sub_ipv6_private"), "{e}");
    }

    /// An empty token is as useless as a missing one, and would otherwise be
    /// handed to a joining node as though it were valid.
    #[test]
    fn an_empty_token_is_treated_as_missing() {
        let bad = r#"
[node]
sub_ipv6_private = "fd00:cafe::2f"
[node.k3s]
token = ""
[tunnel]
"#;
        assert!(parse_join_info(bad).is_err());
    }

    #[test]
    fn malformed_toml_errors() {
        assert!(parse_join_info("this is not toml {{{").is_err());
    }

    /// Wrong types must not panic either — `as_table` on a string returns None.
    #[test]
    fn a_node_key_of_the_wrong_type_errors() {
        assert!(parse_join_info("node = \"a string\"").is_err());
    }

    /// Optional fields are genuinely optional; absent means empty, not failure.
    #[test]
    fn absent_tunnel_fields_default_to_empty() {
        let bad = r#"
[node]
sub_ipv6_private = "fd00:cafe::2f"
[node.k3s]
token = "deadbeef"
[tunnel]
"#;
        let j = parse_join_info(bad).unwrap();
        assert_eq!(j.account_token, "");
        assert_eq!(j.platform_api_url, "");
    }
}
