//! The notification server's DNS name: `ntfy-<machine>` on this machine's own
//! tunnel, pointing at the same address Caddy answers on.
//!
//! Checked before it is written: the platform's record endpoint replaces a
//! record of the same name on the account's OTHER tunnels, not on this one, so
//! posting it again every tick would add duplicates.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::Tunnel;
use crate::runtime::{Controller, Ctx, Scope, Tick};

const NAME: &str = "ntfy-dns";

pub struct NtfyDnsController {
    pub config: crate::config::Config,
}

impl Controller for NtfyDnsController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(600)
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let tunnel = Tunnel::read(&self.config.config_path)?;
        let Some(want) = wanted_record(&tunnel) else {
            return Ok(Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        };
        let client = reqwest::Client::new();
        let tunnels: Value = client
            .get(format!("{}/tunnels", tunnel.platform_api_url))
            .bearer_auth(&tunnel.account_token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("list this account's tunnels")?
            .error_for_status()?
            .json()
            .await?;
        match decide(&tunnels, &tunnel.tunnel_id, &want)? {
            Decision::Present => Ok(Tick::Idle(format!("{} is in place", want.name))),
            Decision::Create => {
                client
                    .post(format!(
                        "{}/tunnels/{}/records",
                        tunnel.platform_api_url, tunnel.tunnel_id
                    ))
                    .bearer_auth(&tunnel.account_token)
                    .timeout(Duration::from_secs(15))
                    .json(&json!({
                        "record_type": "AAAA",
                        "name": want.name,
                        "value": want.value,
                    }))
                    .send()
                    .await
                    .context("create the notification server's DNS record")?
                    .error_for_status()?;
                tracing::info!("notifications: created DNS record {} -> {}", want.name, want.value);
                Ok(Tick::Done)
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct Record {
    name: String,
    value: String,
}

fn wanted_record(tunnel: &Tunnel) -> Option<Record> {
    if !tunnel.enabled || tunnel.tunnel_id.is_empty() || tunnel.sub_ipv6.is_empty() {
        return None;
    }
    Some(Record {
        name: tunnel.ntfy_record_name()?,
        value: tunnel.sub_ipv6.clone(),
    })
}

#[derive(Debug, PartialEq)]
enum Decision {
    Present,
    Create,
}

/// What to do, from `GET /tunnels`.
fn decide(tunnels: &Value, tunnel_id: &str, want: &Record) -> Result<Decision> {
    let list = tunnels.as_array().context("GET /tunnels: not a list")?;
    let Some(ours) = list
        .iter()
        .find(|t| t["tunnel_id"].to_string().trim_matches('"') == tunnel_id)
    else {
        bail!("this machine's tunnel {tunnel_id} is not on the platform");
    };
    let present = ours["dns_records"].as_array().into_iter().flatten().any(|r| {
        r["name"].as_str() == Some(want.name.as_str())
            && r["value"].as_str() == Some(want.value.as_str())
    });
    Ok(if present {
        Decision::Present
    } else {
        Decision::Create
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want() -> Record {
        Record {
            name: "ntfy-node1".into(),
            value: "2a01::20".into(),
        }
    }

    #[test]
    fn a_record_already_on_this_tunnel_is_left_alone() {
        let tunnels = json!([
            {"tunnel_id": 25, "sub_ipv6": "2a01::20", "dns_records": [
                {"name": "node1", "value": "2a01::20"},
                {"name": "ntfy-node1", "value": "2a01::20"},
            ]},
        ]);
        assert_eq!(decide(&tunnels, "25", &want()).unwrap(), Decision::Present);
    }

    #[test]
    fn a_missing_or_stale_record_is_created() {
        let missing = json!([{"tunnel_id": 25, "dns_records": [{"name": "node1", "value": "2a01::20"}]}]);
        assert_eq!(decide(&missing, "25", &want()).unwrap(), Decision::Create);
        let stale = json!([{"tunnel_id": 25, "dns_records": [{"name": "ntfy-node1", "value": "2a01::99"}]}]);
        assert_eq!(decide(&stale, "25", &want()).unwrap(), Decision::Create);
    }

    #[test]
    fn a_tunnel_the_platform_does_not_have_is_an_error() {
        assert!(decide(&json!([{"tunnel_id": 7}]), "25", &want()).is_err());
        assert!(decide(&json!({"oops": 1}), "25", &want()).is_err());
    }

    #[test]
    fn nothing_is_wanted_without_a_platform_connection() {
        let off = Tunnel {
            enabled: false,
            platform_api_url: String::new(),
            account_token: String::new(),
            tunnel_id: "25".into(),
            sub_ipv6: "2a01::20".into(),
            host: "node1.6.yolab.io".into(),
        };
        assert_eq!(wanted_record(&off), None);
        let on = Tunnel { enabled: true, ..off };
        assert_eq!(wanted_record(&on), Some(want()));
    }
}
