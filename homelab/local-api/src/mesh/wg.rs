//! The `wg` CLI, bounded and parsed.
//!
//! Every call goes through `Host::run_cmd`, which bounds and `kill_on_drop`s it:
//! this runs on a reconcile loop, so a wedged invocation must stop being waited
//! on AND stop running, or the next tick starts another.
//!
//! GOING THROUGH `Host` IS WHAT MAKES THE MESH TESTABLE. It used to spawn
//! `Command::new("wg")` directly, which meant `reconcile_local` — the function
//! that decides whether a peer carries production traffic — could not be tested
//! at all, only reasoned about. That is precisely the code that took the cluster
//! down on 2026-09-08.
//!
//! NOTHING HERE EVER LOGS RAW `wg show dump` OUTPUT. Its first line contains the
//! interface's PRIVATE KEY, and a debug log that seemed harmless would put the
//! cluster's mesh key in the journal.

use anyhow::{bail, Context, Result};

use crate::host::Host;

/// The private mesh interface. wg0 is the public tunnel and is never touched
/// here — a direct path between nodes is a cluster concern, and wg0 carries
/// visitor traffic to Caddy.
pub const IFACE: &str = "wg1";

async fn wg<H: Host>(host: &H, args: &[&str]) -> Result<String> {
    let out = host
        .run_cmd("wg", args)
        .await
        .with_context(|| format!("run wg {}", args.join(" ")))?;

    if !out.success {
        // stderr only. stdout may carry key material depending on the subcommand.
        bail!("wg {} failed: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout)
}

/// One peer, as `wg show <iface> dump` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    /// Unix seconds of the last completed handshake. 0 means never.
    pub last_handshake: u64,
    /// Bytes RECEIVED from this peer. The blackhole detector reads this and
    /// nothing else, because it is the one counter the kernel only increments
    /// for packets that PASSED the allowed-ips check — see `mod.rs`.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
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
                rx_bytes: f.get(5).and_then(|v| v.parse().ok()).unwrap_or(0),
                tx_bytes: f.get(6).and_then(|v| v.parse().ok()).unwrap_or(0),
            })
        })
        .collect()
}

pub async fn peers<H: Host>(host: &H) -> Result<Vec<Peer>> {
    Ok(parse_dump(&wg(host, &["show", IFACE, "dump"]).await?))
}

/// This node's own wg1 public key, for peers to authenticate us by.
///
/// Read from the running interface rather than derived from config.toml's
/// private key: if the two ever disagree, the running one is the truth, and a
/// key that does not match the live interface produces a handshake that simply
/// never completes with nothing to say why.
pub async fn self_public_key<H: Host>(host: &H) -> Result<String> {
    Ok(wg(host, &["show", IFACE, "public-key"])
        .await?
        .trim()
        .to_string())
}

pub async fn listen_port<H: Host>(host: &H) -> Result<u16> {
    wg(host, &["show", IFACE, "listen-port"])
        .await?
        .trim()
        .parse()
        .context("parse wg listen-port")
}

/// Adds or updates a peer. `allowed_ips` empty removes all routes to it.
///
/// `endpoint` is `None` for the RESPONDER side of the bootstrap handshake: see
/// mod.rs's mesh_candidates handler. WireGuard silently drops an initiation
/// packet from a public key it does not already have configured, so before
/// this node can accept a handshake it must recognise the caller at all —
/// but it has no reason to dial the caller itself, and omitting `endpoint`
/// is exactly the standard listen-only WireGuard peer: WireGuard learns the
/// real source address itself from the first valid packet it receives.
pub async fn set_peer<H: Host>(
    host: &H,
    public_key: &str,
    endpoint: Option<&str>,
    allowed_ips: &str,
    keepalive_secs: u32,
) -> Result<()> {
    let ka = keepalive_secs.to_string();
    let mut args = vec!["set", IFACE, "peer", public_key];
    if let Some(ep) = endpoint {
        args.push("endpoint");
        args.push(ep);
    }
    args.push("allowed-ips");
    args.push(allowed_ips);
    args.push("persistent-keepalive");
    args.push(&ka);
    wg(host, &args).await?;
    Ok(())
}

/// Drops a peer entirely.
///
/// This is the demotion path, and it is why demotion is safe: removing the
/// specific route leaves the hub's broader allowed-ips as the only match, so the
/// very next packet goes back through the relay with no other change.
pub async fn remove_peer<H: Host>(host: &H, public_key: &str) -> Result<()> {
    wg(host, &["set", IFACE, "peer", public_key, "remove"]).await?;
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
            rx_bytes: 0,
            tx_bytes: 0,
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
            rx_bytes: 0,
            tx_bytes: 0,
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
            rx_bytes: 0,
            tx_bytes: 0,
        };
        // saturating_sub, so this is 0 elapsed rather than a huge wrap-around.
        assert!(p.is_alive(1_000, 180));
    }

    #[test]
    fn byte_counters_are_read_from_the_dump() {
        // rx_bytes is what the blackhole detector keys off, and it is field 5.
        // Reading the wrong column would make every peer look permanently
        // silent and demote healthy direct paths on a timer.
        let peers = parse_dump(DUMP);
        assert_eq!(peers[0].rx_bytes, 100);
        assert_eq!(peers[0].tx_bytes, 200);
        assert_eq!(peers[1].rx_bytes, 5);
        assert_eq!(peers[1].tx_bytes, 6);
    }

    #[test]
    fn a_truncated_dump_line_yields_zero_counters_rather_than_panicking() {
        // Five fields is the minimum parse_dump accepts; rx/tx absent must not
        // be fatal, since a zero there is simply "no traffic seen yet".
        let peers = parse_dump("iface\nK=\t(none)\t(none)\tfd00::1/128\t42\n");
        assert_eq!(peers[0].rx_bytes, 0);
        assert_eq!(peers[0].tx_bytes, 0);
    }
}
