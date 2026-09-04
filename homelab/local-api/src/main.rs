mod auth;
mod boot;
mod ceph_cli;
mod cephfs;
mod charts;
mod config;
mod disks_reconciler;
mod error;
mod host;
mod kubectl;
mod lease;
mod proc;
mod routers;
mod storage;
mod system;
mod topology;

use std::sync::Arc;

use axum::{
    middleware,
    routing::{any, delete, get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use auth::{auth_middleware, AuthState};
use config::Config;
use routers::{
    apps, backup_schedule, backups, ceph, ceph_join, custom_app, disks, nodes, packs, rebuild,
    status, terminal, update,
};

/// Single shared state threaded through all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub auth: AuthState,
}

/// Process-lifetime identity for the BackupRun/RestoreRun reconcile Lease. Doesn't need
/// to be stable across restarts — if this process dies, the lease it held simply expires
/// and whichever process (this one restarted, or another node) next acquires it takes
/// over with a fresh identity; see lease.rs.
fn random_holder_id() -> String {
    use rand::RngCore as _;
    let mut buf = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("local-api-{}", hex::encode(buf))
}

/// Keeps a reconcile loop running for the life of the process.
///
/// Every loop passed here is written as `loop { ... sleep ... }` and never
/// returns on its own — its own internal errors are already caught and logged
/// a level down. So a `tokio::spawn`ed copy ending, for any reason (a panic,
/// or the one loop that can return early: disks_reconciler::run() bails out if
/// it cannot read this node's hostname), means the reconciler behind it is
/// gone for good. Nothing else would ever notice: local-api keeps answering
/// HTTP 200 on every other route while, say, the disk reconciler has been
/// dead for a week. `f` is called again to get a fresh future every restart
/// (this is why it takes a factory rather than one future), so a `catch_unwind`
/// per attempt is enough — there is no state inside the loop to lose, since a
/// reconcile tick recomputes everything from cluster/Kubernetes state anyway.
fn supervise<F, Fut>(name: &'static str, mut f: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(f()).await {
                Ok(()) => {
                    tracing::error!(
                        "{name}: reconcile loop exited unexpectedly — restarting in 30s"
                    );
                }
                Err(e) => {
                    tracing::error!("{name}: reconcile loop panicked ({e}) — restarting in 30s");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("storage") {
        std::process::exit(storage::run(&args[2..]).await);
    }
    if args.get(1).map(String::as_str) == Some("boot") {
        std::process::exit(boot::run(&args[2..]).await);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Arc::new(Config::from_env());
    let sessions = auth::new_sessions();
    auth::init_sessions(&sessions).await;
    let auth_state = AuthState {
        sessions,
        config: Arc::clone(&cfg),
    };
    let state = AppState {
        config: Arc::clone(&cfg),
        auth: auth_state.clone(),
    };

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
        .route(
            "/api/update/channel",
            get(update::get_channel).put(update::set_channel),
        )
        .route("/api/update/remotes", post(update::add_remote))
        .route("/api/update/remotes/:name", delete(update::remove_remote))
        // Rebuild log
        .route("/api/rebuild-log", get(rebuild::rebuild_log))
        // Backups
        .route("/api/backups/recovery-key", get(backups::get_recovery_key))
        .route("/api/backups/s3", get(backups::get_s3))
        .route("/api/backups/s3/enable", post(backups::enable_s3))
        .route(
            "/api/backups/credentials/refresh",
            post(backups::refresh_credentials),
        )
        .route("/api/backups/sftp", get(backups::get_sftp))
        .route("/api/backups/status", get(backups::backup_status))
        .route("/api/backups/state", get(backups::operation_state))
        .route("/api/backups/dr/start", post(backups::dr_start))
        .route("/api/backups/dr/status", get(backups::dr_status))
        .route("/api/backups/snapshots", get(backups::list_snapshots))
        .route(
            "/api/backups/cluster/run-now",
            post(backups::run_backup_now),
        )
        .route(
            "/api/backups/schedule",
            get(backup_schedule::get_schedule).put(backup_schedule::set_schedule),
        )
        .route(
            "/api/backups/schedule/preview",
            get(backup_schedule::preview_schedule),
        )
        .route(
            "/api/backups/snapshots/:id/catalog",
            get(backups::snapshot_catalog),
        )
        // Disks
        .route("/api/disks", get(disks::list_disks))
        .route(
            "/api/disks/:node/:id",
            axum::routing::put(disks::set_disk_state),
        )
        .route("/api/disks/:node/:id/erase", post(disks::erase_disk))
        // Storage topology policy (auto/manual)
        .route(
            "/api/storage/policy",
            get(topology::get_policy).put(topology::set_policy),
        )
        // Ceph
        .route("/api/ceph/status", get(ceph::ceph_status))
        .route("/api/ceph/detail", get(ceph::storage_detail))
        .route("/api/ceph/replication", post(ceph::set_replication))
        .route("/api/ceph/dashboard", get(ceph::dashboard_creds))
        // The dashboard itself, proxied to whichever mgr is active. Caddy sends
        // /ceph-dashboard/* here rather than to a fixed address, because the
        // active mgr moves and a fixed address is right only by luck.
        // THREE spellings, and all three are needed. matchit's `/*rest` requires
        // at least one character after the slash, so it does not match a bare
        // "/ceph-dashboard/" — which is exactly what the Storage page links to
        // and what a browser sends for a directory-style URL. Registering only
        // the wildcard and the bare prefix produced a 404 from the router,
        // before the proxy ran at all. See dashboard_route_tests.
        .route("/ceph-dashboard", any(ceph::dashboard_proxy))
        .route("/ceph-dashboard/", any(ceph::dashboard_proxy))
        .route("/ceph-dashboard/*rest", any(ceph::dashboard_proxy))
        .route("/api/cluster/health", get(ceph::cluster_health))
        .route("/api/ceph/osd/:id/mark-in", post(ceph::osd_mark_in))
        .route("/api/ceph/osd/:id/mark-out", post(ceph::osd_mark_out))
        // Nodes
        .route("/api/nodes", get(nodes::nodes))
        .route("/api/nodes/links", get(nodes::node_links))
        .route("/api/nodes/traffic", get(nodes::traffic))
        .route("/api/cluster/join-info", get(nodes::join_info))
        // Ceph credentials for a machine that is joining. Authorized by the shared
        // account_token, like every other node-to-node call — the same secret that
        // already authorizes joining k3s.
        .route("/api/cluster/ceph-join", get(ceph_join::ceph_join_bundle))
        // Apps
        .route(
            "/api/apps/repos",
            get(apps::list_repos).post(apps::add_repo),
        )
        .route("/api/apps/repos/:name", delete(apps::remove_repo))
        .route("/api/apps/repos/sync", post(apps::sync_repos))
        .route(
            "/api/apps/custom",
            get(custom_app::list_custom).post(custom_app::save_custom),
        )
        .route("/api/apps/custom/:id", delete(custom_app::delete_custom))
        // A packaged chart is larger than axum's 2 MB default body limit allows.
        .route(
            "/api/apps/custom/chart",
            post(custom_app::upload_chart)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/api/apps/packs",
            get(packs::list_packs).put(packs::save_pack),
        )
        .route("/api/apps/packs/:name", delete(packs::delete_pack))
        .route("/api/account/token", get(apps::account_token))
        .route("/api/tunnel/domain", get(apps::tunnel_domain))
        .route("/api/apps/catalog", get(apps::catalog))
        // Refresh one chart before its install form renders, so a just-published
        // schema is not hidden behind the hourly background sync.
        .route(
            "/api/apps/catalog/:id/refresh",
            post(apps::refresh_catalog_app),
        )
        .route("/api/apps", get(apps::list_apps))
        // POST installs (uses app_id), DELETE uninstalls (uses instance_name) — same slot
        .route(
            "/api/apps/:id",
            post(apps::install_app).delete(apps::uninstall_app),
        )
        .route("/api/apps/:id/update", post(apps::update_app))
        .route("/api/apps/:id/scan-outputs", post(apps::scan_outputs))
        .route("/api/apps/:id/pods", get(apps::list_pods))
        .route("/api/apps/:id/describe/:pod_name", get(apps::describe_pod))
        .route("/api/apps/:id/logs/:pod_name", get(apps::pod_logs))
        // Terminal
        .route("/api/terminal/exec", post(terminal::exec))
        // Runs after auth (added first, so it's the innermost of these three layers —
        // see restore_run::freeze_during_restore's own doc for what it blocks and why.
        .layer(middleware::from_fn(
            routers::restore_run::freeze_during_restore,
        ))
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .layer(cors)
        .with_state(state.clone());

    // Single reconcile loop drives both BackupRun and RestoreRun objects (see
    // routers/backup_run.rs's module doc for why this replaced three separate
    // ConfigMap-lock-guarded timers).
    supervise(
        "backup-run",
        || routers::backup_run::run(random_holder_id()),
    );
    supervise(
        "replication-source",
        backups::run_replication_source_reconciler,
    );
    // OSD active-state (crush weight + in/out) is driven inside disks_reconciler::run,
    // the single actuator for the DISK→ON/OFF config — no separate watcher.
    supervise("disks", disks_reconciler::run);
    supervise("cephfs", cephfs::run);
    supervise("topology", topology::run_topology_controller);
    // Keeps the app catalog current without a nixos-rebuild — see charts.rs.
    supervise("chart-sync", charts::run_chart_sync);

    let addr = format!("[::]:{}", cfg.port);
    tracing::info!("listening on {addr}");
    // No request is in flight yet at either of these — there is no frontend to report
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
