mod auth;
mod boot;
mod cache;
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
mod exec;
mod heal;
mod host;
mod kubectl;
mod mesh;
mod notify;
mod ops;
mod proc;
mod records;
mod router;
mod routers;
mod runtime;
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
    let sessions = auth::new_sessions();
    auth::init_sessions(&sessions).await;
    let auth_state = AuthState {
        sessions,
        config: Arc::clone(&cfg),
    };
    let state = AppState {
        config: Arc::clone(&cfg),
        auth: auth_state,
    };

    let app = router::build_router(state);

    controllers::spawn_all(runtime::leader::start(system::hostname()));

    let addr = format!("[::]:{}", cfg.port);
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("could not bind {addr}: {e}"));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("axum server exited unexpectedly");
}
