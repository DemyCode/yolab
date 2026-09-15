//! The names every machine of the user shares: `cluster.<user>.<domain>` (the
//! interface) and `notify.<user>.<domain>` (notifications).
//!
//! Each machine adds its own tunnel under both names on the platform, which
//! answers DNS with the machines whose tunnel is up (yolab-external
//! `shared_names`). Re-asserted every tick: idempotent, and it repairs a record
//! the platform lost.
//!
//! Caddy gets the certificates for them by DNS-01 — an HTTP challenge would reach
//! whichever machine it reaches — through the platform's acme-dns endpoint, with
//! the account token as key. The token reaches Caddy through an environment file
//! written at boot, never through the Nix store.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::notify::Tunnel;
use crate::runtime::{Controller, Ctx, Scope, Tick};

const NAME: &str = "shared-names";
pub const NAMES: &[&str] = &["cluster", "notify"];
const CADDY_ENV: &str = "var/lib/yolab/caddy/acme.env";

/// Caddy's environment: the key its `acmedns` DNS provider authenticates with.
fn caddy_env(tunnel: &Tunnel) -> Result<String> {
    let token = &tunnel.account_token;
    anyhow::ensure!(
        !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c)),
        "config.toml has no usable [tunnel] account_token"
    );
    Ok(format!("YOLAB_ACCOUNT_TOKEN={token}\n"))
}

pub(crate) fn write_caddy_env(root: &Path, config_path: &str) -> Result<()> {
    let env = caddy_env(&Tunnel::read(config_path)?)?;
    crate::config::write_private_file(&root.join(CADDY_ENV), env.as_bytes())
}

/// `local-api shared-names <subcommand>`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("credentials") => {
            let config = crate::config::machine_dir().join("config.toml");
            match write_caddy_env(Path::new("/"), &config.to_string_lossy()) {
                Ok(()) => 0,
                Err(e) => {
                    tracing::error!("shared-names credentials: {e:#}");
                    1
                }
            }
        }
        other => {
            eprintln!("shared-names: unknown subcommand {other:?} (known: credentials)");
            2
        }
    }
}

pub struct SharedNamesController {
    pub config: crate::config::Config,
}

impl Controller for SharedNamesController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        // Each machine adds its own tunnel.
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(600)
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let tunnel = Tunnel::read(&self.config.config_path)?;
        if !tunnel.enabled || tunnel.tunnel_id.is_empty() {
            return Ok(Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        }
        let client = reqwest::Client::new();
        for name in NAMES {
            client
                .put(record_url(&tunnel, name))
                .bearer_auth(&tunnel.account_token)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .with_context(|| format!("add this machine under {name}"))?
                .error_for_status()
                .with_context(|| format!("add this machine under {name}"))?;
        }
        Ok(Tick::Idle(format!(
            "this machine answers for {}",
            NAMES.join(" and ")
        )))
    }
}

fn record_url(tunnel: &Tunnel, name: &str) -> String {
    format!(
        "{}/tunnels/{}/shared-records/{name}",
        tunnel.platform_api_url, tunnel.tunnel_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(token: &str) -> Tunnel {
        Tunnel {
            enabled: true,
            platform_api_url: "https://api.yolab.io".into(),
            account_token: token.into(),
            tunnel_id: "25".into(),
            host: "node1.6.yolab.io".into(),
        }
    }

    #[test]
    fn each_machine_adds_its_own_tunnel_under_the_shared_names() {
        assert_eq!(
            record_url(&tunnel("t"), "cluster"),
            "https://api.yolab.io/tunnels/25/shared-records/cluster"
        );
    }

    #[test]
    fn caddy_gets_the_account_token_and_nothing_that_could_break_its_env_file() {
        assert_eq!(
            caddy_env(&tunnel("-PXX_abc.123")).unwrap(),
            "YOLAB_ACCOUNT_TOKEN=-PXX_abc.123\n"
        );
        assert!(caddy_env(&tunnel("")).is_err());
        assert!(caddy_env(&tunnel("a\nEVIL=1")).is_err());
    }

    #[test]
    fn the_env_file_is_root_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[tunnel]\nenabled = true\naccount_token = \"tok\"\ntunnel_id = \"25\"\ndns_url = \"https://node1.6.yolab.io\"\n",
        )
        .unwrap();
        write_caddy_env(dir.path(), &config.to_string_lossy()).unwrap();
        let path = dir.path().join(CADDY_ENV);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "YOLAB_ACCOUNT_TOKEN=tok\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
