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
    pub ceph_fsid: String,
}

pub async fn nodes() -> Result<Json<Vec<NodeInfo>>> {
    let items = kubectl::get_nodes().await?;
    Ok(Json(
        items
            .iter()
            .map(|item| {
                let meta = &item["metadata"];
                let roles = meta["labels"]
                    .as_object()
                    .map(|l| {
                        l.keys()
                            .filter_map(|k| {
                                k.strip_prefix("node-role.kubernetes.io/").map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let ip = item["status"]["addresses"]
                    .as_array()
                    .and_then(|a| a.iter().find(|a| a["type"] == "InternalIP"))
                    .and_then(|a| a["address"].as_str().map(String::from))
                    .unwrap_or_default();
                let ready = item["status"]["conditions"]
                    .as_array()
                    .map(|cs| {
                        cs.iter()
                            .any(|c| c["type"] == "Ready" && c["status"] == "True")
                    })
                    .unwrap_or(false);
                NodeInfo {
                    name: meta["name"].as_str().unwrap_or("").to_string(),
                    ip,
                    ready,
                    roles,
                    joined_at: meta["creationTimestamp"].as_str().unwrap_or("").to_string(),
                }
            })
            .collect(),
    ))
}

pub async fn node_links(State(state): State<AppState>) -> Result<Json<Vec<NodeLink>>> {
    let tunnel = state
        .config
        .tunnel_table()
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

    let resp = reqwest::Client::new()
        .get(format!("{platform_api_url}/tunnels"))
        .bearer_auth(&account_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let node_re = regex::Regex::new(r"^node\d+$").unwrap();
    let empty = vec![];
    let tunnels = resp.as_array().unwrap_or(&empty);
    let mut links: Vec<NodeLink> = tunnels
        .iter()
        .flat_map(|tunnel| {
            let records = tunnel["dns_records"].as_array().unwrap_or(&empty);
            records
                .iter()
                .filter_map(|r| {
                    let name = r["name"].as_str()?;
                    if !node_re.is_match(name) {
                        return None;
                    }
                    let fqdn = r["fqdn"].as_str()?;
                    Some(NodeLink {
                        name: name.to_string(),
                        url: format!("https://{fqdn}"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    links.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(links))
}

pub async fn join_info(State(state): State<AppState>) -> Result<Json<JoinInfo>> {
    let text = std::fs::read_to_string(&state.config.config_path)?;
    Ok(Json(parse_join_info(&text)?))
}

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
        let j = parse_join_info(&cfg(
            "[ceph]\nfsid = \"11111111-2222-4333-8444-555555555555\"",
        ))
        .unwrap();
        assert_eq!(j.k3s_token, "deadbeef");
        assert_eq!(j.server_addr, "https://[fd00:cafe::2f]:6443");
        assert_eq!(j.account_token, "acct");
        assert_eq!(j.platform_api_url, "https://api.example");
        assert_eq!(j.ceph_fsid, "11111111-2222-4333-8444-555555555555");
    }

    #[test]
    fn the_server_address_brackets_the_ipv6_literal() {
        let j = parse_join_info(&cfg("")).unwrap();
        assert!(
            j.server_addr.starts_with("https://[fd00:cafe::2f]:"),
            "{}",
            j.server_addr
        );
    }

    #[test]
    fn a_config_without_a_ceph_section_reports_an_empty_fsid() {
        let j = parse_join_info(&cfg("")).unwrap();
        assert_eq!(j.ceph_fsid, "");
    }

    #[test]
    fn an_empty_ceph_section_reports_an_empty_fsid() {
        assert_eq!(parse_join_info(&cfg("[ceph]")).unwrap().ceph_fsid, "");
    }

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

    #[test]
    fn a_node_key_of_the_wrong_type_errors() {
        assert!(parse_join_info("node = \"a string\"").is_err());
    }

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
