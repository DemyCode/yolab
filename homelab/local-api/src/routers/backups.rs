use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashSet;

use crate::host::Host;
use crate::routers::backup_common::*;
use crate::routers::{backup, restore};
use crate::{config::Config, error::Result, AppState};

pub fn ye_creds(cfg: &Config) -> Option<(String, String)> {
    let tunnel = cfg.tunnel_table()?;
    let url = tunnel
        .get("platform_api_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let token = tunnel
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if url.is_empty() || token.is_empty() {
        return None;
    }
    Some((url, token))
}

pub async fn get_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(
            serde_json::json!({ "provisioned": false, "reason": "platform API not configured" }),
        ));
    };
    Ok(Json(s3_status(&crate::http::client(), &url, &token).await?))
}

async fn s3_status(
    client: &crate::http::Client,
    url: &str,
    token: &str,
) -> anyhow::Result<serde_json::Value> {
    let resp = client
        .get(format!("{url}/storage/s3"))
        .bearer_auth(token)
        .send()
        .await?;
    if resp.status() == crate::http::StatusCode::NOT_FOUND {
        return Ok(serde_json::json!({ "provisioned": false }));
    }
    let body: serde_json::Value = resp.error_for_status()?.json().await?;
    Ok(serde_json::json!({ "provisioned": true, "s3": body }))
}

pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    if restore::running_anywhere(&b.kube).await? {
        return Err(
            anyhow::anyhow!("A restore is in progress — try again once it finishes.").into(),
        );
    }
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };

    let sources = enable(&b, &url, &token).await?;
    Ok(Json(serde_json::json!({
        "provisioned": true,
        "pvcs_configured": sources,
        "backup": "PVC data + cluster state snapshotted together daily, whenever the last successful backup is more than 24h old",
    })))
}

pub(crate) async fn enable<H: Host>(
    b: &Backend<H>,
    url: &str,
    token: &str,
) -> anyhow::Result<Vec<String>> {
    let cfg = ensure_master_config_with(&b.kube, url, token).await?;
    let mut sources: Vec<String> = Vec::new();
    for pvc in &user_pvcs(&b.kube).await? {
        allow_privileged_movers(&b.kube, &pvc.namespace).await;
        restic_secret(&b.kube, &pvc.namespace, &pvc.namespace, &pvc.name, &cfg).await?;
        replication_source(&b.kube, pvc, false).await?;
        sources.push(format!("{}/{}", pvc.namespace, pvc.name));
    }
    Ok(sources)
}

fn format_recovery_key(hex: &str) -> String {
    hex.to_uppercase()
        .as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

pub async fn get_recovery_key(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = master_config(&state.backend().await?.kube).await? else {
        return Ok(Json(serde_json::json!({ "configured": false })));
    };
    Ok(Json(serde_json::json!({
        "configured": true,
        "recovery_key": format_recovery_key(&cfg.restic_password),
    })))
}

pub async fn operation_state(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(operation(&state.backend().await?).await?))
}

pub(crate) async fn operation<H: Host>(b: &Backend<H>) -> anyhow::Result<serde_json::Value> {
    let restores = restore::list(&b.kube).await?;
    let sets = backup::list(&b.kube).await?;
    let active: Vec<serde_json::Value> = sets
        .iter()
        .filter(|s| s["state"] == "running")
        .cloned()
        .collect();
    let last = sets.iter().find(|s| s["state"] != "running").cloned();
    let active_restore = restores.iter().find(|s| s["state"] == "running").cloned();
    Ok(serde_json::json!({
        "backing_up": !active.is_empty() || backup::volsync_mover_running(&b.kube).await,
        "restoring": active_restore.is_some(),
        "backup_run": active.first().cloned(),
        "restore_run": active_restore,
        "last_backup": last,
        "last_ok_age_hours": backup::last_ok_age_hours(&b.kube).await,
        "stale_after_hours": 24,
    }))
}

pub async fn list_runs(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(
        backup::list(&state.kube.client().await?).await?,
    )))
}

#[derive(Deserialize)]
pub struct RestoreRequest {
    pub namespace: String,
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

pub async fn restore_app(
    State(state): State<AppState>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>> {
    let name = restore::start(&state.backend().await?, &body.namespace, body.snapshot_id).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

pub async fn list_restores(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(
        restore::list(&state.kube.client().await?).await?,
    )))
}

fn backed_up_apps_json(
    versions: &restore::BackupVersions,
    installed: &HashSet<String>,
) -> serde_json::Value {
    let apps: Vec<serde_json::Value> = versions
        .apps
        .iter()
        .map(|(ns, vs)| {
            serde_json::json!({
                "namespace": ns,
                "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
                "installed": installed.contains(ns),
                "versions": vs,
            })
        })
        .collect();
    serde_json::json!({ "configured": versions.configured, "apps": apps })
}

pub async fn list_backed_up_apps(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let versions = restore::backup_versions(&b).await?;
    let installed: HashSet<String> = managed_namespaces(&b.kube).await?.into_iter().collect();
    Ok(Json(backed_up_apps_json(&versions, &installed)))
}

async fn snapshots_containing<H: Host>(
    host: &H,
    cfg: &BackupConfig,
    namespace: &str,
) -> Option<HashSet<String>> {
    let repo = cfg.restic_repo("cluster-backup");
    let pattern = format!("{namespace}.yaml");
    let out = restic_with(
        host,
        &repo,
        cfg,
        &[
            "find",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
            &pattern,
        ],
        RESTIC_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.success {
        return None;
    }
    let found: serde_json::Value = serde_json::from_str(&out.stdout).ok()?;
    Some(
        found
            .as_array()?
            .iter()
            .filter(|e| e["matches"].as_array().is_some_and(|m| !m.is_empty()))
            .filter_map(|e| e["snapshot"].as_str().map(str::to_string))
            .collect(),
    )
}

#[derive(Deserialize)]
pub struct SnapshotQuery {
    pub namespace: Option<String>,
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SnapshotQuery>,
) -> Result<Json<serde_json::Value>> {
    Ok(Json(
        snapshots(&state.backend().await?, q.namespace.as_deref()).await?,
    ))
}

pub(crate) async fn snapshots<H: Host>(
    b: &Backend<H>,
    namespace: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(serde_json::json!({ "snapshots": [], "configured": false }));
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;

    let out = restic_with(
        &b.host,
        &repo,
        &cfg,
        &[
            "snapshots",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
        ],
        RESTIC_TIMEOUT,
    )
    .await?;

    if !out.success {
        return Ok(serde_json::json!({ "snapshots": [], "configured": true }));
    }

    let mut snapshots: serde_json::Value =
        serde_json::from_str(&out.stdout).unwrap_or(serde_json::json!([]));

    if let Some(ns) = namespace.filter(|s| !s.is_empty()) {
        if let Some(ids) = snapshots_containing(&b.host, &cfg, ns).await {
            if let Some(arr) = snapshots.as_array() {
                let kept: Vec<serde_json::Value> = arr
                    .iter()
                    .filter(|s| {
                        s["short_id"]
                            .as_str()
                            .or_else(|| s["id"].as_str())
                            .is_some_and(|id| {
                                ids.iter()
                                    .any(|f| id.starts_with(f.as_str()) || f.starts_with(id))
                            })
                    })
                    .cloned()
                    .collect();
                snapshots = serde_json::Value::Array(kept);
            }
        }
    }

    Ok(serde_json::json!({ "snapshots": snapshots, "configured": true }))
}

pub async fn run_backup_now(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let name = backup::start(&state.backend().await?, "manual").await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

pub async fn run_app_backup_now(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<serde_json::Value>> {
    if !namespace.starts_with("yolab-") {
        return Err(anyhow::anyhow!("{namespace} is not an app namespace").into());
    }
    let name = backup::start_app(&state.backend().await?, &namespace, "manual").await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

#[derive(Deserialize)]
pub struct DefinitionQuery {
    pub snapshot_id: String,
}

pub async fn app_definition_from_backup(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DefinitionQuery>,
) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let def = restore::definition_from_backup(&b, &namespace, &q.snapshot_id).await?;
    let app = match restore::kept_from_backup(&b, &namespace, &q.snapshot_id).await {
        Ok((_, Some(schema))) => crate::appschema::AppSchema::new(schema),
        _ => crate::routers::apps::app_schema(&state.config.catalog_dir(), &def.app_id),
    };
    let redacted = crate::routers::apps::redact_definition(&def, &app);
    Ok(Json(
        serde_json::to_value(redacted).unwrap_or(serde_json::Value::Null),
    ))
}

pub(crate) async fn setup_namespace_backup<H: Host>(
    b: &Backend<H>,
    namespace: &str,
) -> anyhow::Result<()> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(());
    };
    let pvcs = user_pvcs(&b.kube).await?;
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        allow_privileged_movers(&b.kube, &pvc.namespace).await;
        restic_secret(&b.kube, &pvc.namespace, &pvc.namespace, &pvc.name, &cfg).await?;
        cfg.unlock(
            &b.host,
            &format!("volsync/{}/{}", pvc.namespace, pvc.name),
        )
        .await;
        replication_source(&b.kube, &pvc, false).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppBackup {
    pub namespace: String,
    pub instance_name: String,
    pub app_id: String,
    pub enabled: bool,
    pub schedule: String,
    pub state: String,
    pub last_ok_at: Option<String>,
    pub error: Option<String>,
}

pub(crate) fn app_backup_state(set: Option<&serde_json::Value>) -> (String, Option<String>) {
    let Some(set) = set else {
        return ("never".to_string(), None);
    };
    let state = set["state"].as_str().unwrap_or("");
    let error = set["error"]
        .as_str()
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    match state {
        "queued" => ("queued".to_string(), None),
        "running" => ("running".to_string(), None),
        "restorable" => ("ok".to_string(), None),
        _ => ("failed".to_string(), error),
    }
}

fn app_backup_json(app: &AppBackup) -> serde_json::Value {
    serde_json::json!({
        "namespace": app.namespace,
        "instance_name": app.instance_name,
        "app_id": app.app_id,
        "enabled": app.enabled,
        "schedule": app.schedule,
        "state": app.state,
        "last_ok_at": app.last_ok_at,
        "error": app.error,
    })
}

pub async fn list_protected_apps(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(protected_apps(&state.backend().await?).await?))
}

pub(crate) async fn protected_apps<H: Host>(b: &Backend<H>) -> anyhow::Result<serde_json::Value> {
    if master_config(&b.kube).await?.is_none() {
        return Ok(serde_json::json!({ "configured": false, "apps": [] }));
    }
    let sets = backup::list(&b.kube).await.unwrap_or_default();
    let mut apps = Vec::new();
    for namespace in managed_namespaces(&b.kube).await? {
        let Some(def) = crate::routers::apps::read_definition_opt(&b.kube, &namespace).await else {
            continue;
        };
        let newest = sets
            .iter()
            .find(|s| s["namespace"].as_str() == Some(namespace.as_str()));
        let (state, error) = app_backup_state(newest);
        let last_ok_at = sets
            .iter()
            .find(|s| {
                s["namespace"].as_str() == Some(namespace.as_str())
                    && s["state"].as_str() == Some("restorable")
            })
            .and_then(|s| s["finished_at"].as_str())
            .map(str::to_string);
        apps.push(AppBackup {
            instance_name: def.instance_name.clone(),
            app_id: def.app_id.clone(),
            enabled: def.backup.enabled,
            schedule: def.backup.schedule.clone(),
            namespace,
            state,
            last_ok_at,
            error,
        });
    }
    apps.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
    let apps: Vec<serde_json::Value> = apps.iter().map(app_backup_json).collect();
    Ok(serde_json::json!({ "configured": true, "apps": apps }))
}

pub async fn app_restore_points(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<serde_json::Value>> {
    if !crate::routers::install::is_app_namespace(&namespace) {
        return Err(anyhow::anyhow!("{namespace} is not an app namespace").into());
    }
    let b = state.backend().await?;
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Json(
            serde_json::json!({ "configured": false, "points": [] }),
        ));
    };
    let points = restore::restore_points(&b.host, &cfg, &namespace).await?;
    Ok(Json(
        serde_json::json!({ "configured": true, "points": points }),
    ))
}

pub struct LockSweeperController;

impl crate::runtime::Controller for LockSweeperController {
    fn name(&self) -> &'static str {
        "backup-lock-sweeper"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1800)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    fn not_before_uptime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        sweep_locks(&Backend::real().await?).await
    }
}

pub struct BackupKeyController {
    pub config: Config,
}

impl crate::runtime::Controller for BackupKeyController {
    fn name(&self) -> &'static str {
        "backup-key"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let Some((url, token)) = ye_creds(&self.config) else {
            return Ok(crate::runtime::Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        };
        let b = Backend::real().await?;
        Ok(key_refresh_tick(
            refresh_master_key_with(&b.kube, &url, &token).await?,
        ))
    }
}

fn key_refresh_tick(outcome: KeyRefresh) -> crate::runtime::Tick {
    use crate::runtime::Tick;
    match outcome {
        KeyRefresh::NotEnabled => Tick::Idle("backups are not enabled".into()),
        KeyRefresh::Current => Tick::Idle("the backup key matches the platform".into()),
        KeyRefresh::Replaced => {
            tracing::info!("backup key replaced with the one the platform reissued");
            Tick::Done
        }
        KeyRefresh::OtherBucket => {
            Tick::Idle("the platform names a different bucket; keeping this cluster's key".into())
        }
    }
}

pub(crate) async fn sweep_locks<H: Host>(b: &Backend<H>) -> anyhow::Result<crate::runtime::Tick> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(crate::runtime::Tick::Idle("backups are not enabled".into()));
    };
    cfg.unlock(&b.host, "cluster-backup").await;
    for pvc in user_pvcs(&b.kube).await? {
        let path = format!("volsync/{}/{}", pvc.namespace, pvc.name);
        cfg.unlock(&b.host, &path).await;
    }
    Ok(crate::runtime::Tick::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_replaced_key_counts_as_work_done() {
        use crate::runtime::Tick;
        assert!(matches!(key_refresh_tick(KeyRefresh::Replaced), Tick::Done));
        for idle in [
            KeyRefresh::NotEnabled,
            KeyRefresh::Current,
            KeyRefresh::OtherBucket,
        ] {
            assert!(matches!(key_refresh_tick(idle), Tick::Idle(_)));
        }
    }

    fn versions() -> restore::BackupVersions {
        let v = |id: &str| restore::AppVersion {
            snapshot_id: id.into(),
            time: "2026-09-14T02:00:00Z".into(),
        };
        restore::BackupVersions {
            configured: true,
            apps: std::collections::BTreeMap::from([
                ("yolab-a".to_string(), vec![v("new"), v("old")]),
                ("yolab-b".to_string(), vec![v("old")]),
            ]),
        }
    }

    #[test]
    fn the_list_shows_each_backed_up_app_with_its_state_and_versions() {
        let installed: HashSet<String> = ["yolab-b".to_string()].into();
        let v = backed_up_apps_json(&versions(), &installed);
        assert_eq!(v["configured"], true);
        assert_eq!(
            v["apps"][0],
            serde_json::json!({
                "namespace": "yolab-a",
                "instance_name": "a",
                "installed": false,
                "versions": [
                    {"snapshot_id": "new", "time": "2026-09-14T02:00:00Z"},
                    {"snapshot_id": "old", "time": "2026-09-14T02:00:00Z"},
                ],
            })
        );
        assert_eq!(v["apps"][1]["installed"], true);
    }

    #[test]
    fn format_recovery_key_groups_and_uppercases() {
        assert_eq!(
            format_recovery_key("abcdef0123456789"),
            "ABCDE-F0123-45678-9"
        );
    }

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list, status};
        use serde_json::{json, Value};
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const CONFIG_PATH: &str = "/api/v1/namespaces/kube-system/secrets/yolab-backup-config";

        fn enabled() -> Value {
            use base64::Engine as _;
            let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" },
                "data": {
                    "access_key_id": b("AKID"), "secret_access_key": b("SECRET"),
                    "bucket": b("bucket-1"), "endpoint": b("https://s3.example"),
                    "restic_password": b("pw")
                }
            })
        }

        async fn config(server: &MockServer, code: u16, body: Value) {
            Mock::given(method("GET"))
                .and(path(CONFIG_PATH))
                .respond_with(ResponseTemplate::new(code).set_body_json(body))
                .mount(server)
                .await;
        }

        async fn apps(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces"))
                .and(query_param("labelSelector", "yolab.io/managed=true"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Namespace",
                    vec![json!({ "metadata": { "name": "yolab-notes" } })],
                )))
                .mount(server)
                .await;
            let claim = |ns: &str, name: &str| {
                json!({
                    "metadata": { "name": name, "namespace": ns },
                    "spec": { "resources": { "requests": { "storage": "1Gi" } } }
                })
            };
            Mock::given(method("GET"))
                .and(path("/api/v1/persistentvolumeclaims"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "PersistentVolumeClaim",
                    vec![
                        claim("yolab-notes", "data"),
                        claim("yolab-notes", "volsync-data-cache"),
                        claim("yolab-other", "data"),
                    ],
                )))
                .mount(server)
                .await;
        }

        fn unlocked_repos(host: &FakeHost) -> Vec<String> {
            host.envs_of("restic unlock")
                .into_iter()
                .filter_map(|env| {
                    env.into_iter()
                        .find(|(k, _)| k == "RESTIC_REPOSITORY")
                        .map(|(_, v)| v)
                })
                .collect()
        }

        #[tokio::test]
        async fn the_sweeper_does_nothing_while_backups_are_off() {
            let (server, kube) = api_server().await;
            config(&server, 404, status(404, "NotFound")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            assert!(matches!(
                sweep_locks(&b).await.unwrap(),
                crate::runtime::Tick::Idle(_)
            ));
            assert!(b.host.calls().is_empty());
        }

        #[tokio::test]
        async fn the_sweeper_unlocks_the_cluster_repo_and_every_app_volume() {
            let (server, kube) = api_server().await;
            config(&server, 200, enabled()).await;
            apps(&server).await;
            let b = Backend {
                kube,
                host: FakeHost::new().ok("restic unlock", ""),
            };

            sweep_locks(&b).await.unwrap();
            assert_eq!(
                unlocked_repos(&b.host),
                vec![
                    "s3:https://s3.example/bucket-1/cluster-backup".to_string(),
                    "s3:https://s3.example/bucket-1/volsync/yolab-notes/data".to_string(),
                ]
            );
        }

        #[tokio::test]
        async fn a_new_app_is_not_wired_for_backups_while_they_are_off() {
            let (server, kube) = api_server().await;
            config(&server, 404, status(404, "NotFound")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            setup_namespace_backup(&b, "yolab-notes").await.unwrap();
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert!(b.host.calls().is_empty());
        }

        #[tokio::test]
        async fn protected_apps_says_not_configured_without_listing_anything() {
            let (server, kube) = api_server().await;
            config(&server, 404, status(404, "NotFound")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            assert_eq!(
                protected_apps(&b).await.unwrap(),
                json!({ "configured": false, "apps": [] })
            );
            assert!(b.host.calls().is_empty());
        }

        #[tokio::test]
        async fn protected_apps_fails_rather_than_saying_off_when_the_cluster_is_down() {
            let (server, kube) = api_server().await;
            config(&server, 503, status(503, "ServiceUnavailable")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };
            assert!(protected_apps(&b).await.is_err());
        }

        #[tokio::test]
        async fn a_restore_driven_by_another_node_shows_as_restoring() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/pods"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list("Pod", vec![])))
                .mount(&server)
                .await;
            crate::records::testing::records(
                &server,
                "yolab-restores",
                json!([{
                    "id": "rs-remote", "namespace": "yolab-notes",
                    "started_at": "2026-09-28T00:00:00Z", "state": "running",
                    "owner": "some-other-node", "heartbeat": "2026-09-28T00:00:00Z"
                }]),
            )
            .await;
            crate::records::testing::no_records(&server, "yolab-backups").await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            let op = operation(&b).await.unwrap();
            assert_eq!(op["restoring"], true);
            assert_eq!(op["restore_run"]["id"], "rs-remote");
            assert_eq!(op["backing_up"], false);
        }
    }

    mod against_the_platform {
        use super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn storage_the_platform_has_not_made_yet_is_not_provisioned() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/storage/s3"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            assert_eq!(
                s3_status(&crate::http::Client::new(), &server.uri(), "tok")
                    .await
                    .unwrap(),
                serde_json::json!({ "provisioned": false })
            );
        }

        #[tokio::test]
        async fn provisioned_storage_is_passed_through() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/storage/s3"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "bucket_name": "b" })),
                )
                .mount(&server)
                .await;
            let v = s3_status(&crate::http::Client::new(), &server.uri(), "tok")
                .await
                .unwrap();
            assert_eq!(v["provisioned"], true);
            assert_eq!(v["s3"]["bucket_name"], "b");
        }

        #[tokio::test]
        async fn a_platform_error_is_an_error_not_unprovisioned() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(502))
                .mount(&server)
                .await;
            assert!(s3_status(&crate::http::Client::new(), &server.uri(), "tok")
                .await
                .is_err());
        }
    }
}
