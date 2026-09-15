use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use tokio::process::Command;

use crate::error::Outcome;
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
    // `?` on the check itself: not knowing whether a restore runs is not "no".
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

/// GET /api/backups/state — the frontend's single source of truth for what is running.
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

/// GET /api/backups/runs — every recorded backup set, newest first, in three states
/// (`running`, `restorable`, `crashed`).
pub async fn list_runs(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(backup::list().await?)))
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
        .await?
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
pub async fn list_restores(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::Value::Array(restore::list().await?)))
}

// ── Add from backup ────────────────────────────────────────────────────────────

/// An app being added from backup by this process, and how it went.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
struct Adding {
    snapshot_id: String,
    /// None while it runs.
    error: Option<String>,
    done: bool,
}

/// Adds started from this machine, by namespace. In memory: an add is one long
/// request's worth of work, and the app it produces is its lasting record.
static ADDING: std::sync::Mutex<std::collections::BTreeMap<String, Adding>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn adding() -> std::collections::BTreeMap<String, Adding> {
    ADDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn set_adding(namespace: &str, state: Adding) {
    ADDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(namespace.to_string(), state);
}

/// Every app in any backup: whether it is installed, whether an add is running
/// or failed here, and each point in time it can come back from, newest first.
fn backed_up_apps_json(
    versions: &restore::BackupVersions,
    installed: &HashSet<String>,
    adding: &std::collections::BTreeMap<String, Adding>,
) -> serde_json::Value {
    let apps: Vec<serde_json::Value> = versions
        .apps
        .iter()
        .map(|(ns, vs)| {
            serde_json::json!({
                "namespace": ns,
                "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
                "installed": installed.contains(ns),
                "adding": adding.get(ns),
                "versions": vs,
            })
        })
        .collect();
    serde_json::json!({ "configured": versions.configured, "apps": apps })
}

/// GET /api/backups/apps
pub async fn list_backed_up_apps(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>> {
    let versions = restore::backup_versions().await?;
    let installed: HashSet<String> = list_managed_namespaces().await?.into_iter().collect();
    Ok(Json(backed_up_apps_json(&versions, &installed, &adding())))
}

#[derive(Deserialize)]
pub struct AddFromBackupRequest {
    pub namespace: String,
    pub snapshot_id: String,
}

/// Why this add cannot start, if it cannot.
fn add_refusal(
    request: &AddFromBackupRequest,
    versions: &restore::BackupVersions,
    installed: &HashSet<String>,
    adding: &std::collections::BTreeMap<String, Adding>,
) -> Option<String> {
    let ns = &request.namespace;
    if adding.get(ns).is_some_and(|a| !a.done) {
        return Some(format!("{ns} is already being added"));
    }
    if installed.contains(ns) {
        return Some(format!("{ns} is already installed — uninstall it first"));
    }
    let held = versions
        .apps
        .get(ns)
        .is_some_and(|vs| vs.iter().any(|v| v.snapshot_id == request.snapshot_id));
    if !held {
        return Some(format!("no backup {} holds {ns}", request.snapshot_id));
    }
    None
}

/// POST /api/backups/apps/add — installs an app from one of its backups: its
/// chart with the settings it had then, its files, and its saved objects. Starts
/// the work and returns; `GET /api/backups/apps` reports how it went.
pub async fn add_from_backup(
    State(_state): State<AppState>,
    Json(request): Json<AddFromBackupRequest>,
) -> Result<Json<serde_json::Value>> {
    let versions = restore::backup_versions().await?;
    let installed: HashSet<String> = list_managed_namespaces().await?.into_iter().collect();
    if let Some(why) = add_refusal(&request, &versions, &installed, &adding()) {
        return Err(anyhow::anyhow!(why).into());
    }
    let running = Adding {
        snapshot_id: request.snapshot_id.clone(),
        error: None,
        done: false,
    };
    set_adding(&request.namespace, running.clone());
    tokio::spawn(async move {
        let result = restore::reinstall_from_backup(&request.namespace, &request.snapshot_id).await;
        if let Err(e) = &result {
            tracing::warn!(
                "add {} from backup {}: {e:#}",
                request.namespace,
                request.snapshot_id
            );
        }
        set_adding(
            &request.namespace,
            Adding {
                error: result.err().map(|e| format!("{e:#}")),
                done: true,
                ..running
            },
        );
    });
    Ok(Json(serde_json::json!({ "ok": true, "started": true })))
}

// ── Cluster backup ─────────────────────────────────────────────────────────────

/// Cluster snapshot ids that actually contain `namespace`.
///
/// A RESTORE IS ONLY MEANINGFUL FOR A POINT IN TIME THE APP EXISTED AT.
///
/// `restore_inner` drives an app restore off a CLUSTER snapshot: it extracts
/// that snapshot's `<namespace>.yaml` to rebuild the app's objects, then rolls
/// each PVC back to the snapshot's timestamp. A snapshot taken before the app
/// was installed has no such file, so restoring from it fails — and offering it
/// is worse than useless, because the person picking it has been told it is a
/// point they can go back to.
///
/// The restore dialog listed every cluster backup regardless, including ones
/// from before the app existed.
///
/// One `restic find` across the whole repository rather than a per-snapshot
/// probe: the alternative is one process per snapshot, and this runs while
/// somebody waits for a dialog to open.
///
/// `None` means the question could not be answered — the caller then shows
/// everything rather than pretending an app has no restore points, since a
/// wrong "no backups exist" reads as data loss.
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
                // An entry with no matches is reported for some restic versions;
                // treat only a real hit as the app being present.
                e["matches"].as_array().is_some_and(|m| !m.is_empty())
            })
            .filter_map(|e| e["snapshot"].as_str().map(str::to_string))
            .collect(),
    )
}

/// GET /api/backups/snapshots — cluster-backup restic snapshots.
///
/// `?namespace=yolab-foo` narrows the list to the points in time that app can
/// actually be restored to; see `snapshots_containing`.
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
        // Only when the question could be answered. On failure every snapshot is
        // still offered: a wrong "no backups exist" reads as data loss.
        if let Some(ids) = snapshots_containing(&cfg, ns).await {
            if let Some(arr) = snapshots.as_array() {
                let kept: Vec<serde_json::Value> = arr
                    .iter()
                    .filter(|s| {
                        s["short_id"]
                            .as_str()
                            .or_else(|| s["id"].as_str())
                            .is_some_and(|id| {
                                // restic reports short ids in `find`, full ids in
                                // `snapshots`; match either way round.
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
        tokio::fs::remove_dir_all(&target)
            .await
            .debug_on_err("clean up a failed catalog extraction");
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

    tokio::fs::remove_dir_all(&target)
        .await
        .debug_on_err("clean up the extracted snapshot catalog");
    Ok(Json(catalog))
}

/// POST /api/backups/credentials/refresh — re-fetches B2 credentials from
/// yolab-external after a key rotation.
pub async fn refresh_credentials(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    // `?` on the check itself: not knowing whether a restore runs is not "no".
    if restore::running_anywhere().await? {
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
        // `?`: an app installed without its backup wiring would sit "backed up" on
        // the page with nothing behind it. The install reports the failure instead.
        ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await?;
        // ADOPTING A REPOSITORY IS THE MOMENT TO CLEAR A LOCK NOBODY OWNS.
        //
        // A repo is keyed by namespace and PVC name, so REINSTALLING an app lands
        // on the same one its predecessor used — deliberately, since that keeps
        // the backup history. It also inherits whatever that predecessor left
        // behind. A fresh yolab-filebrowser install on 2026-09-11 picked up a
        // lock held by a mover pod that had died the previous day:
        //
        //   repository is already locked by PID 46 on
        //     volsync-src-volsync-filebrowser-data-jbz7g
        //   lock was created at 2026-09-10 14:08:42 (25h12m ago)
        //
        // Every backup then uploaded its snapshot and failed at `forget`. The
        // periodic sweeper would clear it, but only on its next half-hourly pass,
        // and its previous one ran before this app existed — so a newly installed
        // app reports failing backups for up to thirty minutes for no reason the
        // owner can see.
        //
        // Stale locks only (plain `unlock`, never `--remove-all`), so this cannot
        // disturb a backup that is genuinely running.
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
/// read-only `restic snapshots` call in this crate: `app_damage` (since removed) listed snapshots
/// for EVERY app repo, the home page polled it every 15s while storage looked
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
pub struct LockSweeperController;

impl crate::runtime::Controller for LockSweeperController {
    fn name(&self) -> &'static str {
        "backup-lock-sweeper"
    }
    fn scope(&self) -> crate::runtime::Scope {
        // One node sweeps: the repositories are shared, not per machine.
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1800)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    fn not_before_uptime(&self) -> std::time::Duration {
        // After the boot rush, and after the scheduler has had its first look.
        std::time::Duration::from_secs(300)
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let Some(cfg) = read_master_config().await else {
            return Ok(crate::runtime::Tick::Idle("backups are not enabled".into()));
        };
        // The cluster repo plus one per app PVC — the same set
        // `setup_namespace_backup` wires up, so a new app is covered the moment
        // it has a ReplicationSource.
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

    fn add(ns: &str, snap: &str) -> AddFromBackupRequest {
        AddFromBackupRequest {
            namespace: ns.into(),
            snapshot_id: snap.into(),
        }
    }

    #[test]
    fn an_app_is_added_only_from_a_backup_that_holds_it_and_only_when_absent() {
        let none = std::collections::BTreeMap::new();
        let installed: HashSet<String> = ["yolab-b".to_string()].into();
        assert_eq!(
            add_refusal(&add("yolab-a", "old"), &versions(), &installed, &none),
            None
        );
        assert!(
            add_refusal(&add("yolab-a", "gone"), &versions(), &installed, &none)
                .unwrap()
                .contains("no backup gone")
        );
        assert!(
            add_refusal(&add("yolab-b", "old"), &versions(), &installed, &none)
                .unwrap()
                .contains("already installed")
        );
        let running = std::collections::BTreeMap::from([(
            "yolab-a".to_string(),
            Adding {
                snapshot_id: "old".into(),
                error: None,
                done: false,
            },
        )]);
        assert!(
            add_refusal(&add("yolab-a", "new"), &versions(), &installed, &running)
                .unwrap()
                .contains("already being added")
        );
        let failed = std::collections::BTreeMap::from([(
            "yolab-a".to_string(),
            Adding {
                snapshot_id: "old".into(),
                error: Some("helm".into()),
                done: true,
            },
        )]);
        assert_eq!(
            add_refusal(&add("yolab-a", "new"), &versions(), &installed, &failed),
            None,
            "a failed add can be retried"
        );
    }

    #[test]
    fn the_list_shows_each_backed_up_app_with_its_state_and_versions() {
        let installed: HashSet<String> = ["yolab-b".to_string()].into();
        let adding = std::collections::BTreeMap::from([(
            "yolab-a".to_string(),
            Adding {
                snapshot_id: "new".into(),
                error: None,
                done: false,
            },
        )]);
        let v = backed_up_apps_json(&versions(), &installed, &adding);
        assert_eq!(v["configured"], true);
        assert_eq!(
            v["apps"][0],
            serde_json::json!({
                "namespace": "yolab-a",
                "instance_name": "a",
                "installed": false,
                "adding": {"snapshot_id": "new", "error": null, "done": false},
                "versions": [
                    {"snapshot_id": "new", "time": "2026-09-14T02:00:00Z"},
                    {"snapshot_id": "old", "time": "2026-09-14T02:00:00Z"},
                ],
            })
        );
        assert_eq!(v["apps"][1]["installed"], true);
        assert_eq!(v["apps"][1]["adding"], serde_json::Value::Null);
    }

    #[test]
    fn format_recovery_key_groups_and_uppercases() {
        assert_eq!(
            format_recovery_key("abcdef0123456789"),
            "ABCDE-F0123-45678-9"
        );
    }
}
