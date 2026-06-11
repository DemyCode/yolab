mod auth;
mod config;
mod error;
mod kubectl;
mod priority;
mod proc;
mod routers;

use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use auth::{auth_middleware, AuthState};
use config::Config;
use routers::{apps, ceph, disks, nodes, rebuild, status, terminal, update};

/// Single shared state threaded through all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthState,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Arc::new(Config::from_env());
    let auth_state = AuthState {
        sessions: auth::new_sessions(),
        config: Arc::clone(&cfg),
    };
    let state = AppState { config: Arc::clone(&cfg), auth: auth_state.clone() };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // Auth
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        // Status
        .route("/api/status", get(status::handler))
        // Update / channel
        .route("/api/update", post(update::update))
        .route("/api/update/channel", get(update::get_channel))
        .route("/api/update/channel", put(update::set_channel))
        .route("/api/update/remotes", post(update::add_remote))
        .route("/api/update/remotes/:name", delete(update::remove_remote))
        // Rebuild log
        .route("/api/rebuild-log", get(rebuild::rebuild_log))
        // Disks
        .route("/api/disks/local", get(disks::disks_local))
        .route("/api/disks", get(disks::disks))
        .route("/api/disks/order", put(disks::update_order))
        .route("/api/disks/activate-local", post(disks::activate_local))
        .route("/api/disks/deactivate-local", post(disks::deactivate_local))
        // Ceph
        .route("/api/ceph/status", get(ceph::ceph_status))
        // Nodes
        .route("/api/nodes", get(nodes::nodes))
        .route("/api/cluster/join-info", get(nodes::join_info))
        // Apps
        .route("/api/tunnel/domain", get(apps::tunnel_domain))
        .route("/api/apps/catalog", get(apps::catalog))
        .route("/api/apps", get(apps::list_apps))
        .route("/api/apps/:app_id", post(apps::install_app))
        .route("/api/apps/:instance_name/update", post(apps::update_app))
        .route("/api/apps/:instance_name/scan-outputs", post(apps::scan_outputs))
        .route("/api/apps/:instance_name", delete(apps::uninstall_app))
        .route("/api/apps/:instance_name/pods", get(apps::list_pods))
        .route("/api/apps/:instance_name/describe/:pod_name", get(apps::describe_pod))
        .route("/api/apps/:instance_name/logs/:pod_name", get(apps::pod_logs))
        // Terminal
        .route("/api/terminal/exec", post(terminal::exec))
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .layer(cors)
        .with_state(state.clone());

    // Reconcile loop on primary node
    if cfg.is_primary_node() {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                let cfg2 = Arc::clone(&state.config);
                tokio::spawn(async move {
                    disks::reconcile_storage(cfg2).await;
                });
            }
        });
    }

    let addr = format!("[::]:{}", cfg.port);
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
