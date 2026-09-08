//! The `wg` CLI, bounded and parsed.
//!
//! Every call goes through `wg()` with a timeout and `kill_on_drop`, matching
//! ceph_cli.rs: this runs on a reconcile loop, so a wedged invocation must stop
//! being waited on AND stop running, or the next tick starts another.
//!
//! NOTHING HERE EVER LOGS RAW `wg show dump` OUTPUT. Its first line contains the
//! interface's PRIVATE KEY, and a debug log that seemed harmless would put the
//! cluster's mesh key in the journal.

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// The private mesh interface. wg0 is the public tunnel and is never touched
/// here — a direct path between nodes is a cluster concern, and wg0 carries
/// visitor traffic to Caddy.
pub const IFACE: &str = "wg1";

const TIMEOUT_SECS: u64 = 10;

async fn wg(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        Command::new("wg").args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("wg {} timed out after {TIMEOUT_SECS}s", args.join(" ")))?
    .with_context(|| format!("run wg {}", args.join(" ")))?;

    if !out.status.success() {
        // stderr only. stdout may carry key material depending on the subcommand.
        bail!(
            "wg {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// One peer, as `wg show <iface> dump` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    /// Unix seconds of the last completed handshake. 0 means never.
    pub last_handshake: u64,
}

impl Peer {
    /// Whether this peer has handshaked recently enough to be carrying traffic.
    ///
    /// A direct peer that stops answering is the dangerous state in this design:
    /// its narrow allowed-ips still wins longest-prefix match, so packets go into
    /// a hole rather than falling back to the relay. This is the predicate that
    /// catches it.
    pub fn is_alive(&self, now: u64, max_age_secs: u64) -> bool {
        self.last_handshake != 0 && now.saturating_sub(self.last_handshake) <= max_age_secs
    }
}

/// Parses `wg show <iface> dump`.
///
/// Line 1 is the interface (private-key, public-key, listen-port, fwmark) and is
/// skipped — deliberately, since field 0 there is the private key. Every later
/// line is a peer:
///
///   public-key  preshared-key  endpoint  allowed-ips  handshake  rx  tx  keepalive
///
/// Absent values are "(none)" or "off", and allowed-ips is comma-separated.
pub fn parse_dump(dump: &str) -> Vec<Peer> {
    dump.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            let endpoint = match f[2] {
                "(none)" | "" => None,
                e => Some(e.to_string()),
            };
            let allowed_ips = match f[3] {
                "(none)" | "" => vec![],
                a => a.split(',').map(|s| s.trim().to_string()).collect(),
            };
            Some(Peer {
                public_key: f[0].to_string(),
                endpoint,
                allowed_ips,
                last_handshake: f[4].parse().unwrap_or(0),
            })
        })
        .collect()
}

pub async fn peers() -> Result<Vec<Peer>> {
    Ok(parse_dump(&wg(&["show", IFACE, "dump"]).await?))
}

/// This node's own wg1 public key, for peers to authenticate us by.
///
/// Read from the running interface rather than derived from config.toml's
/// private key: if the two ever disagree, the running one is the truth, and a
/// key that does not match the live interface produces a handshake that simply
/// never completes with nothing to say why.
pub async fn self_public_key() -> Result<String> {
    Ok(wg(&["show", IFACE, "public-key"]).await?.trim().to_string())
}

pub async fn listen_port() -> Result<u16> {
    wg(&["show", IFACE, "listen-port"])
        .await?
        .trim()
        .parse()
        .context("parse wg listen-port")
}

/// Adds or updates a peer. `allowed_ips` empty removes all routes to it.
pub async fn set_peer(
    public_key: &str,
    endpoint: &str,
    allowed_ips: &str,
    keepalive_secs: u32,
) -> Result<()> {
    let ka = keepalive_secs.to_string();
    wg(&[
        "set",
        IFACE,
        "peer",
        public_key,
        "endpoint",
        endpoint,
        "allowed-ips",
        allowed_ips,
        "persistent-keepalive",
        &ka,
    ])
    .await?;
    Ok(())
}

/// Drops a peer entirely.
///
/// This is the demotion path, and it is why demotion is safe: removing the
/// specific route leaves the hub's broader allowed-ips as the only match, so the
/// very next packet goes back through the relay with no other change.
pub async fn remove_peer(public_key: &str) -> Result<()> {
    wg(&["set", IFACE, "peer", public_key, "remove"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "PRIVKEYAAA=\tPUBSELF=\t51821\toff\n\
        HUBKEY=\t(none)\t203.0.113.9:51820\tfd00:cafe::/112\t1757000000\t100\t200\t25\n\
        PEERKEY=\t(none)\t192.168.1.50:51821\tfd00:cafe::6/128\t1757000100\t5\t6\t25\n";

    #[test]
    fn skips_the_interface_line_so_the_private_key_is_never_a_peer() {
        let peers = parse_dump(DUMP);
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|p| p.public_key != "PRIVKEYAAA="));
    }

    #[test]
    fn parses_endpoint_allowed_ips_and_handshake() {
        let peers = parse_dump(DUMP);
        let hub = &peers[0];
        assert_eq!(hub.public_key, "HUBKEY=");
        assert_eq!(hub.endpoint.as_deref(), Some("203.0.113.9:51820"));
        assert_eq!(hub.allowed_ips, vec!["fd00:cafe::/112"]);
        assert_eq!(hub.last_handshake, 1_757_000_000);
    }

    #[test]
    fn a_peer_with_no_endpoint_reads_as_none_not_the_literal_string() {
        let peers = parse_dump("iface\nK=\t(none)\t(none)\t(none)\t0\t0\t0\toff\n");
        assert_eq!(peers[0].endpoint, None);
        assert!(peers[0].allowed_ips.is_empty());
        assert_eq!(peers[0].last_handshake, 0);
    }

    #[test]
    fn never_handshaked_is_not_alive_however_generous_the_window() {
        let p = Peer {
            public_key: "K=".into(),
            endpoint: None,
            allowed_ips: vec![],
            last_handshake: 0,
        };
        assert!(!p.is_alive(1_757_000_000, u64::MAX));
    }

    #[test]
    fn liveness_is_bounded_by_the_window() {
        let p = Peer {
            public_key: "K=".into(),
            endpoint: None,
            allowed_ips: vec![],
            last_handshake: 1_000,
        };
        assert!(p.is_alive(1_100, 180));
        assert!(!p.is_alive(1_300, 180));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_read_as_wildly_alive() {
        let p = Peer {
            public_key: "K=".into(),
            endpoint: None,
            allowed_ips: vec![],
            last_handshake: 2_000,
        };
        // saturating_sub, so this is 0 elapsed rather than a huge wrap-around.
        assert!(p.is_alive(1_000, 180));
    }
}
