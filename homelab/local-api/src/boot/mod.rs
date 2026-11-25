mod banner;
mod lan_subnets;
mod node_ip;

use std::path::Path;

use anyhow::Result;

use crate::host::Host;

pub async fn run(args: &[String]) -> i32 {
    let host = crate::tools::Tools::real().host;
    run_at(&host, args, Path::new("/")).await
}

async fn run_at<H: Host>(host: &H, args: &[String], root: &Path) -> i32 {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("boot: missing subcommand");
        return 2;
    };

    let env = |name: &str| std::env::var(name).unwrap_or_default();

    let result: Result<()> = match sub {
        "node-ip" => node_ip::run(host, &env("YOLAB_NODE_IPV6"), root).await,
        "lan-subnets" => {
            lan_subnets::run(host, &env("YOLAB_LAN_SET4"), &env("YOLAB_LAN_SET6")).await
        }
        "banner" => {
            let config_path = env("YOLAB_CONFIG");
            banner::run(host, &config_path, &root.join("run/issue")).await
        }
        _ => {
            eprintln!("boot: unknown subcommand '{sub}'");
            return 2;
        }
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("boot {sub}: {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn a_missing_or_unknown_subcommand_is_a_usage_error() {
        assert_eq!(run(&[]).await, 2);
        assert_eq!(run(&["reboot-everything".to_string()]).await, 2);
    }

    #[tokio::test]
    async fn a_lan_subnet_sync_without_its_ipset_names_fails_with_exit_code_one() {
        assert_eq!(run(&["lan-subnets".to_string()]).await, 1);
    }

    #[tokio::test]
    async fn node_ip_writes_the_k3s_config_under_the_given_root_and_exits_zero() {
        let host = FakeHost::new().ok(
            "ip -4 route get 1.1.1.1",
            "1.1.1.1 via 10.0.0.1 dev eth0 src 10.0.0.42 uid 0",
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_at(&host, &["node-ip".to_string()], dir.path()).await, 0);
        let written =
            std::fs::read_to_string(dir.path().join("etc/rancher/k3s/config.yaml")).unwrap();
        assert!(written.ends_with(",10.0.0.42\n"), "{written}");
    }

    #[tokio::test]
    async fn the_banner_is_written_to_run_issue_under_the_root_and_fails_without_that_folder() {
        let host = FakeHost::new();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_at(&host, &["banner".to_string()], dir.path()).await, 1);
        std::fs::create_dir_all(dir.path().join("run")).unwrap();
        assert_eq!(run_at(&host, &["banner".to_string()], dir.path()).await, 0);
        assert!(dir.path().join("run/issue").exists());
    }
}
