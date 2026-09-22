use axum::{
    middleware,
    routing::{any, delete, get, post, put},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use crate::auth::auth_middleware;
use crate::routers::{
    apps, backups, ceph as ceph_api, ceph_join, custom_app, disks, logs, nodes, reboot, rebuild,
    status, terminal, update,
};
use crate::{auth, cache, heal, mesh, notify, runtime, topology, AppState};

pub fn build_router(state: AppState) -> Router {
    let auth_state = state.auth.clone();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/auth/check", get(auth::check))
        .route("/api/status", get(status::handler))
        .route("/api/system/controllers", get(runtime::status::handler))
        .route("/api/console/link", get(status::console_link))
        .route("/api/update", post(update::update))
        .route("/api/update/all", post(update::update_all))
        .route("/api/system/reboot", post(reboot::reboot))
        .route("/api/system/reboot/all", post(reboot::reboot_all))
        .route("/api/update/trigger", post(update::trigger_update))
        .route(
            "/api/update/channel",
            get(update::get_channel).put(update::set_channel),
        )
        .route("/api/rebuild-log", get(rebuild::rebuild_log))
        .route("/api/backups/recovery-key", get(backups::get_recovery_key))
        .route("/api/backups/s3", get(backups::get_s3))
        .route("/api/backups/s3/enable", post(backups::enable_s3))
        .route("/api/backups/state", get(backups::operation_state))
        .route("/api/backups/snapshots", get(backups::list_snapshots))
        .route("/api/backups/runs", get(backups::list_runs))
        .route("/api/backups/restore", post(backups::restore_app))
        .route("/api/backups/restores", get(backups::list_restores))
        .route("/api/backups/apps", get(backups::list_backed_up_apps))
        .route(
            "/api/backups/cluster/run-now",
            post(backups::run_backup_now),
        )
        .route(
            "/api/backups/apps/:namespace/run-now",
            post(backups::run_app_backup_now),
        )
        .route(
            "/api/backups/apps/:namespace/definition",
            get(backups::app_definition_from_backup),
        )
        .route("/api/logs", get(logs::list_logs))
        .route("/api/disks", get(disks::list_disks))
        .route(
            "/api/disks/:node/:id",
            axum::routing::put(disks::set_disk_state),
        )
        .route("/api/heal", get(heal::get_status).post(heal::post_heal))
        .route("/api/heal/peer", get(heal::get_peer))
        .route("/api/heal/peer/prepare", post(heal::post_peer_prepare))
        .route("/api/heal/peer/arm", post(heal::post_peer_arm))
        .route("/api/heal/peer/undo", post(heal::post_peer_undo))
        .route("/api/notifications", get(notify::get_subscription))
        .route("/api/notifications/test", post(notify::post_test))
        .route("/api/notifications/deliver", post(notify::post_deliver))
        .route(
            "/api/storage/policy",
            get(topology::get_policy).put(topology::set_policy),
        )
        .route("/api/ceph/detail", get(ceph_api::storage_detail))
        .route("/api/ceph/dashboard", get(ceph_api::dashboard_creds))
        .route("/ceph-dashboard", any(ceph_api::dashboard_proxy))
        .route("/ceph-dashboard/", any(ceph_api::dashboard_proxy))
        .route("/ceph-dashboard/*rest", any(ceph_api::dashboard_proxy))
        .route("/api/cluster/health", get(ceph_api::cluster_health))
        .route("/api/ceph/osd/:id/mark-in", post(ceph_api::osd_mark_in))
        .route("/api/ceph/osd/:id/mark-out", post(ceph_api::osd_mark_out))
        .route("/api/nodes", get(nodes::nodes))
        .route("/api/nodes/links", get(nodes::node_links))
        .route("/api/cluster/join-info", get(nodes::join_info))
        .route("/api/cluster/mesh-candidates", get(mesh::mesh_candidates))
        .route("/api/mesh/paths", get(mesh::paths))
        .route("/api/cluster/ceph-join", get(ceph_join::ceph_join_bundle))
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
        .route(
            "/api/apps/custom/chart",
            post(custom_app::upload_chart)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/api/tunnel/domain", get(apps::tunnel_domain))
        .route("/api/apps/catalog", get(apps::catalog))
        .route(
            "/api/apps/catalog/:id/refresh",
            post(apps::refresh_catalog_app),
        )
        .route("/api/apps", get(apps::list_apps))
        .route(
            "/api/apps/:id",
            post(apps::install_app).delete(apps::uninstall_app),
        )
        .route("/api/apps/:id/update", post(apps::update_app))
        .route("/api/apps/:id/definition", get(apps::app_definition))
        .route("/api/apps/:id/backup", put(apps::set_backup_policy))
        .route("/api/apps/:id/scan-outputs", post(apps::scan_outputs))
        .route("/api/apps/:id/pods", get(apps::list_pods))
        .route("/api/apps/:id/logs/:pod_name", get(apps::pod_logs))
        .route("/api/terminal/exec", post(terminal::exec))
        .layer(middleware::from_fn(cache::middleware))
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .layer(cors)
        .with_state(state)
}
