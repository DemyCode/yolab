use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

use crate::routers::backup_common::*;
use crate::routers::{backup_run, restore_run};
use crate::{config::Config, error::Result, AppState};

// ── Backup/restore operation state ──────────────────────────────────────────
//
// GET /api/backups/state is the frontend's single source of truth — a page refresh,
// a second tab, or a lost connection should never desync from what's actually
// happening on the backend. Unlike the old ConfigMap-lock design, "is a backup/restore
// running" is now just "does a non-terminal BackupRun/RestoreRun object exist" — see
// backup_run.rs / restore_run.rs for why that can never get stuck the way a flag could.

/// GET /api/backups/state
pub async fn operation_state(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let backup = backup_run::current_status().await;
    let restore = restore_run::current_status().await;
    Ok(Json(serde_json::json!({
        "backing_up": !backup["active"].is_null(),
        "restoring": !restore["active"].is_null(),
        "backup_run": backup["active"],
        "restore_run": restore["active"],
        "last_backup": backup["last"],
        // Staleness, not duration: how long since a restorable backup existed.
        "last_ok_age_hours": backup["last_ok_age_hours"],
        "stale_after_hours": backup["stale_after_hours"],
    })))
}

// ── Config reader ─────────────────────────────────────────────────────────────

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

// ── S3 / SFTP pass-through endpoints ─────────────────────────────────────────

pub async fn get_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "platform API not configured" })));
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
        return Ok(Json(serde_json::json!({ "provisioned": false, "reason": "platform API not configured" })));
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
    Ok(Json(serde_json::json!({ "provisioned": true, "sftp": body })))
}

/// True while any backup/restore activity holds a stake in the restic repos — gates
/// enable/run-now/dr-start so two operations never contend for the same repo lock.
async fn any_operation_in_progress() -> bool {
    backup_run::is_active().await || restore_run::is_active().await || backup_run::volsync_mover_running().await
}

async fn ensure_no_operation_in_progress() -> anyhow::Result<()> {
    if any_operation_in_progress().await {
        anyhow::bail!("A backup or restore is already in progress — try again once it finishes.");
    }
    Ok(())
}

/// POST /api/backups/s3/enable — idempotent: provisions B2, configures VolSync per PVC.
pub async fn enable_s3(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };

    let cfg = ensure_master_config(&url, &token).await?;
    let pvcs = list_user_pvcs().await.unwrap_or_default();

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

/// Formats a hex secret as a human-copyable recovery key: uppercase, hyphenated in
/// groups of 5 (e.g. "A1B2C-3D4E5-..."). This is the restic encryption password itself,
/// not a derivation of it — anyone who has this key can decrypt the B2 backups directly
/// with `restic`, so it must be treated with the same care as the password itself.
fn format_recovery_key(hex: &str) -> String {
    hex.to_uppercase()
        .as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// GET /api/backups/recovery-key
///
/// Surfaces the restic encryption password as a one-time-displayable recovery key.
/// This is the ONLY copy of the key that exists outside of etcd (which is itself
/// backed up encrypted with this same password) — on total machine loss, this is
/// the sole way to decrypt the B2 backups. The frontend is expected to show this
/// once at backup-enable time with an explicit "I've saved this" acknowledgment,
/// and to offer it again on request (e.g. a "View recovery key" action) since a
/// user may need to re-copy it later (new password manager, printed copy lost, etc).
///
/// Deliberately NOT escrowed with yolab-external by default: the whole point of
/// generating this password locally (see `ensure_master_config`) is that yolab-
/// external cannot decrypt the user's backups even with full account access.
/// Escrowing it would be a real product/security tradeoff that needs an explicit,
/// opt-in decision — not something to silently wire up as a side effect of a bug-fix pass.
pub async fn get_recovery_key(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Ok(Json(serde_json::json!({ "configured": false })));
    };
    Ok(Json(serde_json::json!({
        "configured": true,
        "recovery_key": format_recovery_key(&cfg.restic_password),
    })))
}

/// A PVC hasn't synced in this long → flag it as stale rather than silently "Pending" forever.
/// 36 h comfortably exceeds the daily backup cadence plus retry slack.
const STALE_AFTER_HOURS: i64 = 36;

/// GET /api/backups/status — per-PVC VolSync ReplicationSource status + etcd snapshot.
pub async fn backup_status(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let v = get_replication_sources().await;

    // Build a (namespace, pvc_name) → (phase, deletionTimestamp) map from all PVCs.
    let pvc_health_map: HashMap<(String, String), (String, Option<String>)> = Command::new("kubectl")
        .args(["get", "pvc", "-A", "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            let ns = item["metadata"]["namespace"].as_str()?.to_string();
            let name = item["metadata"]["name"].as_str()?.to_string();
            let phase = item["status"]["phase"].as_str().unwrap_or("Unknown").to_string();
            let deletion_ts = item["metadata"]["deletionTimestamp"].as_str().map(String::from);
            Some(((ns, name), (phase, deletion_ts)))
        })
        .collect();

    let pvcs: Vec<serde_json::Value> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|item| {
            let namespace = item["metadata"]["namespace"].as_str().unwrap_or("").to_string();
            let pvc = item["spec"]["sourcePVC"].as_str().unwrap_or("").to_string();
            let created = item["metadata"]["creationTimestamp"].as_str().map(String::from);
            let last_sync_time = item["status"]["lastSyncTime"].as_str().map(String::from);
            let last_sync_duration =
                item["status"]["lastSyncDuration"].as_str().map(String::from);
            let result = item["status"]["latestMoverStatus"]["result"]
                .as_str()
                .unwrap_or(if last_sync_time.is_some() { "Successful" } else { "Pending" })
                .to_string();
            let (pvc_phase, pvc_deletion_ts) = pvc_health_map
                .get(&(namespace.clone(), pvc.clone()))
                .cloned()
                .unwrap_or(("NotFound".to_string(), None));

            // Stale: never synced and this RS has existed longer than the grace window, or
            // its last successful sync is older than the grace window. Either way, a backup
            // that looks "Pending" forever with no visible alert is exactly how a dead backup
            // goes unnoticed until the day it's needed.
            let stale = match &last_sync_time {
                Some(t) => hours_since(t).map_or(true, |h| h > STALE_AFTER_HOURS),
                None => created
                    .as_deref()
                    .and_then(hours_since)
                    .map_or(false, |h| h > STALE_AFTER_HOURS),
            };
            // The PVC has a pending deletion but is still present (finalizer blocking) —
            // the exact state that makes every future backup job permanently unschedulable.
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

    // When cluster state (etcd) was last captured — sourced from the BackupRun that
    // actually included it, not from the transient etcdsnapshotfile CRD (see
    // backup_run::last_etcd_snapshot for why that never worked).
    let etcd_last = backup_run::last_etcd_snapshot().await;

    Ok(Json(serde_json::json!({
        "pvcs": pvcs,
        "etcd_last_snapshot": etcd_last,
        "backup_alert": backup_alert,
    })))
}

// ── Disaster recovery (thin HTTP layer over restore_run.rs) ──────────────────

#[derive(Deserialize, Default)]
pub struct DrStartBody {
    /// Omit to restore from the latest cluster-backup snapshot.
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// Recreate the storage layer before restoring, because it is damaged beyond
    /// repair. Defaults to false and is never inferred: it deletes pools, so it has to
    /// be asked for. The phase itself re-checks against the cluster and refuses if the
    /// data could still come back — this flag grants permission, it does not assert a
    /// fact.
    #[serde(default)]
    pub rebuild_storage: bool,
}

/// POST /api/backups/dr/start — creates a RestoreRun; see restore_run.rs for the
/// Validating → WaitingForStorage → RestoringVolumes → Applying state machine.
pub async fn dr_start(
    State(_state): State<AppState>,
    Json(body): Json<DrStartBody>,
) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;
    if !body.all && body.namespaces.is_empty() {
        return Err(anyhow::anyhow!("specify all:true or a non-empty namespaces[]").into());
    }
    let name = restore_run::start(
        body.snapshot_id,
        body.all,
        body.namespaces,
        body.rebuild_storage,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true, "started": true, "name": name })))
}

/// GET /api/backups/dr/status — the active RestoreRun's full status (phase, per-namespace/
/// per-volume progress), or the most recently finished one.
pub async fn dr_status(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    Ok(Json(restore_run::current_status().await))
}

// ── Cluster backup (thin HTTP layer over backup_run.rs) ──────────────────────

/// GET /api/backups/snapshots — list available cluster-backup restic snapshots.
pub async fn list_snapshots(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Ok(Json(serde_json::json!({ "snapshots": [], "configured": false })));
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let out = Command::new("restic")
        .args(["snapshots", "--json", "--tag", "cluster-backup"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("restic not available: {e}"))?;

    if !out.status.success() {
        // Repo not initialised yet — no snapshots exist.
        return Ok(Json(serde_json::json!({ "snapshots": [], "configured": true })));
    }

    let snapshots: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!([]));

    Ok(Json(serde_json::json!({ "snapshots": snapshots, "configured": true })))
}

/// GET /api/backups/snapshots/:id/catalog
/// Extracts catalog.json from a specific restic cluster-backup snapshot.
pub async fn snapshot_catalog(
    State(_state): State<AppState>,
    Path(snapshot_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let Some(cfg) = read_master_config().await else {
        return Err(anyhow::anyhow!("backup not configured").into());
    };
    let repo = cfg.restic_repo("cluster-backup");
    let target = format!("/tmp/yolab-catalog-{}", random_hex(8));

    let restore_out = Command::new("restic")
        .args(["restore", &snapshot_id, "--target", &target, "--include", "**/catalog.json"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("restic not available: {e}"))?;

    if !restore_out.status.success() {
        let _ = tokio::fs::remove_dir_all(&target).await;
        return Err(anyhow::anyhow!(
            "restic restore failed: {}",
            String::from_utf8_lossy(&restore_out.stderr).trim()
        ).into());
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
        let bytes = tokio::fs::read(&file_path).await
            .map_err(|e| anyhow::anyhow!("read catalog.json: {e}"))?;
        serde_json::from_slice(&bytes)
            .unwrap_or(serde_json::json!({"namespaces": [], "timestamp": null}))
    };

    let _ = tokio::fs::remove_dir_all(&target).await;
    Ok(Json(catalog))
}

/// POST /api/backups/credentials/refresh — re-fetches B2 credentials from
/// yolab-external. Use when backups start failing after a key rotation on the
/// platform side; `ensure_master_config` otherwise caches the original credentials
/// forever once they're first provisioned.
pub async fn refresh_credentials(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured in config.toml").into());
    };
    refresh_master_config(&url, &token).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/backups/cluster/run-now — manual trigger. Creates a BackupRun; see
/// backup_run.rs for the SyncingVolumes → SnapshottingCluster → Pruning state machine.
pub async fn run_backup_now(State(_state): State<AppState>) -> Result<Json<serde_json::Value>> {
    ensure_no_operation_in_progress().await?;
    if read_master_config().await.is_none() {
        return Err(anyhow::anyhow!("backup not configured").into());
    }
    let name = backup_run::start("manual").await?;
    Ok(Json(serde_json::json!({ "ok": true, "started": true, "name": name })))
}

// ── Per-namespace install-time / self-healing hooks ───────────────────────────

/// Creates the restic secret and ReplicationSource for a single namespace at install
/// time. Called by apps.rs immediately after the namespace is created.
/// `namespace` is the raw app namespace (e.g. "yolab-gitea"), not the instance name.
pub async fn setup_namespace_backup(namespace: &str) {
    let Some(cfg) = read_master_config().await else { return };
    let Ok(pvcs) = list_user_pvcs().await else { return };
    for pvc in pvcs.into_iter().filter(|p| p.namespace == namespace) {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await;
        let _ = ensure_replication_source(&pvc, false).await;
    }
}

/// One-time migration: patches the independent `schedule` out of any managed
/// ReplicationSources that still carry the old `0 3 * * *` cron trigger.
/// After this, RSes only fire when the backup job explicitly stamps a `manual` trigger.
async fn strip_rs_schedules() {
    let out = match Command::new("kubectl")
        .args(["get", "replicationsource", "-A",
               "-l", "app.kubernetes.io/managed-by=yolab", "-o", "json"])
        .output().await
    {
        Ok(o) => o,
        Err(_) => return,
    };
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!({"items": []}));
    for item in v["items"].as_array().cloned().unwrap_or_default() {
        let name = item["metadata"]["name"].as_str().unwrap_or("");
        let ns   = item["metadata"]["namespace"].as_str().unwrap_or("");
        if name.is_empty() || ns.is_empty() { continue; }
        if item["spec"]["trigger"]["schedule"].as_str().is_none() { continue; }
        tracing::info!("backup-reconciler: {ns}/{name} — removing legacy schedule trigger");
        let _ = Command::new("kubectl")
            .args([
                "patch", "replicationsource", name, "-n", ns,
                "--type=json",
                r#"-p=[{"op":"remove","path":"/spec/trigger/schedule"}]"#,
            ])
            .output().await;
    }
}

/// Self-healing reconciler — runs hourly to catch any PVCs whose RS/secret was
/// missed at install time (e.g. race between app deploy and local-api restart).
/// Never overwrites a live manual trigger set by the backup job (ensure_replication_source
/// skips PVCs where an RS already exists when trigger_now=false).
pub async fn run_replication_source_reconciler() {
    tokio::time::sleep(Duration::from_secs(120)).await;
    // Migrate any pre-existing RSes that still carry an independent schedule.
    strip_rs_schedules().await;
    loop {
        if let Some(cfg) = read_master_config().await {
            if let Ok(pvcs) = list_user_pvcs().await {
                for pvc in pvcs {
                    annotate_ns_privileged_movers(&pvc.namespace).await;
                    if let Err(e) = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await {
                        tracing::debug!("backup-reconciler: restic secret {}/{}: {e}", pvc.namespace, pvc.name);
                    }
                    if let Err(e) = ensure_replication_source(&pvc, false).await {
                        tracing::debug!("backup-reconciler: RS {}/{}: {e}", pvc.namespace, pvc.name);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_recovery_key_groups_and_uppercases() {
        assert_eq!(format_recovery_key("abcdef0123456789"), "ABCDE-F0123-45678-9");
    }
}
