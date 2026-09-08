//! Which of this node's local addresses a peer could dial directly.
//!
//! The filtering here is the whole safety story of discovery. A node has far
//! more addresses than it has useful ones, and two of the useless kinds are
//! actively dangerous:
//!
//!   - Container bridges (docker0 172.17.0.1, flannel 10.42.x) exist with the
//!     SAME address on every node in the cluster. Offer one as a candidate and
//!     the peer probing it connects to its own bridge, not to us.
//!   - The mesh addresses themselves (fd00:cafe::/112, and wg0's public /128)
//!     are reachable only THROUGH the relay. A "direct" path built on one of
//!     those still pays for every byte while reporting that it does not.
//!
//! So this is an allowlist by interface, not a denylist by address.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::host::Host;

/// Interface name prefixes that are never a direct path to this node.
///
/// wg: the tunnels themselves. flannel/cni/veth/docker/br-/kube/virbr: container
/// networking, which is per-node and frequently identical across nodes. lo: not
/// reachable from anywhere else at all.
const VIRTUAL_PREFIXES: &[&str] = &[
    "lo",
    "wg",
    "flannel",
    "cni",
    "veth",
    "docker",
    "br-",
    "kube",
    "virbr",
    "tailscale",
    "zt",
];

fn is_physical(iface: &str) -> bool {
    !VIRTUAL_PREFIXES.iter().any(|p| iface.starts_with(p))
}

/// Addresses that are never worth offering even on a physical interface.
///
/// Link-local needs a scope identifier to be dialable (`fe80::1%eth0`) and the
/// scope is meaningless to the peer, so it is dropped rather than offered as
/// something that looks valid and never works.
fn is_dialable(addr: &str) -> bool {
    let a = addr.to_ascii_lowercase();
    !(a.starts_with("fe80:")       // IPv6 link-local
        || a.starts_with("169.254.") // IPv4 link-local / APIPA
        || a.starts_with("127.")
        || a == "::1")
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Candidates {
    /// This node's wg1 public key — how a peer authenticates the direct path.
    pub public_key: String,
    pub listen_port: u16,
    /// Bare IPs, no port. The peer pairs each with `listen_port`.
    pub addresses: Vec<String>,
}

/// Extracts dialable addresses from `ip -j addr show`.
///
/// Split from the command so the filtering — the part that matters — is
/// testable against real `ip` output without a host that has these interfaces.
pub fn parse_addrs(json: &str) -> Result<Vec<String>> {
    let links: serde_json::Value = serde_json::from_str(json).context("parse ip -j addr")?;
    let mut out = Vec::new();
    for link in links.as_array().unwrap_or(&vec![]) {
        let name = link["ifname"].as_str().unwrap_or("");
        if !is_physical(name) {
            continue;
        }
        for a in link["addr_info"].as_array().unwrap_or(&vec![]) {
            let Some(local) = a["local"].as_str() else {
                continue;
            };
            // "global" excludes link and host scopes without matching on the
            // address text a second time.
            if a["scope"].as_str() != Some("global") {
                continue;
            }
            if is_dialable(local) && !out.contains(&local.to_string()) {
                out.push(local.to_string());
            }
        }
    }
    Ok(out)
}

/// Through `Host` like everything else in this module, so a caller can be tested
/// without a machine that happens to have the right interfaces — and so the
/// bound comes from one place rather than a second hand-rolled timeout here.
pub async fn local_addresses<H: Host>(host: &H) -> Result<Vec<String>> {
    let out = host
        .run_cmd("ip", &["-j", "addr", "show"])
        .await
        .context("run ip -j addr")?;
    parse_addrs(&out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP_JSON: &str = r#"[
      {"ifname":"lo","addr_info":[{"local":"127.0.0.1","scope":"host"}]},
      {"ifname":"enp0s31f6","addr_info":[
         {"local":"192.168.1.50","scope":"global"},
         {"local":"2a01:e0a:1::42","scope":"global"},
         {"local":"fe80::1","scope":"link"}]},
      {"ifname":"wg1","addr_info":[{"local":"fd00:cafe::5","scope":"global"}]},
      {"ifname":"wg0","addr_info":[{"local":"2a01:4f8:1c1e:43ff::5","scope":"global"}]},
      {"ifname":"docker0","addr_info":[{"local":"172.17.0.1","scope":"global"}]},
      {"ifname":"flannel.1","addr_info":[{"local":"10.42.0.0","scope":"global"}]},
      {"ifname":"cni0","addr_info":[{"local":"10.42.0.1","scope":"global"}]}
    ]"#;

    #[test]
    fn keeps_only_addresses_on_physical_interfaces() {
        let addrs = parse_addrs(IP_JSON).unwrap();
        assert_eq!(addrs, vec!["192.168.1.50", "2a01:e0a:1::42"]);
    }

    #[test]
    fn never_offers_the_mesh_address_which_is_reachable_only_via_the_relay() {
        let addrs = parse_addrs(IP_JSON).unwrap();
        assert!(!addrs.iter().any(|a| a.starts_with("fd00:cafe")));
        assert!(!addrs.iter().any(|a| a.starts_with("2a01:4f8")));
    }

    #[test]
    fn never_offers_a_container_bridge_that_exists_identically_on_every_node() {
        let addrs = parse_addrs(IP_JSON).unwrap();
        for bad in ["172.17.0.1", "10.42.0.0", "10.42.0.1"] {
            assert!(!addrs.contains(&bad.to_string()), "offered {bad}");
        }
    }

    #[test]
    fn drops_link_local_which_cannot_be_dialed_without_a_scope() {
        assert!(!is_dialable("fe80::1"));
        assert!(!is_dialable("169.254.10.1"));
        assert!(is_dialable("192.168.1.50"));
    }

    #[test]
    fn a_real_interface_named_like_a_virtual_one_is_still_excluded() {
        // Conservative on purpose: excluding a usable path costs a relayed
        // connection, including an unusable one costs a blackhole.
        assert!(!is_physical("wgtest0"));
        assert!(is_physical("eth0"));
        assert!(is_physical("wlp3s0"));
    }

    #[test]
    fn empty_input_is_not_an_error() {
        assert_eq!(parse_addrs("[]").unwrap(), Vec::<String>::new());
    }
}
