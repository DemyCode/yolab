use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct CephJoinBundle {
    pub fsid: String,
    pub mon_keyring: String,
    pub admin_keyring: String,
    pub bootstrap_osd_keyring: String,
    pub mon_addrs: Vec<String>,
}

pub fn parse_mon_addrs(dump: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(mons) = dump["mons"].as_array() else {
        return out;
    };
    for m in mons {
        let addr = m["public_addrs"]["addrvec"]
            .as_array()
            .and_then(|v| v.iter().find_map(|e| e["addr"].as_str()))
            .or_else(|| m["public_addr"].as_str())
            .or_else(|| m["addr"].as_str());
        if let Some(a) = addr.and_then(strip_port) {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

fn strip_port(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.split('/').next().unwrap_or(raw);
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next()?
    } else {
        raw.rsplit_once(':').map(|(h, _)| h).unwrap_or(raw)
    };
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

async fn keyring(entity: &str) -> anyhow::Result<String> {
    let text = crate::ceph_cli::ceph(&["auth", "get", entity]).await?;
    if !text.contains("key =") {
        anyhow::bail!("`ceph auth get {entity}` returned no key");
    }
    Ok(text)
}

pub async fn ceph_join_bundle() -> Result<Json<CephJoinBundle>> {
    let fsid = crate::ceph_cli::cluster_fsid()
        .await
        .map_err(|e| anyhow::anyhow!("ceph is not reachable from this node: {e}"))?;

    let dump = crate::ceph_cli::ceph_json(&["mon", "dump"]).await?;
    let mon_addrs = parse_mon_addrs(&dump);
    if mon_addrs.is_empty() {
        return Err(anyhow::anyhow!("no mon addresses in `ceph mon dump`").into());
    }

    Ok(Json(CephJoinBundle {
        fsid,
        mon_keyring: keyring("mon.").await?,
        admin_keyring: keyring("client.admin").await?,
        bootstrap_osd_keyring: keyring("client.bootstrap-osd").await?,
        mon_addrs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bracketed_ipv6_literal_keeps_its_colons() {
        assert_eq!(
            strip_port("[fd00:cafe::1]:3300").as_deref(),
            Some("fd00:cafe::1")
        );
    }

    #[test]
    fn the_nonce_suffix_is_dropped() {
        assert_eq!(
            strip_port("[fd00:cafe::1]:6789/0").as_deref(),
            Some("fd00:cafe::1")
        );
    }

    #[test]
    fn an_ipv4_address_loses_only_its_port() {
        assert_eq!(strip_port("10.0.0.1:6789").as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn nonsense_yields_nothing_rather_than_a_plausible_wrong_answer() {
        assert_eq!(strip_port(""), None);
        assert_eq!(strip_port("[]:6789"), None);
    }

    fn dump_with(addrs: &[&str]) -> Value {
        json!({
            "mons": addrs.iter().map(|a| json!({
                "name": "n",
                "public_addrs": {"addrvec": [
                    {"type": "v2", "addr": format!("[{a}]:3300"), "nonce": 0},
                    {"type": "v1", "addr": format!("[{a}]:6789"), "nonce": 0},
                ]},
            })).collect::<Vec<_>>()
        })
    }

    #[test]
    fn every_mon_in_the_map_is_returned_once() {
        let got = parse_mon_addrs(&dump_with(&["fd00:cafe::1", "fd00:cafe::2"]));
        assert_eq!(
            got,
            vec!["fd00:cafe::1".to_string(), "fd00:cafe::2".to_string()]
        );
    }

    #[test]
    fn the_v1_and_v2_entries_of_one_mon_collapse_to_a_single_address() {
        let got = parse_mon_addrs(&dump_with(&["fd00:cafe::1"]));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn an_older_dump_without_addrvec_falls_back_to_addr() {
        let v = json!({"mons": [{"name": "n", "addr": "[fd00:cafe::7]:6789/0"}]});
        assert_eq!(parse_mon_addrs(&v), vec!["fd00:cafe::7".to_string()]);
    }

    #[test]
    fn an_unrecognisable_dump_yields_no_addresses() {
        assert!(parse_mon_addrs(&json!({})).is_empty());
        assert!(parse_mon_addrs(&json!({"mons": []})).is_empty());
        assert!(parse_mon_addrs(&json!({"mons": [{"name": "n"}]})).is_empty());
    }
}
