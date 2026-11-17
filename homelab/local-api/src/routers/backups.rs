use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashSet;

use crate::error::Outcome;
use crate::host::Host;
use crate::routers::backup_common::*;
use crate::routers::{backup, backup_points, restore};
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
    Ok(Json(s3_status(&state.http, &url, &token).await?))
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
    for pvc in &outside_tree_pvcs(&b.kube).await? {
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
    let b = state.backend().await?;
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Json(serde_json::json!({ "configured": false })));
    };
    Ok(Json(serde_json::json!({
        "configured": true,
        "recovery_key": format_recovery_key(&cfg.restic_password),
        "acknowledged": recovery_key_acknowledged(&b.kube).await?,
    })))
}

pub async fn acknowledge_recovery(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>> {
    acknowledge_recovery_key(&state.backend().await?.kube).await?;
    Ok(Json(serde_json::json!({ "acknowledged": true })))
}

#[derive(Deserialize)]
pub struct RestoreRequest {
    pub namespace: String,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub choices: crate::routers::restore_plan::Choices,
}

pub async fn restore_app(
    State(state): State<AppState>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>> {
    let name = restore::start(
        &state.backend().await?,
        &body.namespace,
        body.snapshot_id,
        body.choices,
    )
    .await?;
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
    sizes: &backup_points::Sizes,
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
                "size_bytes": sizes.get(ns).map(|s| s.size_bytes),
            })
        })
        .collect();
    serde_json::json!({ "configured": versions.configured, "apps": apps })
}

pub async fn list_backed_up_apps(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let versions = restore::backup_versions(&b).await?;
    let installed: HashSet<String> = managed_namespaces(&b.kube).await?.into_iter().collect();
    let sizes = sizes_or_empty(&b.kube).await;
    Ok(Json(backed_up_apps_json(&versions, &installed, &sizes)))
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

pub async fn app_restore_plan(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DefinitionQuery>,
) -> Result<Json<crate::routers::restore_plan::RestorePlan>> {
    let b = state.backend().await?;
    Ok(Json(
        restore::restore_plan(&b, &namespace, &q.snapshot_id).await?,
    ))
}

pub(crate) async fn setup_namespace_backup<H: Host>(
    b: &Backend<H>,
    namespace: &str,
) -> anyhow::Result<()> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(());
    };
    let pvcs = outside_tree_pvcs(&b.kube).await?;
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        allow_privileged_movers(&b.kube, &pvc.namespace).await;
        restic_secret(&b.kube, &pvc.namespace, &pvc.namespace, &pvc.name, &cfg).await?;
        cfg.unlock(&b.host, &format!("volsync/{}/{}", pvc.namespace, pvc.name))
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
    pub warning: Option<String>,
    pub size: Option<backup_points::ServiceSize>,
    pub budget_set: bool,
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

fn size_json(size: Option<&backup_points::ServiceSize>) -> serde_json::Value {
    match size {
        Some(s) => serde_json::json!({
            "size_bytes": s.size_bytes,
            "budget_bytes": s.budget_bytes,
            "over_budget": s.over_budget,
            "measured_at": s.updated_at,
        }),
        None => serde_json::json!({
            "size_bytes": null,
            "budget_bytes": null,
            "over_budget": false,
            "measured_at": null,
        }),
    }
}

fn app_backup_json(app: &AppBackup) -> serde_json::Value {
    let mut v = serde_json::json!({
        "namespace": app.namespace,
        "instance_name": app.instance_name,
        "app_id": app.app_id,
        "enabled": app.enabled,
        "schedule": app.schedule,
        "state": app.state,
        "last_ok_at": app.last_ok_at,
        "error": app.error,
        "warning": app.warning,
        "budget_set": app.budget_set,
    });
    if let (Some(obj), serde_json::Value::Object(size)) =
        (v.as_object_mut(), size_json(app.size.as_ref()))
    {
        obj.extend(size);
    }
    v
}

pub async fn list_protected_apps(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(protected_apps(&state.backend().await?).await?))
}

async fn sizes_or_empty(client: &kube::Client) -> backup_points::Sizes {
    backup_points::read_sizes(client)
        .await
        .ok_or_warn("backups: the recorded backup sizes are unreadable")
        .unwrap_or_default()
}

pub(crate) async fn protected_apps<H: Host>(b: &Backend<H>) -> anyhow::Result<serde_json::Value> {
    if master_config(&b.kube).await?.is_none() {
        return Ok(serde_json::json!({ "configured": false, "apps": [] }));
    }
    let sets = backup::list(&b.kube).await.unwrap_or_default();
    let mut sizes = sizes_or_empty(&b.kube).await;
    let mut apps = Vec::new();
    for namespace in managed_namespaces(&b.kube).await? {
        let Some(def) = crate::routers::apps::read_definition_opt(&b.kube, &namespace).await?
        else {
            continue;
        };
        let newest = sets
            .iter()
            .find(|s| s["namespace"].as_str() == Some(namespace.as_str()));
        let (state, error) = app_backup_state(newest);
        let last_ok = sets.iter().find(|s| {
            s["namespace"].as_str() == Some(namespace.as_str())
                && s["state"].as_str() == Some("restorable")
        });
        let last_ok_at = last_ok
            .and_then(|s| s["finished_at"].as_str())
            .map(str::to_string);
        let warning = last_ok
            .and_then(|s| s["warning"].as_str())
            .map(str::to_string);
        apps.push(AppBackup {
            instance_name: def.instance_name.clone(),
            app_id: def.app_id.clone(),
            enabled: def.backup.enabled,
            schedule: def.backup.schedule.clone(),
            budget_set: def.backup.budget_bytes.is_some(),
            size: sizes.remove(&namespace),
            namespace,
            state,
            last_ok_at,
            error,
            warning,
        });
    }
    apps.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
    let apps: Vec<serde_json::Value> = apps.iter().map(app_backup_json).collect();
    Ok(serde_json::json!({ "configured": true, "apps": apps }))
}

fn app_namespace(namespace: &str) -> anyhow::Result<()> {
    if !crate::routers::install::is_app_namespace(namespace) {
        anyhow::bail!("{namespace} is not a service");
    }
    Ok(())
}

pub(crate) fn points_json(
    points: &[backup_points::Point],
    size: Option<&backup_points::ServiceSize>,
) -> serde_json::Value {
    let listed: Vec<serde_json::Value> = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            serde_json::json!({
                "snapshot_id": p.snapshot_id,
                "time": p.time,
                "keep": p.keep,
                "older": p.legacy(),
                "newest": i == 0,
                "added_bytes": size.and_then(|s| s.added.get(&p.snapshot_id)).copied(),
            })
        })
        .collect();
    let mut v = serde_json::json!({ "configured": true, "points": listed });
    if let (Some(obj), serde_json::Value::Object(extra)) = (v.as_object_mut(), size_json(size)) {
        obj.extend(extra);
    }
    v
}

pub async fn app_restore_points(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<serde_json::Value>> {
    app_namespace(&namespace)?;
    let b = state.backend().await?;
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Json(
            serde_json::json!({ "configured": false, "points": [] }),
        ));
    };
    let svc = backup_points::Service {
        host: &b.host,
        cfg: &cfg,
        namespace: &namespace,
    };
    let points = svc.points().await?;
    let size = sizes_or_empty(&b.kube).await.remove(&namespace);
    let mut v = points_json(&points, size.as_ref());
    v["budget_set"] = serde_json::json!(backup_points::budget_of(&b.kube, &namespace)
        .await?
        .is_some());
    Ok(Json(v))
}

async fn configured(b: &Backend) -> anyhow::Result<BackupConfig> {
    master_config(&b.kube)
        .await?
        .ok_or_else(|| anyhow::anyhow!("backups are not turned on"))
}

pub async fn point_frees(
    State(state): State<AppState>,
    Path((namespace, snapshot_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    crate::routers::install::check_backup_ref(&namespace, &snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let b = state.backend().await?;
    let cfg = configured(&b).await?;
    let svc = backup_points::Service {
        host: &b.host,
        cfg: &cfg,
        namespace: &namespace,
    };
    let frees = svc.frees(&snapshot_id).await?;
    Ok(Json(serde_json::json!({ "frees_bytes": frees })))
}

pub async fn delete_point(
    State(state): State<AppState>,
    Path((namespace, snapshot_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    crate::routers::install::check_backup_ref(&namespace, &snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let b = state.backend().await?;
    let cfg = configured(&b).await?;
    backup_points::delete_point(&b, &cfg, &namespace, &snapshot_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct KeepRequest {
    pub keep: bool,
}

pub async fn keep_point(
    State(state): State<AppState>,
    Path((namespace, snapshot_id)): Path<(String, String)>,
    Json(body): Json<KeepRequest>,
) -> Result<Json<serde_json::Value>> {
    crate::routers::install::check_backup_ref(&namespace, &snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let b = state.backend().await?;
    let cfg = configured(&b).await?;
    let id = backup_points::keep(&b, &cfg, &namespace, &snapshot_id, body.keep).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "keep": body.keep, "snapshot_id": id }),
    ))
}

#[derive(Deserialize)]
pub struct BudgetRequest {
    pub budget_bytes: Option<u64>,
}

pub(crate) fn checked_budget(budget: Option<u64>) -> anyhow::Result<Option<u64>> {
    match budget {
        Some(0) => anyhow::bail!("a budget of nothing would delete every backup but the newest"),
        other => Ok(other),
    }
}

pub async fn set_budget(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(body): Json<BudgetRequest>,
) -> Result<Json<serde_json::Value>> {
    app_namespace(&namespace)?;
    let budget = checked_budget(body.budget_bytes)?;
    let b = state.backend().await?;
    let mut def = crate::routers::apps::read_definition(&b.kube, &namespace).await?;
    def.backup.budget_bytes = budget;
    let app = crate::routers::apps::installed_schema(
        &b.kube,
        &namespace,
        &def.app_id,
        &state.config.catalog_dir(),
    )
    .await;
    crate::routers::apps::write_definition(&b.kube, &namespace, &def, &app).await?;
    if let Some(cfg) = master_config(&b.kube).await? {
        let (b, ns) = (b.clone(), namespace.clone());
        tokio::spawn(async move { backup_points::refresh(&b, &cfg, &ns).await });
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "budget_bytes": budget }),
    ))
}

pub async fn delete_all_backups(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Result<Json<serde_json::Value>> {
    app_namespace(&namespace)?;
    let b = state.backend().await?;
    let cfg = configured(&b).await?;
    let deleted = backup_points::delete_all(&b, &cfg, &namespace).await?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": deleted })))
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
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    fn not_before_uptime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }
    async fn reconcile(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        sweep_locks(&ctx.tools.backend().await?).await
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
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    async fn reconcile(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let Some((url, token)) = ye_creds(&self.config) else {
            return Ok(crate::runtime::Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        };
        let b = ctx.tools.backend().await?;
        if master_config(&b.kube).await?.is_none() {
            let sources = enable(&b, &url, &token).await?;
            tracing::info!(
                "backups turned on automatically; {} volumes protected",
                sources.len()
            );
            return Ok(crate::runtime::Tick::Done);
        }
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
    for namespace in managed_namespaces(&b.kube).await? {
        cfg.unlock(&b.host, &crate::routers::folder_backup::store(&namespace))
            .await;
    }
    for pvc in outside_tree_pvcs(&b.kube).await? {
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
        let sizes = backup_points::Sizes::from([(
            "yolab-a".to_string(),
            backup_points::ServiceSize {
                size_bytes: 7,
                ..Default::default()
            },
        )]);
        let v = backed_up_apps_json(&versions(), &installed, &sizes);
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
                "size_bytes": 7,
            })
        );
        assert_eq!(v["apps"][1]["installed"], true);
        assert_eq!(v["apps"][1]["size_bytes"], serde_json::Value::Null);
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
            Mock::given(method("GET"))
                .and(path("/api/v1/persistentvolumes"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(list("PersistentVolume", vec![])),
                )
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
        async fn the_sweeper_unlocks_the_settings_every_service_store_and_every_volume_outside_the_tree(
        ) {
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
                    "s3:https://s3.example/bucket-1/services/yolab-notes".to_string(),
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
