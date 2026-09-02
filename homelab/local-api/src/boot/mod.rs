//! Early-boot node subcommands that are not Ceph-specific: writing k3s's
//! dual-stack node-ip config, recovering stale CephFS CSI locks after a
//! reboot, and the tty1 QR banner. Same shape as `storage::run` — systemd
//! keeps the ordering/timers, only the script bodies moved here.
mod banner;
mod csi_recovery;
mod node_ip;

use anyhow::Result;

use crate::host::RealHost;

pub async fn run(args: &[String]) -> i32 {
    let host = RealHost;
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("boot: missing subcommand");
        return 2;
    };

    let env = |name: &str| std::env::var(name).unwrap_or_default();

    let result: Result<()> = match sub {
        "node-ip" => node_ip::run(&host, &env("YOLAB_NODE_IPV6"), std::path::Path::new("/")).await,
        "csi-recovery" => csi_recovery::run(&host).await,
        "banner" => {
            let config_path = env("YOLAB_CONFIG");
            banner::run(&host, &config_path, std::path::Path::new("/run/issue")).await
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
