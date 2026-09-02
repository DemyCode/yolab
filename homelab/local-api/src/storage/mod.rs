//! The early-boot storage agent.
//!
//! `local-api serve` is the API + reconciler, ordered after k3s. These
//! subcommands are the other half: the work that must run *before* the Ceph
//! daemons and k3s, which used to be bash embedded in Nix strings. systemd
//! keeps the ordering/timers; only the script body moved here.
pub mod keys;
pub mod osd;

use anyhow::Result;

use crate::host::RealHost;

pub async fn run(args: &[String]) -> i32 {
    let host = RealHost;
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("storage: missing subcommand");
        return 2;
    };

    let result: Result<()> = match sub {
        "mgr-key" => keys::mint(&host, "mgr").await,
        "mds-key" => keys::mint(&host, "mds").await,
        "osd-activate" => osd::run(&host).await,
        _ => {
            eprintln!("storage: unknown subcommand '{sub}'");
            return 2;
        }
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("storage {sub}: {e:#}");
            1
        }
    }
}
