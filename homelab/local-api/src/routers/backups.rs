use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashSet;

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
    let resp = http_client()
        .get(format!("{url}/storage/s3"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Json(serde_json::json!({ "provisioned": false })));
    }
    let body: serde_json::Value = resp
        .error_for_status()
        .map_err(|e| anyhow::anyhow!(e))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(Json(serde_json::json!({ "provisioned": true, "s3": body })))
}

pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if restore::running_anywhere().await? {
        return Err(
            anyhow::anyhow!("A restore is in progress — try again once it finishes.").into(),
        );
    }
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };

    let cfg = ensure_master_config(&url, &token).await?;
    let pvcs = list_user_pvcs().await?;

    let mut sources: Vec<String> = Vec::new();
    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await?;
        ensure_replication_source(pvc, false).await?;
        sources.push(format!("{}/{}", pvc.namespace, pvc.name));
    }

    Ok(Json(serde_json::json!({
        "provisioned": true,
        "pvcs_configured": sources,
        "backup": "PVC data + cluster state snapshotted together daily, whenever the last successful backup is more than 24h old",
    })))
}

fn format_recovery_key(hex: &str) -> String {
    hex.to_uppercase()
        .as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

pub async fn get_recovery_key(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Ok(Json(serde_json::json!({ "configured": false })));
    };
    Ok(Json(serde_json::json!({
        "configured": true,
        "recovery_key": format_recovery_key(&cfg.restic_password),
    })))
}

pub async fn operation_state(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let restores = restore::list().await?;
    let sets = backup::list().await?;
    let active: Vec<serde_json::Value> = sets
        .iter()
        .filter(|s| s["state"] == "running")
        .cloned()
        .collect();
    let last = sets.iter().find(|s| s["state"] != "running").cloned();
    let active_restore = restores.iter().find(|s| s["state"] == "running").cloned();
    Ok(Json(serde_json::json!({
        "backing_up": !active.is_empty() || backup::volsync_mover_running().await,
        "restoring": active_restore.is_some(),
        "backup_run": active.first().cloned(),
        "restore_run": active_restore,
        "last_backup": last,
        "last_ok_age_hours": backup::last_ok_age_hours().await,
        "stale_after_hours": 24,
    })))
}

pub async fn list_runs(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(backup::list().await?)))
}


#[derive(Deserialize)]
pub struct RestoreRequest {
    pub namespace: String,
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

pub async fn restore_app(
    State(_state): State<AppState>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>> {
    let name = restore::start(&body.namespace, body.snapshot_id).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

pub async fn list_restores(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(restore::list().await?)))
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

pub async fn list_backed_up_apps(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>> {
    let versions = restore::backup_versions().await?;
    let installed: HashSet<String> = list_managed_namespaces().await?.into_iter().collect();
    Ok(Json(backed_up_apps_json(&versions, &installed)))
}


async fn snapshots_containing(cfg: &BackupConfig, namespace: &str) -> Option<HashSet<String>> {
    let repo = cfg.restic_repo("cluster-backup");
    let pattern = format!("{namespace}.yaml");
    let out = restic(
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
    )
    .await
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        found
            .as_array()?
            .iter()
            .filter(|e| {
                e["matches"].as_array().is_some_and(|m| !m.is_empty())
            })
            .filter_map(|e| e["snapshot"].as_str().map(str::to_string))
            .collect(),
    )
}

#[derive(Deserialize)]
pub struct SnapshotQuery {
    pub namespace: Option<String>,
}

pub async fn list_snapshots(
    State(_state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SnapshotQuery>,
) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Ok(Json(
            serde_json::json!({ "snapshots": [], "configured": false }),
        ));
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let out = restic(
        &repo,
        &cfg,
        &[
            "snapshots",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
        ],
    )
    .await?;

    if !out.status.success() {
        return Ok(Json(
            serde_json::json!({ "snapshots": [], "configured": true }),
        ));
    }

    let mut snapshots: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!([]));

    if let Some(ns) = q.namespace.as_deref().filter(|s| !s.is_empty()) {
        if let Some(ids) = snapshots_containing(&cfg, ns).await {
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

    Ok(Json(
        serde_json::json!({ "snapshots": snapshots, "configured": true }),
    ))
}

pub async fn run_backup_now(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if read_master_config().await.is_none() {
        return Err(anyhow::anyhow!("backup not configured").into());
    }
    let name = backup::start("manual").await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

pub async fn run_app_backup_now(Path(namespace): Path<String>) -> Result<Json<serde_json::Value>> {
    if read_master_config().await.is_none() {
        return Err(anyhow::anyhow!("backup not configured").into());
    }
    if !namespace.starts_with("yolab-") {
        return Err(anyhow::anyhow!("{namespace} is not an app namespace").into());
    }
    let name = backup::start_app(&namespace, "manual").await?;
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
    let def = restore::definition_from_backup(&namespace, &q.snapshot_id).await?;
    let redacted = crate::routers::apps::redact_definition(&def, &state.config.catalog_dir());
    Ok(Json(
        serde_json::to_value(redacted).unwrap_or(serde_json::Value::Null),
    ))
}


pub async fn setup_namespace_backup(namespace: &str) -> anyhow::Result<()> {
    let Some(cfg) = read_master_config().await else {
        return Ok(());
    };
    let pvcs = list_user_pvcs().await?;
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await?;
        cfg.unlock(&format!(
            "volsync/{}/{}",
            pvc.namespace,
            canonical_pvc_id(&pvc.name)
        ))
        .await;
        ensure_replication_source(&pvc, false).await?;
    }
    Ok(())
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
        let Some(cfg) = read_master_config().await else {
            return Ok(crate::runtime::Tick::Idle("backups are not enabled".into()));
        };
        cfg.unlock("cluster-backup").await;
        for pvc in list_user_pvcs().await? {
            let path = format!("volsync/{}/{}", pvc.namespace, canonical_pvc_id(&pvc.name));
            cfg.unlock(&path).await;
        }
        Ok(crate::runtime::Tick::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
