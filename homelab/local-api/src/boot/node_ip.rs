use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::host::Host;

const DEFAULT_MAX_ATTEMPTS: u32 = 30;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(10);

fn max_attempts() -> u32 {
    std::env::var("YOLAB_NODE_IP_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

fn retry_delay() -> Duration {
    std::env::var("YOLAB_NODE_IP_RETRY_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_DELAY)
}

fn parse_src_ip(route_get_output: &str) -> Option<String> {
    let after = route_get_output.split("src ").nth(1)?;
    after.split_whitespace().next().map(str::to_string)
}

fn node_ip_line(private_ipv6: &str, ipv4: &str) -> String {
    format!("node-ip: {private_ipv6},{ipv4}\n")
}

async fn detect_ipv4<H: Host>(host: &H) -> Option<String> {
    host.run_cmd("ip", &["-4", "route", "get", "1.1.1.1"])
        .await
        .ok()
        .filter(|o| o.success)
        .and_then(|o| parse_src_ip(&o.stdout))
}

pub async fn run<H: Host>(host: &H, private_ipv6: &str, root: &Path) -> Result<()> {
    let attempts = max_attempts();
    let delay = retry_delay();
    let mut ipv4 = None;
    for attempt in 0..attempts {
        ipv4 = detect_ipv4(host).await;
        if ipv4.is_some() {
            break;
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(delay).await;
        }
    }
    let Some(ipv4) = ipv4 else {
        bail!(
            "no IPv4 route found after {attempts} attempts; refusing to write a node-ip \
             that would mismatch k3s's dual-stack cluster-cidr and crash-loop the server"
        );
    };

    let dir = root.join("etc/rancher/k3s");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.yaml");
    std::fs::write(&path, node_ip_line(private_ipv6, &ipv4))?;
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
    fn node_ip_line_is_always_dual_stack() {
        assert_eq!(
            node_ip_line("fd00:cafe::1", "10.0.0.42"),
            "node-ip: fd00:cafe::1,10.0.0.42\n"
        );
    }

    #[tokio::test]
    async fn writes_a_dual_stack_config_when_ipv4_is_reachable_immediately() {
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

    #[tokio::test(start_paused = true)]
    async fn retries_past_a_transient_boot_time_race_and_still_writes_dual_stack() {
        let host = FakeHost::new()
            .fail("ip -4 route get 1.1.1.1", "Network is unreachable")
            .fail("ip -4 route get 1.1.1.1", "Network is unreachable")
            .ok(
                "ip -4 route get 1.1.1.1",
                "1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0",
            );
        let dir = tempfile::tempdir().unwrap();

        run(&host, "fd00:cafe::1", dir.path()).await.unwrap();

        let content =
            std::fs::read_to_string(dir.path().join("etc/rancher/k3s/config.yaml")).unwrap();
        assert_eq!(content, "node-ip: fd00:cafe::1,10.0.0.42\n");
    }

    #[tokio::test(start_paused = true)]
    async fn never_writes_an_ipv6_only_config_that_would_crash_loop_k3s() {
        let host = FakeHost::new().fail("ip -4 route get 1.1.1.1", "Network is unreachable");
        let dir = tempfile::tempdir().unwrap();

        let err = run(&host, "fd00:cafe::1", dir.path()).await.unwrap_err();

        assert!(err.to_string().contains("dual-stack cluster-cidr"));
        assert!(!dir.path().join("etc/rancher/k3s/config.yaml").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_the_max_attempts_not_forever() {
        let host = FakeHost::new().fail("ip -4 route get 1.1.1.1", "Network is unreachable");
        let dir = tempfile::tempdir().unwrap();

        run(&host, "fd00:cafe::1", dir.path()).await.unwrap_err();

        let attempts = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("ip -4 route get 1.1.1.1"))
            .count();
        assert_eq!(attempts, DEFAULT_MAX_ATTEMPTS as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_default_route_is_treated_the_same_as_no_route() {
        let host = FakeHost::new().ok(
            "ip -4 route get 1.1.1.1",
            "RTNETLINK answers: Network is unreachable",
        );
        let dir = tempfile::tempdir().unwrap();

        let err = run(&host, "fd00:cafe::1", dir.path()).await.unwrap_err();

        assert!(err.to_string().contains("no IPv4 route"));
    }
}
