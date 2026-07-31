// BackupRun reconciler — replaces the old ConfigMap-lock-guarded run_cluster_backup +
// trigger_and_wait_volsync + do_cluster_backup + LAST_BACKUP_RESULT_FILE.
//
// Every backup is now a `BackupRun` custom resource that moves through explicit phases,
// each carrying a deadline:
//
//   Pending → SyncingVolumes → SnapshottingCluster → Pruning → Succeeded|Partial|Failed
//
// Why this over a boolean/ConfigMap flag: a flag can only be cleared by whoever set it —
// when that crashes (OOM, reboot, a dropped tokio task), the flag outlives reality and
// nothing can safely clear it. A BackupRun's phase+deadline can always be recomputed by
// any reconcile tick, on any process, after any crash: past the deadline and still
// non-terminal simply means "this run failed", full stop — there is no "wait, maybe it's
// still going" ambiguity to get wrong.
//
// Scheduling is wall-clock derived ("no run succeeded in the last 24h and none is active
// → start one") instead of a single `tokio::time::sleep` until 02:00 UTC — the latter
// uses CLOCK_MONOTONIC, which does not advance across laptop suspend, so a suspended
// machine could miss its nightly backup indefinitely.

use crate::kubectl::Crd;
use crate::lease;
use crate::routers::backup_common::*;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

pub(crate) const BACKUP_RUN: Crd = Crd {
    group: "yolab.io",
    version: "v1alpha1",
    plural: "backupruns",
    kind: "BackupRun",
};

const LEASE_NAME: &str = "yolab-backup-reconciler";
const LEASE_DURATION_SECS: i64 = 90;
const RECONCILE_TICK_SECS: u64 = 20;
const CATCHUP_AFTER_HOURS: i64 = 24;
/// Terminal BackupRuns to keep — old ones are pure history (their B2 restic snapshot
/// is what retention (Pruning phase) actually governs), so this just bounds how many
/// small status objects accumulate in etcd.
const KEEP_TERMINAL_RUNS: usize = 30;

const PHASE_PENDING: &str = "Pending";
const PHASE_SYNCING: &str = "SyncingVolumes";
const PHASE_SNAPSHOTTING: &str = "SnapshottingCluster";
const PHASE_PRUNING: &str = "Pruning";
const PHASE_SUCCEEDED: &str = "Succeeded";
const PHASE_PARTIAL: &str = "Partial";
const PHASE_FAILED: &str = "Failed";

fn is_terminal(phase: &str) -> bool {
    matches!(phase, PHASE_SUCCEEDED | PHASE_PARTIAL | PHASE_FAILED)
}

fn deadline_after(secs: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

fn parse_deadline(v: &Value) -> Option<DateTime<Utc>> {
    v["status"]["phaseDeadline"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// True if any BackupRun is currently non-terminal — the single-flight gate for both
/// the reconciler's own scheduling decision and the manual run-now endpoint.
pub async fn is_active() -> bool {
    BACKUP_RUN.list().await.iter().any(|r| {
        let phase = r["status"]["phase"].as_str().unwrap_or(PHASE_PENDING);
        !is_terminal(phase)
    })
}

/// Also true while VolSync is actively pushing PVC data even outside of a BackupRun
/// (e.g. an old mover still finishing) — kept as a belt-and-suspenders check so a
/// stray mover pod can't overlap with an unrelated restic operation on the same repo.
pub async fn volsync_mover_running() -> bool {
    Command::new("kubectl")
        .args([
            "get", "pods", "-A",
            "-l", "app.kubernetes.io/created-by=volsync",
            "--field-selector=status.phase=Running",
            "-o", "name",
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.contains("volsync-src-")))
        .unwrap_or(false)
}

/// Creates a new BackupRun and spawns its execution. Returns the run's name.
pub async fn start(triggered_by: &str) -> anyhow::Result<String> {
    let name = format!("backup-{}", Utc::now().format("%Y%m%d%H%M%S"));
    BACKUP_RUN.create(&name, json!({ "triggeredBy": triggered_by }), &[("app.kubernetes.io/managed-by", "yolab")]).await?;
    BACKUP_RUN.patch_status(&name, json!({
        "phase": PHASE_PENDING,
        "phaseDeadline": deadline_after(30),
        "startedAt": Utc::now().to_rfc3339(),
    })).await?;
    tokio::spawn(execute(name.clone()));
    Ok(name)
}

/// True if this run has already been finalized by someone else — specifically,
/// reconcile_tick's timeout sweep marking it Failed because this executor blew its
/// phase deadline. Without this check, a slow-but-still-alive executor would keep
/// patching phases after being timed out, potentially clobbering the Failed status
/// and racing a second run the reconciler started once it saw no active run.
async fn is_superseded(name: &str) -> bool {
    match BACKUP_RUN.get(name).await {
        Some(run) => is_terminal(run["status"]["phase"].as_str().unwrap_or("")),
        None => true, // run deleted out from under us (pruned) — definitely stop
    }
}

/// The full phase-by-phase backup, patching BackupRun.status at every transition.
/// If this task dies mid-run (process killed), the run is left non-terminal past its
/// phaseDeadline — reconcile_tick's timeout sweep is what turns that into a clean
/// `Failed` instead of leaving it stuck forever.
async fn execute(name: String) {
    let Some(cfg) = read_master_config().await else {
        let _ = BACKUP_RUN.patch_status(&name, json!({
            "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
            "error": "backup not configured",
        })).await;
        return;
    };

    // ── SyncingVolumes ──────────────────────────────────────────────────────
    let pvcs = list_user_pvcs().await.unwrap_or_default();
    let since = Utc::now();
    let sync_deadline = Utc::now() + chrono::Duration::seconds(1800);
    let pvc_status: Vec<Value> = pvcs.iter()
        .map(|p| json!({ "namespace": p.namespace, "name": p.name, "phase": "Syncing" }))
        .collect();
    let _ = BACKUP_RUN.patch_status(&name, json!({
        "phase": PHASE_SYNCING,
        "phaseDeadline": sync_deadline.to_rfc3339(),
        "syncSince": since.to_rfc3339(),
        "pvcs": pvc_status,
    })).await;

    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, &cfg).await;
        let _ = ensure_replication_source(pvc, true).await;
    }

    let stale = poll_volsync_sync(&name, &pvcs, since, sync_deadline).await;

    if is_superseded(&name).await {
        tracing::warn!("backup-run {name}: superseded (timed out) after SyncingVolumes — stopping");
        return;
    }

    // ── SnapshottingCluster ──────────────────────────────────────────────────
    const SNAPSHOTTING_BUDGET_SECS: u64 = 900;
    let _ = BACKUP_RUN.patch_status(&name, json!({
        "phase": PHASE_SNAPSHOTTING,
        "phaseDeadline": deadline_after(SNAPSHOTTING_BUDGET_SECS as i64),
    })).await;

    // Enforced here, not just observed after the fact by reconcile_tick's deadline sweep —
    // otherwise the phaseDeadline patched above is purely aspirational (snapshot_cluster
    // has no internal bound of its own) and a slow restic upload could run concurrently
    // with a second run the reconciler starts once it times this one out.
    let snapshot_result = tokio::time::timeout(
        Duration::from_secs(SNAPSHOTTING_BUDGET_SECS),
        snapshot_cluster(&cfg),
    ).await;
    let snapshot_id = match snapshot_result {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            let _ = BACKUP_RUN.patch_status(&name, json!({
                "phase": PHASE_FAILED,
                "finishedAt": Utc::now().to_rfc3339(),
                "error": e.to_string(),
                "stalePvcs": stale,
            })).await;
            return;
        }
        Err(_) => {
            let _ = BACKUP_RUN.patch_status(&name, json!({
                "phase": PHASE_FAILED,
                "finishedAt": Utc::now().to_rfc3339(),
                "error": format!("snapshot step exceeded its {SNAPSHOTTING_BUDGET_SECS}s budget"),
                "stalePvcs": stale,
            })).await;
            return;
        }
    };

    if is_superseded(&name).await {
        tracing::warn!("backup-run {name}: superseded (timed out) after SnapshottingCluster — stopping");
        return;
    }

    // ── Pruning ───────────────────────────────────────────────────────────────
    let _ = BACKUP_RUN.patch_status(&name, json!({
        "phase": PHASE_PRUNING,
        "phaseDeadline": deadline_after(300),
        "snapshotId": snapshot_id,
    })).await;

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    // --group-by tags (not the default host,paths): the staging dir is unique per run
    // so grouping by paths put every snapshot in its own group of one and nothing was
    // ever pruned. Every snapshot shares the "cluster-backup" tag.
    let forget = Command::new("restic")
        .args(["forget", "--tag", "cluster-backup", "--group-by", "tags",
               "--keep-daily", "7", "--keep-weekly", "4", "--keep-monthly", "12", "--prune"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await;
    if let Ok(o) = &forget {
        if !o.status.success() {
            tracing::warn!("backup-run {name}: forget/prune failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
    }

    // ── Terminal ──────────────────────────────────────────────────────────────
    let phase = if stale.is_empty() { PHASE_SUCCEEDED } else { PHASE_PARTIAL };
    let _ = BACKUP_RUN.patch_status(&name, json!({
        "phase": phase,
        "finishedAt": Utc::now().to_rfc3339(),
        "stalePvcs": stale,
    })).await;
    tracing::info!("backup-run {name}: {phase} (snapshot {snapshot_id})");
}

/// Polls each PVC's ReplicationSource until Successful-after-`since` or the phase
/// deadline passes, updating BackupRun.status.pvcs as it goes so the frontend can show
/// live per-volume progress instead of a single "syncing…" spinner. Returns the PVCs
/// that never reached a fresh Successful (the "partial backup" list).
async fn poll_volsync_sync(
    name: &str,
    pvcs: &[PvcInfo],
    since: DateTime<Utc>,
    deadline: DateTime<Utc>,
) -> Vec<String> {
    if pvcs.is_empty() {
        return Vec::new();
    }
    loop {
        let rs = get_replication_sources().await;
        let mut pvc_status = Vec::with_capacity(pvcs.len());
        let mut stale = Vec::new();
        let mut all_done = true;

        for pvc in pvcs {
            let cid = canonical_pvc_id(&pvc.name);
            let rs_name = format!("volsync-{cid}");
            let item = rs["items"].as_array().and_then(|items| {
                items.iter().find(|i| {
                    i["metadata"]["name"].as_str() == Some(&rs_name)
                        && i["metadata"]["namespace"].as_str() == Some(&pvc.namespace)
                })
            });
            let result = item.and_then(|i| i["status"]["latestMoverStatus"]["result"].as_str());
            let synced_after_trigger = item
                .and_then(|i| i["status"]["lastSyncTime"].as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc) >= since)
                .unwrap_or(false);

            let phase = if result == Some("Successful") && synced_after_trigger {
                "Synced"
            } else if Utc::now() > deadline {
                stale.push(format!("{}/{}", pvc.namespace, pvc.name));
                "TimedOut"
            } else {
                all_done = false;
                "Syncing"
            };
            pvc_status.push(json!({ "namespace": pvc.namespace, "name": pvc.name, "phase": phase }));
        }

        let _ = BACKUP_RUN.patch_status(name, json!({ "pvcs": pvc_status })).await;

        if all_done || Utc::now() > deadline {
            return stale;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Cluster-metadata snapshot: etcd + k8s objects + catalog.json, uploaded to a FIXED
/// staging path (not date-suffixed, unlike the pre-operator version) so every run's
/// restic snapshot shares the same `paths`, which is what makes `--group-by tags`
/// retention actually able to bucket them together. Returns the snapshot date/id.
///
/// This directory holds every Secret in every yolab-managed namespace in plaintext
/// (etcd.db + the sanitized k8s object export) while staging, however briefly — it must
/// never be left world-readable, and it must never be left behind on a failure exit.
/// Both are handled by this thin wrapper rather than the several `?`-early-return points
/// inside `snapshot_cluster_inner`, so no future new failure path can reintroduce either
/// problem by forgetting a cleanup call at its own return site.
async fn snapshot_cluster(cfg: &BackupConfig) -> anyhow::Result<String> {
    let tmp_dir = "/var/lib/yolab/backup-staging".to_string();

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    tokio::fs::create_dir_all(&tmp_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let result = snapshot_cluster_inner(cfg, &tmp_dir).await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    result
}

async fn snapshot_cluster_inner(cfg: &BackupConfig, tmp_dir: &str) -> anyhow::Result<String> {
    let date = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let repo = cfg.restic_repo("cluster-backup");

    // 1. etcd snapshot — archived as etcd.db in this restic snapshot.
    //    NOTE: etcd.db is NOT consumed by RestoreRun. It is used exclusively by the
    //    external dr-restore.sh script, which runs before K3s/local-api are started
    //    and restores the etcd database directly.
    let snap_name = format!("yolab-cluster-{date}");
    let snap_saved = Command::new("k3s")
        .args(["etcd-snapshot", "save", &format!("--name={snap_name}")])
        .output().await;

    match snap_saved {
        Ok(o) if o.status.success() => {
            let snap_dir = "/var/lib/rancher/k3s/server/db/snapshots";
            if let Ok(entries) = std::fs::read_dir(snap_dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name();
                    let fname_str = fname.to_string_lossy();
                    if fname_str.starts_with(&snap_name) {
                        let dst = format!("{tmp_dir}/etcd.db");
                        if let Err(e) = std::fs::copy(entry.path(), &dst) {
                            tracing::warn!("cluster-backup: copy etcd snapshot: {e}");
                        } else {
                            let _ = std::fs::remove_file(entry.path());
                        }
                        let _ = Command::new("kubectl")
                            .args(["delete", "etcdsnapshotfile", &fname_str.to_string(), "--ignore-not-found"])
                            .output().await;
                        break;
                    }
                }
            }
        }
        Ok(o) => tracing::warn!("cluster-backup: etcd-snapshot: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => tracing::warn!("cluster-backup: k3s unavailable: {e}"),
    }

    // 2. Export K8s objects for all yolab-managed namespaces.
    let namespaces = list_managed_namespaces().await;
    for ns in &namespaces {
        let obj_out = Command::new("kubectl")
            .args(["get", "deploy,svc,secret,configmap",
                   "-n", ns, "-o", "json", "--ignore-not-found"])
            .output().await;
        if let Ok(obj) = obj_out {
            if let Ok(v) = serde_json::from_slice::<Value>(&obj.stdout) {
                let raw = v["items"].as_array().cloned().unwrap_or_default();
                let sanitized = sanitize_k8s_items_for_backup(&raw);
                if !sanitized.is_empty() {
                    let list = json!({ "apiVersion": "v1", "kind": "List", "items": sanitized });
                    if let Ok(s) = serde_json::to_string_pretty(&list) {
                        let _ = tokio::fs::write(format!("{tmp_dir}/{ns}.yaml"), s.as_bytes()).await;
                    }
                }
            }
        }
    }

    // 3. catalog.json — per-namespace PVC info so the restore UI can show service
    //    names, PVC counts, and storage sizes without extra API calls.
    let mut services: Vec<Value> = Vec::new();
    for ns in &namespaces {
        let pvc_out = Command::new("kubectl")
            .args(["get", "pvc", "-n", ns, "-o", "json"])
            .output().await;
        let pvcs: Vec<Value> = pvc_out
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                let name = item["metadata"]["name"].as_str()?.to_string();
                if name.starts_with("volsync-") {
                    return None;
                }
                let capacity = item["spec"]["resources"]["requests"]["storage"]
                    .as_str().unwrap_or("?").to_string();
                Some(json!({ "name": name, "capacity": capacity }))
            })
            .collect();
        services.push(json!({ "namespace": ns, "pvcs": pvcs }));
    }
    let total_pvc_bytes: u64 = services.iter()
        .flat_map(|s| s["pvcs"].as_array().cloned().unwrap_or_default())
        .map(|p| parse_capacity_bytes(p["capacity"].as_str().unwrap_or("0")))
        .sum();
    let catalog = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "namespaces": namespaces,
        "services": services,
        "total_pvc_bytes": total_pvc_bytes,
    });
    let _ = tokio::fs::write(format!("{tmp_dir}/catalog.json"), catalog.to_string()).await;

    // 4. Init restic repo if needed.
    cfg.unlock("cluster-backup").await;
    let check = Command::new("restic").args(["snapshots"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await;
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        let init = Command::new("restic").args(["init"])
            .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
            .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
            .output().await?;
        if !init.status.success() {
            anyhow::bail!("restic init failed: {}", String::from_utf8_lossy(&init.stderr).trim());
        }
    }

    // 5. Backup. kill_on_drop: this is the one step that can genuinely run long (a full
    // B2 upload); if the caller's tokio::time::timeout fires, dropping this future must
    // actually kill the restic process rather than orphan it still holding the repo lock.
    let backup = Command::new("restic")
        .kill_on_drop(true)
        .args(["backup", tmp_dir, "--tag", "cluster-backup"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await?;

    if !backup.status.success() {
        anyhow::bail!("restic backup failed: {}", String::from_utf8_lossy(&backup.stderr).trim());
    }

    tracing::info!("cluster-backup: snapshot complete ({date})");
    Ok(date)
}

/// Extracts `.status` from a listed CRD item and folds in `metadata.name` — callers
/// (the frontend) want the run's name + status fields flattened, not the full
/// apiVersion/kind/metadata/spec/status envelope `Crd::list`/`get` return.
fn flatten_status(item: &Value) -> Value {
    let mut status = item["status"].clone();
    if !status.is_object() {
        status = json!({});
    }
    if let Some(name) = item["metadata"]["name"].as_str() {
        status["name"] = Value::String(name.to_string());
    }
    status
}

/// GET /api/backups/state consumes this: the active run's live phase/progress if one
/// exists, plus the most recently finished run's terminal summary either way.
pub async fn current_status() -> Value {
    let runs = BACKUP_RUN.list().await; // newest-created first
    let active = runs.iter().find(|r| {
        let phase = r["status"]["phase"].as_str().unwrap_or(PHASE_PENDING);
        !is_terminal(phase)
    }).map(flatten_status);
    let last_finished = runs.iter().find(|r| {
        is_terminal(r["status"]["phase"].as_str().unwrap_or(""))
    }).map(flatten_status);
    json!({ "active": active, "last": last_finished })
}

/// Reconcile tick: only the lease holder acts, so at most one process (today: always
/// true since there's one node; matters once there are 2-3) decides "time out this run"
/// or "start a new one" on any given tick.
pub async fn reconcile_tick(holder: &str) {
    let Some(_guard) = lease::acquire(LEASE_NAME, holder, LEASE_DURATION_SECS).await else {
        return;
    };

    // Time out any run stuck past its phase deadline — a crashed process or a hung
    // kubectl/restic call must not block every future backup forever.
    for run in BACKUP_RUN.list().await {
        let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_PENDING).to_string();
        if is_terminal(&phase) {
            continue;
        }
        if let Some(dl) = parse_deadline(&run) {
            if Utc::now() > dl {
                let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
                tracing::warn!("backup-run {name}: timed out in phase {phase}");
                let _ = BACKUP_RUN.patch_status(&name, json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": format!("timed out in phase {phase}"),
                })).await;
            }
        }
    }

    prune_old_runs().await;

    if is_active().await || volsync_mover_running().await {
        return;
    }
    if crate::routers::restore_run::is_active().await {
        return; // never start a backup while a restore is in flight
    }

    let Some(_cfg) = read_master_config().await else {
        return; // backups not enabled
    };

    let runs = BACKUP_RUN.list().await;
    let last_ok_age_hours = runs.iter()
        .find(|r| matches!(r["status"]["phase"].as_str(), Some(PHASE_SUCCEEDED) | Some(PHASE_PARTIAL)))
        .and_then(|r| r["status"]["finishedAt"].as_str())
        .and_then(hours_since);

    let due = match last_ok_age_hours {
        Some(h) => h >= CATCHUP_AFTER_HOURS,
        None => true, // never backed up successfully — do it now
    };
    if due {
        match start("schedule").await {
            Ok(name) => tracing::info!("backup-run {name}: started (schedule)"),
            Err(e) => tracing::warn!("backup-run: failed to start scheduled run: {e}"),
        }
    }
}

/// Deletes terminal BackupRuns beyond the most recent `KEEP_TERMINAL_RUNS` — otherwise
/// every backup ever run (daily, forever) leaves a small object behind permanently.
async fn prune_old_runs() {
    let mut seen_terminal = 0usize;
    for run in BACKUP_RUN.list().await { // newest-created first
        if !is_terminal(run["status"]["phase"].as_str().unwrap_or("")) {
            continue;
        }
        seen_terminal += 1;
        if seen_terminal > KEEP_TERMINAL_RUNS {
            if let Some(name) = run["metadata"]["name"].as_str() {
                BACKUP_RUN.delete(name).await;
            }
        }
    }
}

/// Background loop — replaces run_cluster_backup/run_restore_reconciler's separate
/// timers with one tick that drives both BackupRun and RestoreRun reconciliation.
pub async fn run(holder: String) {
    // Give K3s and Ceph time to settle after (re)start before the first tick.
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        reconcile_tick(&holder).await;
        crate::routers::restore_run::reconcile_tick(&holder).await;
        tokio::time::sleep(Duration::from_secs(RECONCILE_TICK_SECS)).await;
    }
}
