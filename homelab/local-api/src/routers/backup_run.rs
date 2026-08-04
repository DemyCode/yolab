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
// ── Level-triggered, not edge-triggered ──────────────────────────────────────────────
//
// Earlier versions of this reconciler still had a bug class the phase/deadline design
// was supposed to eliminate: each BackupRun was driven by a single `tokio::spawn`ed task
// that ran the whole thing start-to-finish, in-process, with internal `sleep()` polling
// loops lasting up to 30 minutes. If the process restarted (a crash, an OOM, a reboot —
// exactly what happened live once, taking two apps down until someone noticed), that
// task just vanished. The phase/deadline system caught it *eventually* (once the
// deadline passed) but had no way to actually finish the work — it could only mark the
// run Failed, because "resume this specific in-flight task" was never a thing that
// existed anywhere outside that one process's memory.
//
// This version has no such task. `start()` only creates the object; there is no spawn.
// The reconcile tick (`run()`, firing every RECONCILE_TICK_SECS) is the *only* thing
// that ever touches a BackupRun's status, and on every tick it calls `step()` once for
// every non-terminal run: read the CRD's current phase, do exactly one bounded unit of
// work for that phase, and return. No sleeping inside `step()` for anything longer than
// a single phase's bounded operation. All state that matters lives in the CRD status,
// never in a task's stack — so it does not matter *at all* whether the process driving
// tick N is the same process that drove tick N-1. A crash between any two ticks just
// means the next tick (this process restarted, or a fresh one) reads the same status
// and keeps going, because every phase's work is built from operations that are safe to
// redo (the `ensure_*` helpers, idempotent `kubectl apply`, restic commands that are
// safe to re-run) — redoing a phase from scratch after a crash is always safe, even if
// it wastes some partial progress.
//
// Scheduling is wall-clock derived ("no run succeeded in the last 24h and none is active
// → start one") instead of a single `tokio::time::sleep` until 02:00 UTC — the latter
// uses CLOCK_MONOTONIC, which does not advance across laptop suspend, so a suspended
// machine could miss its nightly backup indefinitely.

use crate::kubectl::Crd;
use crate::lease;
use crate::routers::apps::{ANN_APP_ID, ANN_CHART_VERSION};
use crate::routers::backup_common::*;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
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

/// Creates a new BackupRun in Pending phase and returns immediately. There is no
/// spawned task — the next reconcile tick (within RECONCILE_TICK_SECS) is what actually
/// starts driving it, and every tick after that. Nothing about this run's progress is
/// tied to the lifetime of the process that created it.
pub async fn start(triggered_by: &str) -> anyhow::Result<String> {
    let name = format!("backup-{}", Utc::now().format("%Y%m%d%H%M%S"));
    BACKUP_RUN.create(&name, json!({ "triggeredBy": triggered_by }), &[("app.kubernetes.io/managed-by", "yolab")]).await?;
    BACKUP_RUN.patch_status(&name, json!({
        "phase": PHASE_PENDING,
        "phaseDeadline": deadline_after(30),
        "startedAt": Utc::now().to_rfc3339(),
    })).await?;
    Ok(name)
}

/// Advances `name` by exactly one bounded step from whatever phase it is CURRENTLY in
/// (read fresh from the CRD, not from any caller-held state). Called once per
/// non-terminal run on every reconcile tick — see the module doc for why this
/// replaces the old spawned-task design.
async fn step(name: &str) {
    let Some(run) = BACKUP_RUN.get(name).await else { return };
    let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_PENDING).to_string();

    let Some(cfg) = read_master_config().await else {
        let _ = BACKUP_RUN.patch_status(name, json!({
            "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
            "error": "backup not configured",
        })).await;
        return;
    };

    match phase.as_str() {
        PHASE_PENDING => step_pending(name, &cfg).await,
        PHASE_SYNCING => step_syncing(name, &run).await,
        PHASE_SNAPSHOTTING => step_snapshotting(name, &run, &cfg).await,
        PHASE_PRUNING => step_pruning(name, &run, &cfg).await,
        _ => {} // terminal — reconcile_tick already filters these out before calling step()
    }
}

/// Pending → SyncingVolumes. Lists every managed PVC, ensures each has a restic secret
/// and ReplicationSource, and stamps a fresh manual trigger on each. Safe to redo in
/// full: `ensure_restic_secret`/`ensure_replication_source` are idempotent "ensure"
/// operations, and re-stamping a manual trigger on a retry after a crash is harmless —
/// worst case, one extra redundant VolSync sync, never lost or duplicated data.
async fn step_pending(name: &str, cfg: &BackupConfig) {
    let pvcs = list_user_pvcs().await.unwrap_or_default();
    let since = Utc::now();
    let sync_deadline = since + chrono::Duration::seconds(1800);
    let pvc_status: Vec<Value> = pvcs.iter()
        .map(|p| json!({ "namespace": p.namespace, "name": p.name, "phase": "Syncing" }))
        .collect();

    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, cfg).await;
        let _ = ensure_replication_source(pvc, true).await;
    }

    let _ = BACKUP_RUN.patch_status(name, json!({
        "phase": PHASE_SYNCING,
        "phaseDeadline": sync_deadline.to_rfc3339(),
        "syncSince": since.to_rfc3339(),
        "pvcs": pvc_status,
    })).await;
}

/// SyncingVolumes: a single observation of every tracked PVC's ReplicationSource — no
/// internal sleep loop. Updates status.pvcs with the latest observed phase on every
/// tick (so the frontend's live per-volume progress keeps moving even across a crash-
/// and-restart of this process); transitions to SnapshottingCluster once every PVC has
/// synced, or once the phase deadline passes (proceeding with whatever synced — a
/// partial backup beats no backup).
async fn step_syncing(name: &str, run: &Value) {
    let since = run["status"]["syncSince"].as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let deadline = parse_deadline(run).unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(1800));
    let pvcs: Vec<PvcInfo> = run["status"]["pvcs"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .filter_map(|p| Some(PvcInfo {
            namespace: p["namespace"].as_str()?.to_string(),
            name: p["name"].as_str()?.to_string(),
        }))
        .collect();

    let rs = get_replication_sources().await;
    let mut pvc_status = Vec::with_capacity(pvcs.len());
    let mut stale = Vec::new();
    let mut all_done = true;
    let past_deadline = Utc::now() > deadline;

    for pvc in &pvcs {
        let cid = canonical_pvc_id(&pvc.name);
        let rs_name = format!("volsync-{cid}");
        let item = rs["items"].as_array().and_then(|items| {
            items.iter().find(|i| {
                i["metadata"]["name"].as_str() == Some(rs_name.as_str())
                    && i["metadata"]["namespace"].as_str() == Some(pvc.namespace.as_str())
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
        } else if past_deadline {
            stale.push(format!("{}/{}", pvc.namespace, pvc.name));
            "TimedOut"
        } else {
            all_done = false;
            "Syncing"
        };
        pvc_status.push(json!({ "namespace": pvc.namespace, "name": pvc.name, "phase": phase }));
    }

    if all_done || past_deadline {
        const SNAPSHOTTING_BUDGET_SECS: i64 = 900;
        let _ = BACKUP_RUN.patch_status(name, json!({
            "phase": PHASE_SNAPSHOTTING,
            "phaseDeadline": deadline_after(SNAPSHOTTING_BUDGET_SECS),
            "pvcs": pvc_status,
            "stalePvcs": stale,
        })).await;
    } else {
        let _ = BACKUP_RUN.patch_status(name, json!({ "pvcs": pvc_status })).await;
    }
}

/// SnapshottingCluster: runs the etcd/k8s/restic export as one bounded call, budgeted
/// to whatever time remains before this phase's deadline (not a fresh budget every
/// retry — the deadline is authoritative, and reconcile_tick's timeout sweep would
/// already have failed this run before step() ever runs again if that deadline had
/// passed). If the process dies mid-call, the next tick re-enters this same phase and
/// redoes the whole export from scratch — safe because every sub-step inside
/// `snapshot_cluster` (etcd snapshot, k8s export, restic backup) is independently safe
/// to re-run.
async fn step_snapshotting(name: &str, run: &Value, cfg: &BackupConfig) {
    let budget = parse_deadline(run)
        .map(|dl| (dl - Utc::now()).num_seconds().max(30))
        .unwrap_or(900) as u64;
    let stale = run["status"]["stalePvcs"].clone();

    let snapshot_result = tokio::time::timeout(
        Duration::from_secs(budget),
        snapshot_cluster(cfg),
    ).await;

    match snapshot_result {
        Ok(Ok(outcome)) => {
            let _ = BACKUP_RUN.patch_status(name, json!({
                "phase": PHASE_PRUNING,
                "phaseDeadline": deadline_after(300),
                "snapshotId": outcome.date,
                // Whether the etcd database made it into this snapshot. Reported to the
                // user as the "cluster state" backup age; a run can otherwise succeed
                // with volumes backed up but etcd silently missing.
                "etcdIncluded": outcome.etcd_included,
            })).await;
        }
        Ok(Err(e)) => {
            let _ = BACKUP_RUN.patch_status(name, json!({
                "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
                "error": e.to_string(), "stalePvcs": stale,
            })).await;
        }
        Err(_) => {
            let _ = BACKUP_RUN.patch_status(name, json!({
                "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
                "error": format!("snapshot step exceeded its {budget}s budget"), "stalePvcs": stale,
            })).await;
        }
    }
}

/// Pruning: restic forget/prune, then terminal. Safe to redo — re-running `restic
/// forget` when there is nothing new to prune is a no-op.
async fn step_pruning(name: &str, run: &Value, cfg: &BackupConfig) {
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    // --group-by tags (not the default host,paths): the staging dir is fixed across
    // runs now, but this still matters — every snapshot shares the "cluster-backup"
    // tag, which is what actually buckets them together for retention to apply across.
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

    let stale: Vec<String> = run["status"]["stalePvcs"].as_array().cloned().unwrap_or_default()
        .into_iter().filter_map(|v| v.as_str().map(String::from)).collect();
    let phase = if stale.is_empty() { PHASE_SUCCEEDED } else { PHASE_PARTIAL };
    let snapshot_id = run["status"]["snapshotId"].as_str().unwrap_or("").to_string();
    let _ = BACKUP_RUN.patch_status(name, json!({
        "phase": phase,
        "finishedAt": Utc::now().to_rfc3339(),
        "stalePvcs": stale,
    })).await;
    tracing::info!("backup-run {name}: {phase} (snapshot {snapshot_id})");
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
pub(crate) struct SnapshotOutcome {
    /// Date/id of the restic snapshot this run produced.
    pub date: String,
    /// Whether the etcd database actually made it in. The etcd step is best-effort (a
    /// backup of app data is still worth having without it), so this has to be reported
    /// rather than inferred — a run can otherwise show "Succeeded" while the cluster-state
    /// half was silently missing.
    pub etcd_included: bool,
}

async fn snapshot_cluster(cfg: &BackupConfig) -> anyhow::Result<SnapshotOutcome> {
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

async fn snapshot_cluster_inner(cfg: &BackupConfig, tmp_dir: &str) -> anyhow::Result<SnapshotOutcome> {
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

    let mut etcd_included = false;
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
                            etcd_included = true;
                            let _ = std::fs::remove_file(entry.path());
                        }
                        let _ = Command::new("kubectl")
                            .args(["delete", "etcdsnapshotfile", fname_str.as_ref(), "--ignore-not-found"])
                            .output().await;
                        break;
                    }
                }
            }
            if !etcd_included {
                tracing::warn!("cluster-backup: etcd snapshot {snap_name} saved but not found in {snap_dir}");
            }
        }
        Ok(o) => tracing::warn!("cluster-backup: etcd-snapshot: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => tracing::warn!("cluster-backup: k3s unavailable: {e}"),
    }

    // 2. Export K8s objects for all yolab-managed namespaces.
    let namespaces = list_managed_namespaces().await;
    let mut services: Vec<Value> = Vec::new();

    for ns in &namespaces {
        let mut items: Vec<Value> = Vec::new();

        // The Namespace object itself, FIRST in the list so `kubectl apply` creates it
        // before anything scoped to it.
        //
        // Its annotations — yolab.io/app-id, yolab.io/config, yolab.io/outputs — are the
        // app's entire identity. Exporting only the workload objects (which is what this
        // used to do) restores the data but brings the app back anonymous: no name or icon
        // in the UI, "App not found in catalog" on update, no uninstall hook, and every
        // setting the user chose — including app passwords — gone. That is a restore that
        // looks like it half-worked, which is worse than one that visibly failed.
        let ns_obj: Option<Value> = Command::new("kubectl")
            .args(["get", "namespace", ns, "-o", "json"])
            .output().await.ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
        if let Some(v) = &ns_obj {
            items.push(v.clone());
        }

        let obj_out = Command::new("kubectl")
            .args(["get", "deploy,svc,secret,configmap",
                   "-n", ns, "-o", "json", "--ignore-not-found"])
            .output().await;
        let workloads: Vec<Value> = obj_out.ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default();
        items.extend(workloads.iter().cloned());

        let sanitized = sanitize_k8s_items_for_backup(&items);
        if !sanitized.is_empty() {
            let list = json!({ "apiVersion": "v1", "kind": "List", "items": sanitized });
            if let Ok(s) = serde_json::to_string_pretty(&list) {
                let _ = tokio::fs::write(format!("{tmp_dir}/{ns}.yaml"), s.as_bytes()).await;
            }
        }

        // 3. Per-namespace catalog entry. Everything here is served to the browser by
        //    GET /api/backups/snapshots/:id/catalog to render the restore picker, so it
        //    carries only display-safe fields. yolab.io/config and yolab.io/outputs are
        //    deliberately NOT copied in — config holds app passwords, and it already
        //    travels (encrypted) inside {ns}.yaml above, which is the copy the restore
        //    actually applies.
        let ann = ns_obj
            .as_ref()
            .and_then(|v| v["metadata"]["annotations"].as_object().cloned())
            .unwrap_or_default();
        let app_id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Which chart version produced this app. Together with the image digests below,
        // this is the full answer to "what was running when this data was written".
        let chart_version = ann.get(ANN_CHART_VERSION).and_then(|v| v.as_str()).unwrap_or("").to_string();

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

        // Which images this namespace was actually running. With the catalog now
        // digest-pinned, this is an exact record of the version the data belongs to —
        // so a restore can report what it brought back instead of leaving the user to
        // find out from a crash loop.
        let images = collect_images(&workloads);

        services.push(json!({
            "namespace": ns,
            "app_id": app_id,
            "chart_version": chart_version,
            "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
            "pvcs": pvcs,
            "images": images,
        }));
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
        "catalog_version": built_hash(),
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

    tracing::info!("cluster-backup: snapshot complete ({date}, etcd_included={etcd_included})");
    Ok(SnapshotOutcome { date, etcd_included })
}

/// Every container image referenced by a namespace's workloads, deduplicated and sorted.
/// Init containers count: they run migrations and seed databases, so they are part of
/// "which version wrote this data" just as much as the main containers are.
fn collect_images(workloads: &[Value]) -> Vec<String> {
    let mut images: Vec<String> = Vec::new();
    for w in workloads {
        let spec = &w["spec"]["template"]["spec"];
        for key in ["initContainers", "containers"] {
            for c in spec[key].as_array().unwrap_or(&Vec::new()) {
                if let Some(img) = c["image"].as_str() {
                    if !images.iter().any(|e| e == img) {
                        images.push(img.to_string());
                    }
                }
            }
        }
    }
    images.sort();
    images
}

/// The repo commit this node was built from, written by the `yolabVersion` activation
/// script. Recorded in every backup so a restore can say which build produced the data.
fn built_hash() -> Option<String> {
    std::fs::read_to_string("/var/lib/yolab/built-hash")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// When the cluster state (etcd) was last captured, from the most recent run that
/// actually included it.
///
/// This used to be read from the `etcdsnapshotfile` CRD, filtering on an `etcd-daily-`
/// name prefix — but these snapshots are named `yolab-cluster-{date}` and the CRD object
/// is deleted immediately after the file is folded into the restic snapshot, so that
/// query was structurally incapable of ever returning anything. It reported "never" for
/// the entire life of the feature, which is exactly the wrong direction for a field whose
/// whole job is to warn you that cluster-state backups have stopped.
pub async fn last_etcd_snapshot() -> Option<String> {
    BACKUP_RUN
        .list()
        .await // newest-created first
        .iter()
        .find(|r| {
            r["status"]["etcdIncluded"].as_bool().unwrap_or(false)
                && matches!(
                    r["status"]["phase"].as_str(),
                    Some(PHASE_SUCCEEDED) | Some(PHASE_PARTIAL)
                )
        })
        .and_then(|r| r["status"]["finishedAt"].as_str().map(String::from))
}

/// Reconcile tick: only the lease holder acts, so at most one process (today: always
/// true since there's one node; matters once there are 2-3) drives BackupRuns forward
/// on any given tick.
pub async fn reconcile_tick(holder: &str) {
    let Some(_guard) = lease::acquire(LEASE_NAME, holder, LEASE_DURATION_SECS).await else {
        return;
    };

    // Time out any run stuck past its phase deadline — a crashed process or a hung
    // kubectl/restic call must not block every future backup forever.
    let mut timed_out: HashSet<String> = HashSet::new();
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
                timed_out.insert(name);
            }
        }
    }

    prune_old_runs().await;

    // Advance every still-active run by exactly one step. This is the whole reconciler
    // now — no separate executor task exists anywhere for this to race against.
    for run in BACKUP_RUN.list().await {
        let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || timed_out.contains(&name) {
            continue;
        }
        if is_terminal(run["status"]["phase"].as_str().unwrap_or(PHASE_PENDING)) {
            continue;
        }
        step(&name).await;
    }

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
            Ok(name) => tracing::info!("backup-run {name}: created (schedule)"),
            Err(e) => tracing::warn!("backup-run: failed to create scheduled run: {e}"),
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
