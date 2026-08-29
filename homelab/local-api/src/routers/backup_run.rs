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
use crate::routers::apps::{ANN_APP_ID, ANN_CHART_REPO, ANN_CHART_VERSION};
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

/// How long a volume sync may show NO forward progress before it is called stalled.
///
/// A watchdog, not a budget. The previous design gave the whole SyncingVolumes phase a
/// fixed 30 minutes from the moment it started, which asks the wrong question: it fails
/// a sync for being BIG. Live, one 14.7 GiB / 88k-file volume took ~21 minutes of that
/// 30 while seven others finished in 20-33 seconds each, so the run cleared its deadline
/// by about nine minutes — and would quietly start failing every night as that volume
/// grew, with the first symptom being backups that simply stopped completing.
///
/// Restic reports its own progress every few seconds, so "slow" and "stuck" are directly
/// distinguishable and there is no reason to conflate them. A sync that is still moving
/// pushes this deadline forward and can run for hours; a sync that has genuinely frozen
/// is caught in twenty minutes rather than never.
const SYNC_STALL_SECS: i64 = 1200;

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

/// True when `run` is non-terminal and has blown its phase deadline, i.e. the
/// process that was driving it is gone and nothing will finish it.
///
/// A run with no parseable deadline is NOT timed out: an unset deadline means
/// "we don't know", and failing a healthy in-flight backup on a missing field
/// would abandon work that is still progressing.
///
/// SyncingVolumes is exempt because `step_syncing` resolves its own stalls, and it
/// resolves them BETTER: a stalled volume there is dropped from the run and the other
/// volumes still produce a snapshot (Partial), whereas this sweep can only mark the
/// whole run Failed. Both used to be reachable and this one always won, because the
/// sweep runs before `step()` on every tick â so the "partial backup beats no backup"
/// path that step_syncing documents was, in practice, dead code. Seven backed-up
/// volumes were reported as a failed backup because an eighth was slow.
///
/// This does not weaken the crash guarantee the phase/deadline design exists for:
/// step_syncing runs on every tick, from any process, is bounded, and holds no state
/// outside the CRD \x{2014} so a crashed run in this phase is still resolved by whoever ticks
/// next, just into a more useful outcome.
fn is_timed_out(run: &Value, now: DateTime<Utc>) -> bool {
    let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_PENDING);
    if phase == PHASE_SYNCING {
        return false;
    }
    !is_terminal(phase) && parse_deadline(run).is_some_and(|dl| now > dl)
}

/// The most recent restic progress line from the VolSync mover pod backing one
/// ReplicationSource, e.g.
///
///   [17:50] 90.97%  44049 files 13.334 GiB, total 88164 files 14.657 GiB, 0 errors ETA 1:03
///
/// This is the ONLY real-time signal VolSync exposes mid-sync: `latestMoverStatus` stays
/// `{}` until the mover exits and `lastSyncTime` only moves on success, so from the CRD
/// alone a 20-minute copy and a frozen one look identical. That is what forced the old
/// design to guess with a fixed budget.
///
/// Read only for volumes still syncing — normally one or two — so this is a couple of
/// kubectl calls per tick, not one per PVC.
async fn mover_progress(namespace: &str, rs_name: &str) -> Option<String> {
    let prefix = format!("volsync-src-{rs_name}-");
    let pods = Command::new("kubectl")
        .args([
            "-n",
            namespace,
            "get",
            "pods",
            "-o",
            r#"jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}"#,
        ])
        .output()
        .await
        .ok()?;
    let pod = String::from_utf8_lossy(&pods.stdout)
        .lines()
        .find(|l| l.starts_with(&prefix))?
        .to_string();

    let logs = Command::new("kubectl")
        .args(["-n", namespace, "logs", &pod, "--tail=20"])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&logs.stdout)
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('[') && l.contains('%'))
        .map(|l| l.trim().to_string())
}

/// Percent complete and remaining time from a restic progress line.
///
/// Best-effort on purpose: restic's progress format is output, not an API, so a line
/// this does not recognise yields None and the page shows no number rather than a
/// confident wrong one. The line is still usable as a stall fingerprint either way —
/// the watchdog only cares THAT it changed, never what it says.
fn parse_restic_progress(line: &str) -> (Option<f64>, Option<String>) {
    let percent = line
        .split_whitespace()
        .find(|t| t.ends_with('%'))
        .and_then(|t| t.trim_end_matches('%').parse::<f64>().ok())
        .filter(|p| (0.0..=100.0).contains(p));
    let eta = line
        .rsplit_once("ETA ")
        .map(|(_, e)| e.trim().to_string())
        .filter(|e| !e.is_empty() && e.len() <= 12);
    (percent, eta)
}

/// Names of the terminal runs to delete, given the list newest-created first.
/// Non-terminal runs are never candidates however old they look.
fn names_to_prune(runs: &[Value], keep: usize) -> Vec<String> {
    let mut seen_terminal = 0usize;
    let mut out = Vec::new();
    for run in runs {
        if !is_terminal(run["status"]["phase"].as_str().unwrap_or("")) {
            continue;
        }
        seen_terminal += 1;
        if seen_terminal > keep {
            if let Some(name) = run["metadata"]["name"].as_str() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Whether the configured schedule says a backup is overdue.
///
/// The schedule is asked "when should this last have run", never "is it time now" — see
/// backup_schedule's module doc for why that distinction is the whole design on a
/// machine that sleeps.
async fn scheduled_backup_is_due(last_ok_finished: Option<&str>) -> bool {
    let schedule = crate::routers::backup_schedule::load().await;
    let last_ok = last_ok_finished
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Local));
    crate::routers::backup_schedule::is_due(&schedule, last_ok, chrono::Local::now())
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
            "get",
            "pods",
            "-A",
            "-l",
            "app.kubernetes.io/created-by=volsync",
            "--field-selector=status.phase=Running",
            "-o",
            "name",
        ])
        .output()
        .await
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.contains("volsync-src-"))
        })
        .unwrap_or(false)
}

/// Creates a new BackupRun in Pending phase and returns immediately. There is no
/// spawned task — the next reconcile tick (within RECONCILE_TICK_SECS) is what actually
/// starts driving it, and every tick after that. Nothing about this run's progress is
/// tied to the lifetime of the process that created it.
pub async fn start(triggered_by: &str) -> anyhow::Result<String> {
    let name = format!("backup-{}", Utc::now().format("%Y%m%d%H%M%S"));
    BACKUP_RUN
        .create(
            &name,
            json!({ "triggeredBy": triggered_by }),
            &[("app.kubernetes.io/managed-by", "yolab")],
        )
        .await?;
    BACKUP_RUN
        .patch_status(
            &name,
            json!({
                "phase": PHASE_PENDING,
                "phaseDeadline": deadline_after(30),
                "startedAt": Utc::now().to_rfc3339(),
            }),
        )
        .await?;
    Ok(name)
}

/// Advances `name` by exactly one bounded step from whatever phase it is CURRENTLY in
/// (read fresh from the CRD, not from any caller-held state). Called once per
/// non-terminal run on every reconcile tick — see the module doc for why this
/// replaces the old spawned-task design.
async fn step(name: &str) {
    let Some(run) = BACKUP_RUN.get(name).await else {
        return;
    };
    let phase = run["status"]["phase"]
        .as_str()
        .unwrap_or(PHASE_PENDING)
        .to_string();

    let Some(cfg) = read_master_config().await else {
        let _ = BACKUP_RUN
            .patch_status(
                name,
                json!({
                    "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
                    "error": "backup not configured",
                }),
            )
            .await;
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
    // Watchdog, not budget: this is pushed forward on every tick that shows progress.
    let sync_deadline = since + chrono::Duration::seconds(SYNC_STALL_SECS);
    let pvc_status: Vec<Value> = pvcs
        .iter()
        .map(|p| json!({ "namespace": p.namespace, "name": p.name, "phase": "Syncing" }))
        .collect();

    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, cfg).await;
        let _ = ensure_replication_source(pvc, true).await;
    }

    let _ = BACKUP_RUN
        .patch_status(
            name,
            json!({
                "phase": PHASE_SYNCING,
                "phaseDeadline": sync_deadline.to_rfc3339(),
                "syncSince": since.to_rfc3339(),
                "syncProgressAt": since.to_rfc3339(),
                "pvcs": pvc_status,
            }),
        )
        .await;
}

/// SyncingVolumes: a single observation of every tracked PVC's ReplicationSource — no
/// internal sleep loop. Updates status.pvcs with the latest observed phase and, for a
/// volume still copying, restic's own percent/ETA, so the page can show WHICH volume is
/// holding the run up instead of one opaque phase name.
///
/// Advances to SnapshottingCluster once every PVC has synced, or once nothing has moved
/// for SYNC_STALL_SECS (proceeding with whatever synced — a partial backup beats no
/// backup). Note what is NOT a reason to give up any more: elapsed time. A volume that
/// is still copying keeps the run alive indefinitely by pushing the deadline forward.
async fn step_syncing(name: &str, run: &Value) {
    let since = run["status"]["syncSince"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let deadline = parse_deadline(run)
        .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(SYNC_STALL_SECS));
    let pvcs: Vec<PvcInfo> = run["status"]["pvcs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            Some(PvcInfo {
                namespace: p["namespace"].as_str()?.to_string(),
                name: p["name"].as_str()?.to_string(),
            })
        })
        .collect();

    let rs = get_replication_sources().await;
    let mut pvc_status = Vec::with_capacity(pvcs.len());
    let mut stale = Vec::new();
    let mut all_done = true;
    let stalled = Utc::now() > deadline;
    // What "still moving" means, as one string compared against the previous tick's.
    // Deliberately coarse: any volume finishing, or any byte of restic progress on any
    // volume, counts for the whole run. A ten-volume backup where nine are done and one
    // is copying is progressing, and the watchdog should say so.
    let mut fingerprint = String::new();

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

        let done = result == Some("Successful") && synced_after_trigger;
        let mut entry = json!({ "namespace": pvc.namespace, "name": pvc.name });

        let phase = if done {
            fingerprint.push_str(&format!("{}/{}=done;", pvc.namespace, pvc.name));
            "Synced"
        } else {
            // Only volumes actually still copying are worth a log read.
            let progress = mover_progress(&pvc.namespace, &rs_name).await;
            if let Some(line) = &progress {
                fingerprint.push_str(&format!("{}/{}={line};", pvc.namespace, pvc.name));
                let (percent, eta) = parse_restic_progress(line);
                if let Some(p) = percent {
                    entry["percent"] = json!(p);
                }
                if let Some(e) = eta {
                    entry["eta"] = json!(e);
                }
            }
            if stalled {
                stale.push(format!("{}/{}", pvc.namespace, pvc.name));
                "Stalled"
            } else {
                all_done = false;
                "Syncing"
            }
        };
        entry["phase"] = json!(phase);
        pvc_status.push(entry);
    }

    if all_done || stalled {
        const SNAPSHOTTING_BUDGET_SECS: i64 = 900;
        let _ = BACKUP_RUN
            .patch_status(
                name,
                json!({
                    "phase": PHASE_SNAPSHOTTING,
                    "phaseDeadline": deadline_after(SNAPSHOTTING_BUDGET_SECS),
                    "pvcs": pvc_status,
                    "stalePvcs": stale,
                }),
            )
            .await;
        return;
    }

    // Still copying. Push the deadline out only when something actually changed since
    // the last tick — that is the whole difference between a watchdog and a timer that
    // can never fire.
    let moved = run["status"]["syncFingerprint"].as_str() != Some(fingerprint.as_str());
    let mut patch = json!({ "pvcs": pvc_status, "syncFingerprint": fingerprint });
    if moved {
        let now = Utc::now();
        patch["syncProgressAt"] = json!(now.to_rfc3339());
        patch["phaseDeadline"] =
            json!((now + chrono::Duration::seconds(SYNC_STALL_SECS)).to_rfc3339());
    }
    let _ = BACKUP_RUN.patch_status(name, patch).await;
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

    let snapshot_result =
        tokio::time::timeout(Duration::from_secs(budget), snapshot_cluster(cfg)).await;

    match snapshot_result {
        Ok(Ok(outcome)) => {
            let _ = BACKUP_RUN
                .patch_status(
                    name,
                    json!({
                        "phase": PHASE_PRUNING,
                        "phaseDeadline": deadline_after(300),
                        "snapshotId": outcome.date,
                        // Whether the etcd database made it into this snapshot. Reported to the
                        // user as the "cluster state" backup age; a run can otherwise succeed
                        // with volumes backed up but etcd silently missing.
                        "etcdIncluded": outcome.etcd_included,
                    }),
                )
                .await;
        }
        Ok(Err(e)) => {
            let _ = BACKUP_RUN
                .patch_status(
                    name,
                    json!({
                        "phase": PHASE_FAILED, "finishedAt": Utc::now().to_rfc3339(),
                        "error": e.to_string(), "stalePvcs": stale,
                    }),
                )
                .await;
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
        .args([
            "forget",
            "--tag",
            "cluster-backup",
            "--group-by",
            "tags",
            "--keep-daily",
            "7",
            "--keep-weekly",
            "4",
            "--keep-monthly",
            "12",
            "--prune",
        ])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await;
    if let Ok(o) = &forget {
        if !o.status.success() {
            tracing::warn!(
                "backup-run {name}: forget/prune failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
    }

    let stale: Vec<String> = run["status"]["stalePvcs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let phase = if stale.is_empty() {
        PHASE_SUCCEEDED
    } else {
        PHASE_PARTIAL
    };
    let snapshot_id = run["status"]["snapshotId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let _ = BACKUP_RUN
        .patch_status(
            name,
            json!({
                "phase": phase,
                "finishedAt": Utc::now().to_rfc3339(),
                "stalePvcs": stale,
            }),
        )
        .await;
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

async fn snapshot_cluster_inner(
    cfg: &BackupConfig,
    tmp_dir: &str,
) -> anyhow::Result<SnapshotOutcome> {
    let date = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let repo = cfg.restic_repo("cluster-backup");

    // 1. etcd snapshot — archived as etcd.db in this restic snapshot.
    //    NOTE: etcd.db is NOT consumed by RestoreRun. It is used exclusively by the
    //    external dr-restore.sh script, which runs before K3s/local-api are started
    //    and restores the etcd database directly.
    let snap_name = format!("yolab-cluster-{date}");
    let snap_saved = Command::new("k3s")
        .args(["etcd-snapshot", "save", &format!("--name={snap_name}")])
        .output()
        .await;

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
                            .args([
                                "delete",
                                "etcdsnapshotfile",
                                fname_str.as_ref(),
                                "--ignore-not-found",
                            ])
                            .output()
                            .await;
                        break;
                    }
                }
            }
            if !etcd_included {
                tracing::warn!(
                    "cluster-backup: etcd snapshot {snap_name} saved but not found in {snap_dir}"
                );
            }
        }
        Ok(o) => tracing::warn!(
            "cluster-backup: etcd-snapshot: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
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
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
        if let Some(v) = &ns_obj {
            items.push(v.clone());
        }

        let obj_out = Command::new("kubectl")
            .args([
                "get",
                "deploy,svc,secret,configmap",
                "-n",
                ns,
                "-o",
                "json",
                "--ignore-not-found",
            ])
            .output()
            .await;
        let workloads: Vec<Value> = obj_out
            .ok()
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
        let app_id = ann
            .get(ANN_APP_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Which chart version produced this app. Together with the image digests below,
        // this is the full answer to "what was running when this data was written".
        let chart_repo = ann
            .get(ANN_CHART_REPO)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let chart_version = ann
            .get(ANN_CHART_VERSION)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let pvc_out = Command::new("kubectl")
            .args(["get", "pvc", "-n", ns, "-o", "json"])
            .output()
            .await;
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
                    .as_str()
                    .unwrap_or("?")
                    .to_string();
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
            "chart_repo": chart_repo,
            "chart_version": chart_version,
            "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
            "pvcs": pvcs,
            "images": images,
        }));
    }

    let total_pvc_bytes: u64 = services
        .iter()
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
    let check = Command::new("restic")
        .args(["snapshots"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await;
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        let init = Command::new("restic")
            .args(["init"])
            .env("RESTIC_REPOSITORY", &repo)
            .env("RESTIC_PASSWORD", &cfg.restic_password)
            .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
            .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
            .output()
            .await?;
        if !init.status.success() {
            anyhow::bail!(
                "restic init failed: {}",
                String::from_utf8_lossy(&init.stderr).trim()
            );
        }
    }

    // 5. Backup. kill_on_drop: this is the one step that can genuinely run long (a full
    // B2 upload); if the caller's tokio::time::timeout fires, dropping this future must
    // actually kill the restic process rather than orphan it still holding the repo lock.
    let backup = Command::new("restic")
        .kill_on_drop(true)
        .args(["backup", tmp_dir, "--tag", "cluster-backup"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await?;

    if !backup.status.success() {
        anyhow::bail!(
            "restic backup failed: {}",
            String::from_utf8_lossy(&backup.stderr).trim()
        );
    }

    tracing::info!("cluster-backup: snapshot complete ({date}, etcd_included={etcd_included})");
    Ok(SnapshotOutcome {
        date,
        etcd_included,
    })
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
    let active = runs
        .iter()
        .find(|r| {
            let phase = r["status"]["phase"].as_str().unwrap_or(PHASE_PENDING);
            !is_terminal(phase)
        })
        .map(flatten_status);
    let last_finished = runs
        .iter()
        .find(|r| is_terminal(r["status"]["phase"].as_str().unwrap_or("")))
        .map(flatten_status);
    // Age of the last backup that actually produced a snapshot, Partial included — a
    // run that captured seven of eight volumes IS a backup of those seven.
    //
    // This is the number that tells someone their backups are broken, whatever the
    // cause, and it is the one number the page never had. Duration answers "is this run
    // slow", which nobody needs to know; staleness answers "when could I last have
    // restored", which is the entire point of the feature. It was already computed for
    // the scheduler on every tick and then thrown away.
    let last_ok_age_hours = runs
        .iter()
        .find(|r| {
            matches!(
                r["status"]["phase"].as_str(),
                Some(PHASE_SUCCEEDED) | Some(PHASE_PARTIAL)
            )
        })
        .and_then(|r| r["status"]["finishedAt"].as_str())
        .and_then(hours_since);
    json!({
        "active": active,
        "last": last_finished,
        "last_ok_age_hours": last_ok_age_hours,
        // So the page can state the rule instead of hardcoding a number beside it.
        "stale_after_hours": CATCHUP_AFTER_HOURS,
    })
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
        if !is_timed_out(&run, Utc::now()) {
            continue;
        }
        let phase = run["status"]["phase"]
            .as_str()
            .unwrap_or(PHASE_PENDING)
            .to_string();
        let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
        tracing::warn!("backup-run {name}: timed out in phase {phase}");
        let _ = BACKUP_RUN
            .patch_status(
                &name,
                json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": format!("timed out in phase {phase}"),
                }),
            )
            .await;
        timed_out.insert(name);
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
    let last_ok_finished = runs
        .iter()
        .find(|r| {
            matches!(
                r["status"]["phase"].as_str(),
                Some(PHASE_SUCCEEDED) | Some(PHASE_PARTIAL)
            )
        })
        .and_then(|r| r["status"]["finishedAt"].as_str());

    if scheduled_backup_is_due(last_ok_finished).await {
        match start("schedule").await {
            Ok(name) => tracing::info!("backup-run {name}: created (schedule)"),
            Err(e) => tracing::warn!("backup-run: failed to create scheduled run: {e}"),
        }
    }
}

/// Deletes terminal BackupRuns beyond the most recent `KEEP_TERMINAL_RUNS` — otherwise
/// every backup ever run (daily, forever) leaves a small object behind permanently.
async fn prune_old_runs() {
    let runs = BACKUP_RUN.list().await; // newest-created first
    for name in names_to_prune(&runs, KEEP_TERMINAL_RUNS) {
        BACKUP_RUN.delete(&name).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_named(name: &str, phase: &str) -> Value {
        json!({"metadata": {"name": name}, "status": {"phase": phase}})
    }

    fn run_with_deadline(phase: &str, deadline: DateTime<Utc>) -> Value {
        json!({
            "metadata": {"name": "backup-1"},
            "status": {"phase": phase, "phaseDeadline": deadline.to_rfc3339()},
        })
    }

    // ── is_terminal ───────────────────────────────────────────────────────────

    /// `is_terminal` is the single-flight gate: `is_active()` is its negation, so
    /// a phase wrongly classed as terminal lets a second backup start on top of a
    /// running one, and one wrongly classed as in-flight blocks backups forever.
    #[test]
    fn terminal_phases_are_exactly_the_three_end_states() {
        assert!(is_terminal(PHASE_SUCCEEDED));
        assert!(is_terminal(PHASE_PARTIAL));
        assert!(is_terminal(PHASE_FAILED));

        assert!(!is_terminal(PHASE_PENDING));
        assert!(!is_terminal(PHASE_SYNCING));
        assert!(!is_terminal(PHASE_SNAPSHOTTING));
        assert!(!is_terminal(PHASE_PRUNING));
    }

    /// An unrecognised phase — a future version's, or a corrupted status — must
    /// read as in-flight. Treating it as finished would let a concurrent backup
    /// start against the same restic repo.
    #[test]
    fn an_unknown_phase_is_not_terminal() {
        assert!(!is_terminal(""));
        assert!(!is_terminal("Succeeded "));
        assert!(!is_terminal("succeeded"));
        assert!(!is_terminal("Uploading"));
    }

    // ── deadline_after / parse_deadline ───────────────────────────────────────

    #[test]
    fn a_written_deadline_reads_back_as_the_same_instant() {
        let run = json!({"status": {"phaseDeadline": deadline_after(600)}});
        let parsed = parse_deadline(&run).expect("deadline_after must emit RFC3339");
        let delta = (parsed - Utc::now()).num_seconds();
        assert!((595..=600).contains(&delta), "got {delta}s");
    }

    #[test]
    fn parse_deadline_returns_none_for_anything_unusable() {
        assert!(parse_deadline(&json!({})).is_none());
        assert!(parse_deadline(&json!({"status": {}})).is_none());
        assert!(parse_deadline(&json!({"status": {"phaseDeadline": null}})).is_none());
        assert!(parse_deadline(&json!({"status": {"phaseDeadline": "tomorrow"}})).is_none());
        // A unix timestamp is not RFC3339 — must not silently parse as epoch.
        assert!(parse_deadline(&json!({"status": {"phaseDeadline": 1735689600}})).is_none());
    }

    #[test]
    fn parse_deadline_normalizes_other_offsets_to_utc() {
        let run = json!({"status": {"phaseDeadline": "2026-01-01T12:00:00+02:00"}});
        let parsed = parse_deadline(&run).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-01-01T10:00:00+00:00");
    }

    // ── is_timed_out ──────────────────────────────────────────────────────────

    // These use SnapshottingCluster as the representative non-terminal phase.
    // SyncingVolumes no longer belongs here: it owns its own stall handling, so the
    // generic sweep deliberately skips it — see the exemption tests above.
    #[test]
    fn a_run_past_its_deadline_is_timed_out() {
        let now = Utc::now();
        let run = run_with_deadline(PHASE_SNAPSHOTTING, now - chrono::Duration::seconds(1));
        assert!(is_timed_out(&run, now));
    }

    #[test]
    fn a_run_inside_its_deadline_is_left_alone() {
        let now = Utc::now();
        let run = run_with_deadline(PHASE_SNAPSHOTTING, now + chrono::Duration::hours(1));
        assert!(!is_timed_out(&run, now));
    }

    /// A finished run keeps its last deadline, which is always in the past. If
    /// terminality were not checked first, every completed backup would be
    /// rewritten to Failed on the next tick.
    #[test]
    fn a_terminal_run_is_never_timed_out_however_old_its_deadline() {
        let now = Utc::now();
        for phase in [PHASE_SUCCEEDED, PHASE_PARTIAL, PHASE_FAILED] {
            let run = run_with_deadline(phase, now - chrono::Duration::days(400));
            assert!(!is_timed_out(&run, now), "{phase}");
        }
    }

    /// "No deadline recorded" means unknown, not expired — a run that is genuinely
    /// progressing must not be failed because a field is missing.
    #[test]
    fn a_run_with_no_deadline_is_not_timed_out() {
        let now = Utc::now();
        assert!(!is_timed_out(
            &json!({"status": {"phase": PHASE_SYNCING}}),
            now
        ));
        assert!(!is_timed_out(&json!({}), now));
        assert!(!is_timed_out(
            &json!({"status": {"phase": PHASE_SYNCING, "phaseDeadline": "garbage"}}),
            now
        ));
    }

    /// A run with no phase at all defaults to Pending — in-flight, and therefore
    /// still subject to its deadline rather than ignored forever.
    #[test]
    fn a_run_with_no_phase_is_treated_as_pending() {
        let now = Utc::now();
        let run = json!({
            "status": {"phaseDeadline": (now - chrono::Duration::seconds(1)).to_rfc3339()}
        });
        assert!(is_timed_out(&run, now));
    }

    #[test]
    fn timeout_is_exclusive_at_the_deadline_itself() {
        let now = Utc::now();
        assert!(!is_timed_out(&run_with_deadline(PHASE_PENDING, now), now));
    }

    // ── names_to_prune ────────────────────────────────────────────────────────

    #[test]
    fn nothing_is_pruned_below_the_retention_count() {
        let runs: Vec<Value> = (0..5)
            .map(|i| run_named(&format!("backup-{i}"), PHASE_SUCCEEDED))
            .collect();
        assert!(names_to_prune(&runs, 30).is_empty());
    }

    #[test]
    fn the_oldest_terminal_runs_are_pruned_first() {
        // Newest-created first, as Crd::list returns them.
        let runs: Vec<Value> = (0..5)
            .map(|i| run_named(&format!("backup-{i}"), PHASE_SUCCEEDED))
            .collect();
        assert_eq!(
            names_to_prune(&runs, 2),
            vec!["backup-2", "backup-3", "backup-4"]
        );
    }

    /// Deleting a BackupRun that is still driving a restic operation would strand
    /// the run with no way to observe or finish it.
    #[test]
    fn an_in_flight_run_is_never_pruned() {
        let runs = vec![
            run_named("active", PHASE_SYNCING),
            run_named("old-1", PHASE_SUCCEEDED),
            run_named("old-2", PHASE_FAILED),
        ];
        assert_eq!(names_to_prune(&runs, 1), vec!["old-2"]);
    }

    /// In-flight runs must not consume retention slots either, or a stuck run
    /// would slowly evict the entire backup history.
    #[test]
    fn in_flight_runs_do_not_count_against_the_retention_budget() {
        let runs = vec![
            run_named("active-1", PHASE_PENDING),
            run_named("active-2", PHASE_PRUNING),
            run_named("done-1", PHASE_SUCCEEDED),
            run_named("done-2", PHASE_SUCCEEDED),
        ];
        assert!(names_to_prune(&runs, 2).is_empty());
    }

    #[test]
    fn pruning_skips_entries_with_no_name() {
        let runs = vec![
            run_named("keep", PHASE_SUCCEEDED),
            json!({"status": {"phase": PHASE_SUCCEEDED}}),
        ];
        assert!(names_to_prune(&runs, 1).is_empty());
    }

    #[test]
    fn pruning_an_empty_list_is_a_no_op() {
        assert!(names_to_prune(&[], 30).is_empty());
    }

    // ── collect_images ────────────────────────────────────────────────────────

    fn workload(init: &[&str], main: &[&str]) -> Value {
        json!({"spec": {"template": {"spec": {
            "initContainers": init.iter().map(|i| json!({"image": i})).collect::<Vec<_>>(),
            "containers": main.iter().map(|i| json!({"image": i})).collect::<Vec<_>>(),
        }}}})
    }

    /// Init containers run migrations and seed databases, so they are part of
    /// "which version wrote this data" — a restore that only knows the main image
    /// can reproduce the app but not the schema that shaped its data.
    #[test]
    fn collect_images_includes_init_containers() {
        let images = collect_images(&[workload(&["migrate:1.0"], &["app:2.0"])]);
        assert_eq!(images, vec!["app:2.0", "migrate:1.0"]);
    }

    #[test]
    fn collect_images_deduplicates_across_workloads() {
        let images = collect_images(&[
            workload(&[], &["app:2.0", "sidecar:1.0"]),
            workload(&["app:2.0"], &["app:2.0"]),
        ]);
        assert_eq!(images, vec!["app:2.0", "sidecar:1.0"]);
    }

    #[test]
    fn collect_images_is_sorted_so_the_record_is_stable() {
        // An unstable order would make every backup's metadata differ from the last.
        let a = collect_images(&[workload(&[], &["z:1", "a:1", "m:1"])]);
        let b = collect_images(&[workload(&[], &["m:1", "z:1", "a:1"])]);
        assert_eq!(a, b);
        assert_eq!(a, vec!["a:1", "m:1", "z:1"]);
    }

    #[test]
    fn collect_images_handles_workloads_with_no_containers() {
        assert!(collect_images(&[]).is_empty());
        assert!(collect_images(&[json!({})]).is_empty());
        assert!(collect_images(&[json!({"spec": {"template": {"spec": {}}}})]).is_empty());
        // A container entry with no image at all.
        assert!(collect_images(&[json!({"spec": {"template": {"spec": {
            "containers": [{"name": "app"}]
        }}}})])
        .is_empty());
    }

    #[test]
    fn collect_images_keeps_digest_pins_verbatim() {
        // The digest is the whole point of the record — it must not be normalized away.
        let pinned = "ghcr.io/org/app:1.2@sha256:abc123";
        assert_eq!(collect_images(&[workload(&[], &[pinned])]), vec![pinned]);
    }

    // ── flatten_status ────────────────────────────────────────────────────────

    #[test]
    fn flatten_status_lifts_the_name_alongside_the_status_fields() {
        let item = json!({
            "metadata": {"name": "backup-20260101000000"},
            "spec": {"triggeredBy": "schedule"},
            "status": {"phase": PHASE_SUCCEEDED, "finishedAt": "2026-01-01T00:10:00Z"},
        });
        let flat = flatten_status(&item);
        assert_eq!(flat["name"], json!("backup-20260101000000"));
        assert_eq!(flat["phase"], json!(PHASE_SUCCEEDED));
        assert_eq!(flat["finishedAt"], json!("2026-01-01T00:10:00Z"));
        // The envelope the frontend does not want is gone.
        assert!(flat.get("metadata").is_none());
        assert!(flat.get("spec").is_none());
    }

    /// A run created but not yet reconciled has no status at all. The UI still
    /// needs to see that it exists, so this must yield a named object, not null.
    #[test]
    fn flatten_status_yields_a_named_object_when_status_is_missing() {
        let flat = flatten_status(&json!({"metadata": {"name": "backup-new"}}));
        assert_eq!(flat, json!({"name": "backup-new"}));
    }

    #[test]
    fn flatten_status_replaces_a_non_object_status() {
        for status in [json!(null), json!("Succeeded"), json!([1, 2])] {
            let flat = flatten_status(&json!({"metadata": {"name": "x"}, "status": status}));
            assert_eq!(flat, json!({"name": "x"}));
        }
    }

    // ── The stall watchdog ────────────────────────────────────────────────────
    //
    // The rule these pin down: a backup never fails for being slow, only for
    // stopping. Duration is not an input anywhere below.

    /// The line restic actually prints, taken from the live mover pod that
    /// prompted this change.
    const LIVE_LINE: &str =
        "[17:50] 90.97%  44049 files 13.334 GiB, total 88164 files 14.657 GiB, 0 errors ETA 1:03";

    #[test]
    fn restic_progress_yields_percent_and_eta() {
        let (percent, eta) = parse_restic_progress(LIVE_LINE);
        assert_eq!(percent, Some(90.97));
        assert_eq!(eta.as_deref(), Some("1:03"));
    }

    /// A line with no ETA yet is still usable: the percent is what the page shows.
    #[test]
    fn restic_progress_survives_a_missing_eta() {
        let (percent, eta) = parse_restic_progress(
            "[00:10] 0.00%  12 files 1.0 MiB, total 88164 files 14.657 GiB, 0 errors",
        );
        assert_eq!(percent, Some(0.0));
        assert_eq!(eta, None);
    }

    /// restic's output is not an API. Anything unrecognised must yield nothing
    /// rather than a confident wrong number — the line still works as a stall
    /// fingerprint, which is all the watchdog needs from it.
    #[test]
    fn restic_progress_refuses_to_guess() {
        for line in [
            "",
            "unlocking repository",
            "[17:50] 900.00% nonsense",
            "[17:50] not-a-number%",
        ] {
            let (percent, _) = parse_restic_progress(line);
            assert!(percent.is_none(), "must not parse a percent from {line:?}");
        }
    }

    /// The reason the sweep is exempt for this phase. Before, a sync past its
    /// deadline was marked Failed by reconcile_tick before step_syncing could
    /// ever run its own past-deadline branch, so seven backed-up volumes were
    /// reported as a failed backup because an eighth was slow.
    #[test]
    fn a_syncing_run_is_never_failed_by_the_generic_sweep() {
        let long_past = (Utc::now() - chrono::Duration::hours(9)).to_rfc3339();
        let run = json!({"status": {"phase": PHASE_SYNCING, "phaseDeadline": long_past}});
        assert!(!is_timed_out(&run, Utc::now()));
    }

    /// Every other phase keeps the guarantee the CRD design exists for: past the
    /// deadline and non-terminal means the run is dead, resolvable by any tick.
    #[test]
    fn every_other_phase_still_times_out() {
        let long_past = (Utc::now() - chrono::Duration::hours(9)).to_rfc3339();
        for phase in [PHASE_PENDING, PHASE_SNAPSHOTTING, PHASE_PRUNING] {
            let run = json!({"status": {"phase": phase, "phaseDeadline": long_past}});
            assert!(
                is_timed_out(&run, Utc::now()),
                "{phase} must still time out"
            );
        }
    }

    #[test]
    fn a_terminal_run_is_never_timed_out() {
        let long_past = (Utc::now() - chrono::Duration::hours(9)).to_rfc3339();
        for phase in [PHASE_SUCCEEDED, PHASE_PARTIAL, PHASE_FAILED] {
            let run = json!({"status": {"phase": phase, "phaseDeadline": long_past}});
            assert!(!is_timed_out(&run, Utc::now()));
        }
    }

    #[test]
    fn flatten_status_survives_a_missing_name() {
        let flat = flatten_status(&json!({"status": {"phase": PHASE_FAILED}}));
        assert_eq!(flat["phase"], json!(PHASE_FAILED));
        assert!(flat.get("name").is_none());
    }
}
