use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::process::Command;

use crate::routers::backup_common::*;
use crate::routers::{backup, restore};
use crate::{config::Config, error::Result, AppState};

// ── S3 / SFTP pass-through endpoints ─────────────────────────────────────────

pub fn ye_creds(cfg: &Config) -> Option<(String, String)> {
    let text = std::fs::read_to_string(&cfg.config_path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    if let Some(tunnel) = table.get("tunnel").and_then(|v| v.as_table()) {
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
        if !url.is_empty() && !token.is_empty() {
            return Some((url, token));
        }
    }
    None
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

pub async fn get_sftp(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(
            serde_json::json!({ "provisioned": false, "reason": "platform API not configured" }),
        ));
    };
    let resp = http_client()
        .get(format!("{url}/storage/sftp"))
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
    Ok(Json(
        serde_json::json!({ "provisioned": true, "sftp": body }),
    ))
}

/// POST /api/backups/s3/enable — idempotent: provisions B2, configures VolSync per PVC.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if restore::is_running().await {
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

/// GET /api/backups/state — the frontend's single source of truth for what is running.
pub async fn operation_state(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let restores = restore::list().await;
    let sets = backup::list().await;
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

/// GET /api/backups/runs — every recorded backup set, newest first, in three states
/// (`running`, `restorable`, `crashed`).
pub async fn list_runs(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::Value::Array(backup::list().await))
}

/// A PVC hasn't synced in this long → flag it as stale rather than silently "Pending" forever.
const STALE_AFTER_HOURS: i64 = 36;

/// GET /api/backups/status — per-PVC VolSync ReplicationSource status.
pub async fn backup_status(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let v = get_replication_sources().await;

    let pvc_health_map: HashMap<(String, String), (String, Option<String>)> =
        crate::kubectl::get_json(&["get", "pvc", "-A", "-o", "json"])
            .await
            .ok()
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let ns = item["metadata"]["namespace"].as_str()?.to_string();
                let name = item["metadata"]["name"].as_str()?.to_string();
                let phase = item["status"]["phase"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_string();
                let deletion_ts = item["metadata"]["deletionTimestamp"]
                    .as_str()
                    .map(String::from);
                Some(((ns, name), (phase, deletion_ts)))
            })
            .collect();

    let pvcs: Vec<serde_json::Value> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| {
            let namespace = item["metadata"]["namespace"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let pvc = item["spec"]["sourcePVC"].as_str().unwrap_or("").to_string();
            let created = item["metadata"]["creationTimestamp"]
                .as_str()
                .map(String::from);
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            let last_sync_duration = item["status"]["lastSyncDuration"]
                .as_str()
                .map(String::from);
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or(if last_sync_time.is_some() {
                    "Successful"
                } else {
                    "Pending"
                })
                .to_string();
            let (pvc_phase, pvc_deletion_ts) = pvc_health_map
                .get(&(namespace.clone(), pvc.clone()))
                .cloned()
                .unwrap_or(("NotFound".to_string(), None));

            let stale = match &last_sync_time {
                Some(t) => hours_since(t).is_none_or(|h| h > STALE_AFTER_HOURS),
                None => created
                    .as_deref()
                    .and_then(hours_since)
                    .is_some_and(|h| h > STALE_AFTER_HOURS),
            };
            let stuck_terminating = pvc_deletion_ts.is_some();

            serde_json::json!({
                "namespace": namespace,
                "pvc": pvc,
                "last_sync_time": last_sync_time,
                "last_sync_duration": last_sync_duration,
                "result": result,
                "pvc_phase": pvc_phase,
                "stale": stale,
                "stuck_terminating": stuck_terminating,
                "pvc_deletion_timestamp": pvc_deletion_ts,
            })
        })
        .collect();

    let backup_alert = pvcs.iter().any(|p| {
        p["stale"].as_bool().unwrap_or(false) || p["stuck_terminating"].as_bool().unwrap_or(false)
    });

    // When cluster state (etcd) was last captured, from the newest restorable set.
    let etcd_last = backup::list()
        .await
        .into_iter()
        .find(|s| s["state"] == "restorable")
        .and_then(|s| s["finished_at"].as_str().map(String::from));

    Ok(Json(serde_json::json!({
        "pvcs": pvcs,
        "etcd_last_snapshot": etcd_last,
        "backup_alert": backup_alert,
    })))
}

// ── Per-app restore (thin HTTP layer over restore.rs) ─────────────────────────

#[derive(Deserialize)]
pub struct RestoreRequest {
    /// The app's namespace, e.g. "yolab-gitea".
    pub namespace: String,
    /// A specific restic `cluster-backup` snapshot id, or omit to restore latest.
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

/// POST /api/backups/restore — restores one app's data and config from a backup.
pub async fn restore_app(
    State(_state): State<AppState>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>> {
    let name = restore::start(&body.namespace, body.snapshot_id).await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

/// GET /api/backups/restores — every recorded app restore, newest first.
pub async fn list_restores(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::Value::Array(restore::list().await))
}

// ── Cluster backup ─────────────────────────────────────────────────────────────

/// GET /api/backups/snapshots — list available cluster-backup restic snapshots.
pub async fn list_snapshots(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
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

    let snapshots: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!([]));

    Ok(Json(
        serde_json::json!({ "snapshots": snapshots, "configured": true }),
    ))
}

/// GET /api/backups/snapshots/:id/catalog
pub async fn snapshot_catalog(
    State(_state): State<AppState>,
    Path(snapshot_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Err(anyhow::anyhow!("backup not configured").into());
    };
    let repo = cfg.restic_repo("cluster-backup");
    let target = format!("/tmp/yolab-catalog-{}", random_hex(8));

    let restore_out = restic(
        &repo,
        &cfg,
        &[
            "restore",
            &snapshot_id,
            "--target",
            &target,
            "--include",
            "**/catalog.json",
        ],
    )
    .await?;

    if !restore_out.status.success() {
        let _ = tokio::fs::remove_dir_all(&target).await;
        return Err(anyhow::anyhow!(
            "restic restore failed: {}",
            String::from_utf8_lossy(&restore_out.stderr).trim()
        )
        .into());
    }

    let find_out = Command::new("find")
        .args([&target, "-name", "catalog.json", "-type", "f"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("find failed: {e}"))?;

    let file_path = String::from_utf8_lossy(&find_out.stdout).trim().to_string();
    let catalog: serde_json::Value = if file_path.is_empty() {
        serde_json::json!({"namespaces": [], "timestamp": null})
    } else {
        let bytes = tokio::fs::read(&file_path)
            .await
            .map_err(|e| anyhow::anyhow!("read catalog.json: {e}"))?;
        serde_json::from_slice(&bytes)
            .unwrap_or(serde_json::json!({"namespaces": [], "timestamp": null}))
    };

    let _ = tokio::fs::remove_dir_all(&target).await;
    Ok(Json(catalog))
}

/// POST /api/backups/credentials/refresh — re-fetches B2 credentials from
/// yolab-external after a key rotation.
pub async fn refresh_credentials(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if restore::is_running().await {
        return Err(
            anyhow::anyhow!("A restore is in progress — try again once it finishes.").into(),
        );
    }
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };
    refresh_master_config(&url, &token).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/backups/cluster/run-now — manual trigger. Starts one backup set: every
/// VolSync ReplicationSource is triggered and the cluster state is snapshotted, both
/// tagged with the same id.
pub async fn run_backup_now(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    if read_master_config().await.is_none() {
        return Err(anyhow::anyhow!("backup not configured").into());
    }
    let name = backup::start("manual").await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "started": true, "name": name }),
    ))
}

// ── Per-namespace install-time hook ───────────────────────────────────────────

/// Creates the restic secret and ReplicationSource for a single namespace at install
/// time. Called by apps.rs immediately after the namespace is created.
pub async fn setup_namespace_backup(namespace: &str) -> anyhow::Result<()> {
    let Some(cfg) = read_master_config().await else {
        return Ok(()); // backups not enabled — nothing to wire up
    };
    let pvcs = list_user_pvcs().await?;
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await;
        let _ = ensure_replication_source(&pvc, false).await;
    }
    Ok(())
}

/// Background loop: clear restic locks nothing is holding any more.
///
/// A LOCK OUTLIVES THE PROCESS THAT TOOK IT, AND NOTHING CLEANED UP AFTER ONE.
///
/// Every app's PVC has its own restic repository, and a leftover lock on one
/// blocks that app's retention forever: VolSync's mover still uploads the
/// snapshot, then fails at `forget`, and the pod retries in a loop. Observed on
/// 2026-09-11 across babybuddy, code-server and vaultwarden:
///
///   === Starting forget ===
///   unable to create lock in backend: repository is already locked by
///     PID 530299 on node1 by root
///   lock was created at 2026-09-11 13:51:50
///
/// PID 530299 was long dead and no restic process was running anywhere.
///
/// Where those locks came from is the reason `--no-lock` now appears on every
/// read-only `restic snapshots` call in this crate: `app_damage` lists snapshots
/// for EVERY app repo, the home page polls it every 15s while storage looks
/// unhealthy, and each of those listings took a lock. Restart local-api
/// mid-listing — 21 restarts in 30 hours during a day of deploys — and the lock
/// is orphaned. That fix stops new ones; this clears the ones already out there,
/// and covers any future interruption of a lock-taking command.
///
/// `restic unlock` WITHOUT `--remove-all`, deliberately. Plain `unlock` removes
/// only locks restic itself judges stale — the creating process is gone, or the
/// lock has aged out. `--remove-all` would rip out a lock a backup is actively
/// holding, turning a tidy-up into corruption of the run it interrupted. This
/// loop must be safe to run at any moment, including mid-backup, because it
/// does.
pub(crate) async fn run_lock_sweeper() {
    const TICK: std::time::Duration = std::time::Duration::from_secs(1800);
    // After the boot rush, and after the scheduler has had its first look.
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    loop {
        if let Some(cfg) = read_master_config().await {
            // The cluster repo plus one per app PVC — the same set
            // `setup_namespace_backup` wires up, so a new app is covered the
            // moment it has a ReplicationSource.
            cfg.unlock("cluster-backup").await;
            match list_user_pvcs().await {
                Ok(pvcs) => {
                    for pvc in pvcs {
                        let path =
                            format!("volsync/{}/{}", pvc.namespace, canonical_pvc_id(&pvc.name));
                        cfg.unlock(&path).await;
                    }
                }
                Err(e) => {
                    tracing::debug!("lock sweep: could not list PVCs: {e}");
                }
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

// ── "Apps with lost data" triage ───────────────────────────────────────────────

const DATA_LOSS_CM: &str = "yolab-data-loss";
const DATA_LOSS_NS: &str = "kube-system";

async fn data_loss_since() -> chrono::DateTime<chrono::Utc> {
    if let Ok(cm) = crate::kubectl::get_json(&[
        "get",
        "configmap",
        DATA_LOSS_CM,
        "-n",
        DATA_LOSS_NS,
        "-o",
        "json",
    ])
    .await
    {
        if let Some(t) = cm["data"]["detectedAt"].as_str() {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(t) {
                return parsed.with_timezone(&chrono::Utc);
            }
        }
    }
    let now = chrono::Utc::now();
    let manifest = serde_json::json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": DATA_LOSS_CM, "namespace": DATA_LOSS_NS },
        "data": { "detectedAt": now.to_rfc3339() },
    });
    let _ = crate::kubectl::apply(&manifest.to_string()).await;
    now
}

/// `GET /api/backups/damage` — the damaged-apps triage the home page renders.
pub async fn app_damage(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(assess_app_damage().await)
}

async fn assess_app_damage() -> serde_json::Value {
    use chrono::{DateTime, Utc};

    let empty = serde_json::json!({
        "unrecoverable": false, "lost_disks": 0,
        "restorable_count": 0, "delete_count": 0, "apps": [],
    });

    let loss = crate::routers::ceph::assess_pg_loss().await;
    let cephfs_lost = match &loss {
        Some(l) => {
            // `confirmed_lost`, NOT `unrecoverable`: this screen offers to restore
            // from backup, and restoring over data that was only unreadable for a
            // minute overwrites it with something hours older. It must not fire
            // while an OSD is merely down with its disk still in the cluster.
            l.confirmed_lost
                && l.confirmed_lost_pools
                    .iter()
                    .any(|p| p == "yolab-fs-metadata" || p == "yolab-fs-data0")
        }
        None => false,
    };
    if !cephfs_lost {
        return empty;
    }

    let lost_disks = crate::routers::ceph::lost_osd_count().await;
    let loss_since = data_loss_since().await;

    let ns_items = crate::kubectl::get_json(&[
        "get",
        "namespaces",
        "-l",
        "yolab.io/managed=true",
        "-o",
        "json",
    ])
    .await
    .ok()
    .and_then(|v| v["items"].as_array().cloned())
    .unwrap_or_default();

    let mut ns_app_id: HashMap<String, String> = HashMap::new();
    let mut managed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ns in &ns_items {
        let Some(name) = ns["metadata"]["name"].as_str() else {
            continue;
        };
        managed.insert(name.to_string());
        let app_id = ns["metadata"]["annotations"]["yolab.io/app-id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        ns_app_id.insert(name.to_string(), app_id);
    }

    let pvc_items = crate::kubectl::get_json(&["get", "pvc", "-A", "-o", "json"])
        .await
        .ok()
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default();

    let mut pvcs_by_ns: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for pvc in &pvc_items {
        let (Some(ns), Some(name)) = (
            pvc["metadata"]["namespace"].as_str(),
            pvc["metadata"]["name"].as_str(),
        ) else {
            continue;
        };
        if !managed.contains(ns) || name.starts_with("volsync-") {
            continue;
        }
        let created = pvc["metadata"]["creationTimestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let pre_loss = created.map(|t| t < loss_since).unwrap_or(true);
        pvcs_by_ns
            .entry(ns.to_string())
            .or_default()
            .push((name.to_string(), pre_loss));
    }

    let cfg = read_master_config().await;
    let mut checks: Vec<(String, String, String)> = Vec::new();
    let mut affected: Vec<(&String, &Vec<(String, bool)>)> = Vec::new();
    for (ns, pvcs) in &pvcs_by_ns {
        if !pvcs.iter().any(|(_, pre_loss)| *pre_loss) {
            continue;
        }
        affected.push((ns, pvcs));
        if let Some(cfg) = &cfg {
            for (name, _) in pvcs {
                let repo = cfg.restic_repo(&format!("volsync/{ns}/{}", canonical_pvc_id(name)));
                checks.push((ns.clone(), name.clone(), repo));
            }
        }
    }

    let mut results: HashMap<(String, String), Option<DateTime<Utc>>> = HashMap::new();
    if let Some(cfg) = &cfg {
        let probes: Vec<_> = checks
            .iter()
            .map(|(ns, name, repo)| {
                let ns = ns.clone();
                let name = name.clone();
                let repo = repo.clone();
                let cfg = cfg.clone();
                async move {
                    (
                        (ns, name),
                        latest_snapshot_time(&repo, &cfg).await.ok().flatten(),
                    )
                }
            })
            .collect();
        for ((ns, name), latest) in futures::future::join_all(probes).await {
            results.insert((ns, name), latest);
        }
    }

    let mut apps: Vec<serde_json::Value> = Vec::new();
    let mut restorable_count = 0u32;
    let mut delete_count = 0u32;
    for (ns, pvcs) in &affected {
        let mut newest: Option<DateTime<Utc>> = None;
        for (name, _) in pvcs.iter() {
            if let Some(t) = results
                .get(&((*ns).clone(), name.clone()))
                .and_then(|x| x.as_ref())
            {
                newest = Some(match newest {
                    None => *t,
                    Some(cur) => cur.max(*t),
                });
            }
        }
        let restorable = newest.is_some();
        if restorable {
            restorable_count += 1;
        } else {
            delete_count += 1;
        }
        let backup_age_hours =
            newest.map(|t| (chrono::Utc::now() - t).num_seconds() as f64 / 3600.0);
        apps.push(serde_json::json!({
            "namespace": ns,
            "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
            "app_id": ns_app_id.get(ns.as_str()).cloned().unwrap_or_default(),
            "restorable": restorable,
            "backup_age_hours": backup_age_hours,
        }));
    }
    apps.sort_by(|a, b| {
        a["instance_name"]
            .as_str()
            .cmp(&b["instance_name"].as_str())
    });

    serde_json::json!({
        "unrecoverable": true,
        "lost_disks": lost_disks,
        "restorable_count": restorable_count,
        "delete_count": delete_count,
        "apps": apps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_recovery_key_groups_and_uppercases() {
        assert_eq!(
            format_recovery_key("abcdef0123456789"),
            "ABCDE-F0123-45678-9"
        );
    }
}
