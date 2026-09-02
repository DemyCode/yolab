//! Detect the node's outbound IPv4 at boot and write it to k3s's config file
//! as node-ip alongside the private IPv6, enabling dual-stack pods. Runs
//! after WireGuard and before k3s so the IPv6 address is already up.

use std::path::Path;

use anyhow::Result;

use crate::host::Host;

/// `ip -4 route get 1.1.1.1`'s stdout is one line like
/// `"1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0"` — pull out what
/// follows `src `. `None` for a machine with no IPv4 route at all (the
/// IPv6-only case this is written for), never a guessed address.
fn parse_src_ip(route_get_output: &str) -> Option<String> {
    let after = route_get_output.split("src ").nth(1)?;
    after.split_whitespace().next().map(str::to_string)
}

/// The line k3s's config file needs. Dual-stack when an IPv4 route exists,
/// IPv6-only otherwise.
fn node_ip_line(private_ipv6: &str, ipv4: Option<&str>) -> String {
    match ipv4 {
        Some(v4) if !v4.is_empty() => format!("node-ip: {private_ipv6},{v4}\n"),
        _ => format!("node-ip: {private_ipv6}\n"),
    }
}

pub async fn run<H: Host>(host: &H, private_ipv6: &str, root: &Path) -> Result<()> {
    let ipv4 = host
        .run_cmd("ip", &["-4", "route", "get", "1.1.1.1"])
        .await
        .ok()
        .filter(|o| o.success)
        .and_then(|o| parse_src_ip(&o.stdout));

    let dir = root.join("etc/rancher/k3s");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.yaml");
    std::fs::write(&path, node_ip_line(private_ipv6, ipv4.as_deref()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn parse_src_ip_reads_the_field_after_src() {
        assert_eq!(
            parse_src_ip("1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0"),
            Some("10.0.0.42".to_string())
        );
    }

    #[test]
    fn parse_src_ip_is_none_without_a_route() {
        assert_eq!(parse_src_ip(""), None);
        assert_eq!(
            parse_src_ip("RTNETLINK answers: Network is unreachable"),
            None
        );
    }

    #[test]
    fn node_ip_line_is_dual_stack_with_an_ipv4_route() {
        assert_eq!(
            node_ip_line("fd00:cafe::1", Some("10.0.0.42")),
            "node-ip: fd00:cafe::1,10.0.0.42\n"
        );
    }

    #[test]
    fn node_ip_line_falls_back_to_ipv6_only() {
        assert_eq!(
            node_ip_line("fd00:cafe::1", None),
            "node-ip: fd00:cafe::1\n"
        );
        assert_eq!(
            node_ip_line("fd00:cafe::1", Some("")),
            "node-ip: fd00:cafe::1\n"
        );
    }

    #[tokio::test]
    async fn writes_a_dual_stack_config_when_ipv4_is_reachable() {
        let host = FakeHost::new().ok(
            "ip -4 route get 1.1.1.1",
            "1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0",
        );
        let dir = tempfile::tempdir().unwrap();

        run(&host, "fd00:cafe::1", dir.path()).await.unwrap();

        let content =
            std::fs::read_to_string(dir.path().join("etc/rancher/k3s/config.yaml")).unwrap();
        assert_eq!(content, "node-ip: fd00:cafe::1,10.0.0.42\n");
    }

    #[tokio::test]
    async fn writes_ipv6_only_when_there_is_no_ipv4_route() {
        let host = FakeHost::new().fail("ip -4 route get 1.1.1.1", "unreachable");
        let dir = tempfile::tempdir().unwrap();

        run(&host, "fd00:cafe::1", dir.path()).await.unwrap();

        let content =
            std::fs::read_to_string(dir.path().join("etc/rancher/k3s/config.yaml")).unwrap();
        assert_eq!(content, "node-ip: fd00:cafe::1\n");
    }
}
