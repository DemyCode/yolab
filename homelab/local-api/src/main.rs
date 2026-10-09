mod appschema;
mod auth;
mod boot;
mod ceph;
mod ceph_cli;
mod cephfs;
mod charts;
mod config;
mod controllers;
mod cron;
mod csi;
mod disks_reconciler;
mod error;
mod folders;
mod groups;
mod exec;
mod github;
mod hardware;
mod heal;
mod host;
mod http;
mod k8s;
mod mesh;
mod notify;
mod ops;
mod outputs;
mod poll;
mod quantity;
mod records;
mod router;
mod routers;
mod runtime;
mod saved_chart;
mod setups;
mod shared_names;
mod storage;
mod store;

#[cfg(test)]
mod surface;
mod system;
#[cfg(test)]
mod testkit;
mod topology;

use std::sync::Arc;

use auth::AuthState;
use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthState,
    pub kube: k8s::Kube,
    pub http: http::Client,
    pub host: host::RealHost,
}

impl AppState {
    pub(crate) async fn backend(&self) -> anyhow::Result<routers::backup_common::Backend> {
        Ok(routers::backup_common::Backend {
            kube: self.kube.client().await?,
            host: self.host.clone(),
        })
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if args.get(1).map(String::as_str) == Some("storage") {
        std::process::exit(storage::run(&args[2..]).await);
    }
    if args.get(1).map(String::as_str) == Some("boot") {
        std::process::exit(boot::run(&args[2..]).await);
    }
    if args.get(1).map(String::as_str) == Some("notify") {
        std::process::exit(notify::run(&args[2..]).await);
    }
    if args.get(1).map(String::as_str) == Some("shared-names") {
        std::process::exit(shared_names::run(&args[2..]).await);
    }
    if args.get(1).map(String::as_str) == Some("run") {
        let name = args.get(2).map(String::as_str).unwrap_or("");
        let code = match controllers::run_named(name).await {
            Ok(tick) => {
                println!("{name}: {tick:?}");
                0
            }
            Err(e) => {
                eprintln!("{name}: {e:#}");
                1
            }
        };
        std::process::exit(code);
    }

    let cfg = Arc::new(Config::from_env());
    let kube = k8s::Kube::from_environment();
    let sessions = auth::new_sessions();
    auth::init_sessions(&sessions, kube.clone()).await;
    let auth_state = AuthState {
        sessions,
        config: Arc::clone(&cfg),
    };
    let state = AppState {
        config: Arc::clone(&cfg),
        auth: auth_state,
        kube: kube.clone(),
        http: http::client(),
        host: Default::default(),
    };

    let app = router::build_router(state);

    controllers::spawn_all(runtime::leader::start(system::hostname(), kube));

    let mut servers = tokio::task::JoinSet::new();
    for addr in cfg.listen_addrs() {
        servers.spawn(serve_on(addr, app.clone()));
    }
    while servers.join_next().await.is_some() {}
    panic!("every listener stopped");
}

const BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

async fn serve_on(addr: String, app: axum::Router) {
    let listener = loop {
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => break l,
            Err(e) => {
                tracing::warn!("could not bind {addr} yet ({e}), retrying");
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    };
    tracing::info!("listening on {addr}");
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        tracing::error!("listener on {addr} stopped: {e}");
    }
}
