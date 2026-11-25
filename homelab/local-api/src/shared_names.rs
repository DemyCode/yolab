use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::notify::Tunnel;
use crate::runtime::{Ctx, Resource, Scope, Tick};

const NAME: &str = "shared-names";
pub const NAMES: &[&str] = &["cluster", "notify"];
const CADDY_ENV: &str = "var/lib/yolab/caddy/acme.env";

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

pub async fn run(args: &[String]) -> i32 {
    run_at(
        args,
        Path::new("/"),
        &crate::config::machine_dir().join("config.toml"),
    )
}

fn run_at(args: &[String], root: &Path, config: &Path) -> i32 {
    match args.first().map(String::as_str) {
        Some("credentials") => match write_caddy_env(root, &config.to_string_lossy()) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("shared-names credentials: {e:#}");
                1
            }
        },
        other => {
            eprintln!("shared-names: unknown subcommand {other:?} (known: credentials)");
            2
        }
    }
}

pub struct SharedNamesController {
    pub config: crate::config::Config,
}

impl Resource for SharedNamesController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(600)
    }
    async fn converge(&self, ctx: &Ctx) -> Result<Tick> {
        let tunnel = Tunnel::read(&self.config.config_path)?;
        if !tunnel.enabled || tunnel.tunnel_id.is_empty() {
            return Ok(Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        }
        claim_names(&ctx.tools.http, &tunnel).await
    }
}

async fn claim_names(client: &crate::http::Client, tunnel: &Tunnel) -> Result<Tick> {
    for name in NAMES {
        client
            .put(record_url(tunnel, name))
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

    mod against_the_platform {
        use super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn tunnel_at(server: &MockServer) -> Tunnel {
            Tunnel {
                platform_api_url: server.uri(),
                ..tunnel("tok")
            }
        }

        #[tokio::test]
        async fn every_shared_name_is_claimed_with_the_account_token() {
            let server = MockServer::start().await;
            for name in NAMES {
                Mock::given(method("PUT"))
                    .and(path(format!("/tunnels/25/shared-records/{name}")))
                    .and(header("authorization", "Bearer tok"))
                    .respond_with(ResponseTemplate::new(200))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            claim_names(&crate::http::Client::new(), &tunnel_at(&server))
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn a_refused_claim_is_an_error_naming_the_record() {
            let server = MockServer::start().await;
            Mock::given(method("PUT"))
                .respond_with(ResponseTemplate::new(403))
                .mount(&server)
                .await;
            let e = claim_names(&crate::http::Client::new(), &tunnel_at(&server))
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("cluster"), "{e:#}");
        }

        #[tokio::test]
        async fn a_platform_that_cannot_be_reached_is_an_error_naming_the_record() {
            let unreachable = Tunnel {
                platform_api_url: format!("http://[::1]:{}", crate::testkit::closed_port()),
                ..tunnel("tok")
            };
            let e = claim_names(&crate::http::Client::new(), &unreachable)
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("cluster"), "{e:#}");
        }

        fn controller_for(dir: &tempfile::TempDir, body: &str) -> SharedNamesController {
            let path = dir.path().join("config.toml");
            std::fs::write(&path, body).unwrap();
            SharedNamesController {
                config: crate::config::Config::for_test(&path),
            }
        }

        #[tokio::test]
        async fn the_controller_claims_the_names_only_for_a_connected_machine() {
            let server = MockServer::start().await;
            Mock::given(method("PUT"))
                .respond_with(ResponseTemplate::new(200))
                .expect(2)
                .mount(&server)
                .await;
            let ctx = Ctx {
                node: "node1".into(),
                tools: crate::tools::Tools::unreachable(),
            };
            let dir = tempfile::tempdir().unwrap();

            let missing = SharedNamesController {
                config: crate::config::Config::for_test(&dir.path().join("absent.toml")),
            };
            assert_eq!(missing.name(), "shared-names");
            assert_eq!(missing.scope(), Scope::Node);
            assert_eq!(missing.interval(), Duration::from_secs(600));
            assert!(missing.converge(&ctx).await.is_err());

            let not_connected = "this machine is not connected to the YoLab platform".to_string();
            let disabled = controller_for(&dir, "[tunnel]\nenabled = false\ntunnel_id = \"25\"\n");
            assert_eq!(
                disabled.converge(&ctx).await.unwrap(),
                Tick::Idle(not_connected.clone())
            );
            let unnamed = controller_for(&dir, "[tunnel]\nenabled = true\n");
            assert_eq!(
                unnamed.converge(&ctx).await.unwrap(),
                Tick::Idle(not_connected)
            );
            let connected = controller_for(
                &dir,
                &format!(
                    "[tunnel]\nenabled = true\ntunnel_id = \"25\"\naccount_token = \"tok\"\nplatform_api_url = \"{}\"\n",
                    server.uri()
                ),
            );
            assert_eq!(
                connected.converge(&ctx).await.unwrap(),
                Tick::Idle("this machine answers for cluster and notify".into())
            );
        }
    }

    #[tokio::test]
    async fn the_credentials_subcommand_writes_the_env_file_or_fails_and_others_are_usage_errors() {
        assert_eq!(run(&[]).await, 2);
        assert_eq!(run(&["rotate".to_string()]).await, 2);
        let credentials = ["credentials".to_string()];
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        assert_eq!(run_at(&credentials, dir.path(), &config), 1);
        std::fs::write(&config, "[tunnel]\naccount_token = \"\"\n").unwrap();
        assert_eq!(run_at(&credentials, dir.path(), &config), 1);
        std::fs::write(&config, "[tunnel]\naccount_token = \"tok\"\n").unwrap();
        assert_eq!(run_at(&credentials, dir.path(), &config), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(CADDY_ENV)).unwrap(),
            "YOLAB_ACCOUNT_TOKEN=tok\n"
        );
    }
}
