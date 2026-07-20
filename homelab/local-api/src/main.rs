mod auth;
mod config;
mod error;
mod kubectl;
mod proc;
mod routers;

use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use auth::{auth_middleware, AuthState};
use config::Config;
use routers::{apps, backups, ceph, nodes, rebuild, status, terminal, update};

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
    let sessions = auth::new_sessions();
    auth::init_sessions(&sessions).await;
    let auth_state = AuthState {
        sessions,
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
        .route("/api/auth/check", get(auth::check))
        // Status
        .route("/api/status", get(status::handler))
        // Update / channel
        .route("/api/update", post(update::update))
        .route("/api/update/all", post(update::update_all))
        .route("/api/update/trigger", post(update::trigger_update))
        .route("/api/update/channel", get(update::get_channel).put(update::set_channel))
        .route("/api/update/remotes", post(update::add_remote))
        .route("/api/update/remotes/:name", delete(update::remove_remote))
        // Rebuild log
        .route("/api/rebuild-log", get(rebuild::rebuild_log))
        // Backups
        .route("/api/backups/s3", get(backups::get_s3))
        .route("/api/backups/s3/enable", post(backups::enable_s3))
        .route("/api/backups/sftp", get(backups::get_sftp))
        .route("/api/backups/status", get(backups::backup_status))
        .route("/api/backups/state", get(backups::operation_state))
        .route("/api/backups/restore/:namespace/:pvc", post(backups::trigger_restore))
        .route("/api/backups/restore/:namespace/:pvc/status", get(backups::restore_status))
        .route("/api/backups/restore/:namespace/:pvc/emergency", post(backups::emergency_restore))
        .route("/api/backups/restore/:namespace/:pvc/emergency/status", get(backups::emergency_restore_status))
        .route("/api/backups/restore/:namespace/:pvc/emergency/apply", post(backups::apply_emergency_restore))
        .route("/api/backups/dr/start", post(backups::dr_start))
        .route("/api/backups/dr/status", get(backups::dr_status))
        .route("/api/backups/dr/apply", post(backups::dr_apply))
        .route("/api/backups/snapshots", get(backups::list_snapshots))
        .route("/api/backups/cluster/run-now", post(backups::run_backup_now))
        .route("/api/backups/snapshots/:id/catalog", get(backups::snapshot_catalog))
        .route("/api/backups/restore/from-snapshot", post(backups::restore_from_snapshot))
        // Ceph
        .route("/api/ceph/status", get(ceph::ceph_status))
        .route("/api/ceph/detail", get(ceph::storage_detail))
        .route("/api/ceph/replication", post(ceph::set_replication))
        .route("/api/ceph/dashboard", get(ceph::dashboard_creds))
        .route("/api/cluster/health", get(ceph::cluster_health))
        .route("/api/ceph/osd/:id/mark-in", post(ceph::osd_mark_in))
        .route("/api/ceph/osd/:id/mark-out", post(ceph::osd_mark_out))
        // Nodes
        .route("/api/nodes", get(nodes::nodes))
        .route("/api/nodes/links", get(nodes::node_links))
        .route("/api/nodes/traffic", get(nodes::traffic))
        .route("/api/cluster/join-info", get(nodes::join_info))
        // Apps
        .route("/api/account/token", get(apps::account_token))
        .route("/api/tunnel/domain", get(apps::tunnel_domain))
        .route("/api/apps/catalog", get(apps::catalog))
        .route("/api/apps", get(apps::list_apps))
        // POST installs (uses app_id), DELETE uninstalls (uses instance_name) — same slot
        .route("/api/apps/:id", post(apps::install_app).delete(apps::uninstall_app))
        .route("/api/apps/:id/update", post(apps::update_app))
        .route("/api/apps/:id/scan-outputs", post(apps::scan_outputs))
        .route("/api/apps/:id/pods", get(apps::list_pods))
        .route("/api/apps/:id/describe/:pod_name", get(apps::describe_pod))
        .route("/api/apps/:id/logs/:pod_name", get(apps::pod_logs))
        // Terminal
        .route("/api/terminal/exec", post(terminal::exec))
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .layer(cors)
        .with_state(state.clone());

    tokio::spawn(backups::run_cluster_backup(Arc::clone(&cfg)));
    tokio::spawn(ceph::run_osd_state_watcher());

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
