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

#[cfg(test)]
mod surface;
mod system;
#[cfg(test)]
mod testkit;
mod topology;

use std::sync::Arc;

use auth::AuthState;
use config::Config;

/// Single shared state threaded through all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthState,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // BEFORE the subcommand dispatch below, not after it.
    //
    // These two branches call `process::exit`, so for most of this binary's life
    // every `storage` and `boot` subcommand ran with no subscriber installed at
    // all â and `tracing` drops events on the floor when there is none. The
    // storage units narrate every decision they make ("migrating the existing
    // image store off the root disk", "mount failed â leaving containerd on the
    // root disk", "copy failed (is the RBD large enough?)") and not one of those
    // lines has ever reached a journal.
    //
    // What that cost: on 2026-09-07 node2's yolab-containerd-store ran for 18
    // minutes, moved 8.3G, exited 0 and left the store unmounted â and the entire
    // journal for that unit was systemd's own four lines. The reason it gave was
    // written, formatted, and discarded. These units run before k3s and are the
    // hardest thing in the system to debug after the fact; they are exactly the
    // code that must be able to speak.
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
    // Drives exactly one controller's tick and exits — so a person over SSH can
    // run what the daemon would, without waiting for its interval.
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

    // Every background job runs as a controller. The runtime owns leadership,
    // requirements, pauses and restart — see runtime/mod.rs for what this
    // replaced, and controllers.rs for the list. `local-api run <name>` drives
    // exactly one of them once.
    controllers::spawn_all(runtime::leader::start(system::hostname()));

    let addr = format!("[::]:{}", cfg.port);
    tracing::info!("listening on {addr}");
    // No request is in flight yet at either of these â there is no frontend to report
    // a failure to, so this deliberately still crashes the process (systemd restarts
    // it), just with a message that says which of the two things failed rather than
    // a bare "called `Result::unwrap()` on an `Err` value".
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
