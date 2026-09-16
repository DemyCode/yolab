mod auth;
mod boot;
mod cache;
mod ceph;
mod ceph_cli;
mod cephfs;
mod charts;
mod config;
mod controllers;
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
mod routers;
mod runtime;
mod shared_names;
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
    apps, backups, ceph as ceph_api, ceph_join, custom_app, disks, logs, nodes, packs, reboot,
    rebuild, status, terminal, update,
};

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
        // What every background controller is doing — see runtime/status.rs.
        .route("/api/system/controllers", get(runtime::status::handler))
        // Fetched on click, never polled: it carries the account token in a
        // fragment. See routers/status.rs `console_link_url`.
        .route("/api/console/link", get(status::console_link))
        // Update / channel
        .route("/api/update", post(update::update))
        .route("/api/update/all", post(update::update_all))
        // Reboot. Separate from update on purpose: an update leaves k3s and Ceph
        // running, a reboot takes the whole machine away. See routers/reboot.rs.
        .route("/api/system/reboot", post(reboot::reboot))
        .route("/api/system/reboot/all", post(reboot::reboot_all))
        .route("/api/update/trigger", post(update::trigger_update))
        .route(
            "/api/update/channel",
            get(update::get_channel).put(update::set_channel),
        )
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
        .route("/api/backups/snapshots", get(backups::list_snapshots))
        .route("/api/backups/runs", get(backups::list_runs))
        .route("/api/backups/restore", post(backups::restore_app))
        .route("/api/backups/restores", get(backups::list_restores))
        .route("/api/backups/apps", get(backups::list_backed_up_apps))
        .route("/api/backups/apps/add", post(backups::add_from_backup))
        .route(
            "/api/backups/cluster/run-now",
            post(backups::run_backup_now),
        )
        .route(
            "/api/backups/snapshots/:id/catalog",
            get(backups::snapshot_catalog),
        )
        // Logs â see routers/logs.rs for why this is a first-class page
        .route("/api/logs", get(logs::list_logs))
        // Disks
        .route("/api/disks", get(disks::list_disks))
        .route(
            "/api/disks/:node/:id",
            axum::routing::put(disks::set_disk_state),
        )
        .route("/api/disks/:node/:id/erase", post(disks::erase_disk))
        // FORCE HEAL — see heal/mod.rs. Served by every machine, with or without Ceph
        // or Kubernetes answering; the machine that receives the POST drives it.
        .route("/api/heal", get(heal::get_status).post(heal::post_heal))
        // Node to node, from the machine driving a heal (heal/member.rs).
        .route("/api/heal/peer", get(heal::get_peer))
        .route("/api/heal/peer/prepare", post(heal::post_peer_prepare))
        .route("/api/heal/peer/arm", post(heal::post_peer_arm))
        .route("/api/heal/peer/undo", post(heal::post_peer_undo))
        // Phone notifications — see notify/mod.rs.
        .route("/api/notifications", get(notify::get_subscription))
        .route("/api/notifications/test", post(notify::post_test))
        .route("/api/notifications/deliver", post(notify::post_deliver))
        // Storage topology policy (auto/manual)
        .route(
            "/api/storage/policy",
            get(topology::get_policy).put(topology::set_policy),
        )
        // Ceph
        .route("/api/ceph/status", get(ceph_api::ceph_status))
        .route("/api/ceph/detail", get(ceph_api::storage_detail))
        .route("/api/ceph/replication", post(ceph_api::set_replication))
        .route("/api/ceph/dashboard", get(ceph_api::dashboard_creds))
        // The dashboard itself, proxied to whichever mgr is active. Caddy sends
        // /ceph-dashboard/* here rather than to a fixed address, because the
        // active mgr moves and a fixed address is right only by luck.
        // THREE spellings, and all three are needed. matchit's `/*rest` requires
        // at least one character after the slash, so it does not match a bare
        // "/ceph-dashboard/" â which is exactly what the Storage page links to
        // and what a browser sends for a directory-style URL. Registering only
        // the wildcard and the bare prefix produced a 404 from the router,
        // before the proxy ran at all. See dashboard_route_tests.
        .route("/ceph-dashboard", any(ceph_api::dashboard_proxy))
        .route("/ceph-dashboard/", any(ceph_api::dashboard_proxy))
        .route("/ceph-dashboard/*rest", any(ceph_api::dashboard_proxy))
        .route("/api/cluster/health", get(ceph_api::cluster_health))
        .route("/api/ceph/osd/:id/mark-in", post(ceph_api::osd_mark_in))
        .route("/api/ceph/osd/:id/mark-out", post(ceph_api::osd_mark_out))
        // Nodes
        .route("/api/nodes", get(nodes::nodes))
        .route("/api/nodes/links", get(nodes::node_links))
        .route("/api/nodes/traffic", get(nodes::traffic))
        .route("/api/cluster/join-info", get(nodes::join_info))
        // Node→node: where this node can be dialed directly. Cluster-authed and
        // never a user route — it is how the relay bootstraps its own replacement.
        .route("/api/cluster/mesh-candidates", get(mesh::mesh_candidates))
        .route("/api/mesh/paths", get(mesh::paths))
        // Ceph credentials for a machine that is joining. Authorized by the shared
        // account_token, like every other node-to-node call â the same secret that
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
        // POST installs (uses app_id), DELETE uninstalls (uses instance_name) â same slot
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
        // INSIDE the auth layer, deliberately. `.layer` wraps what is already
        // there, so this runs after auth_middleware has accepted the request —
        // an unauthenticated caller can never reach the cache, and a 401 is
        // never what gets stored under a key.
        .layer(middleware::from_fn(cache::middleware))
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .layer(cors)
        .with_state(state.clone());

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
