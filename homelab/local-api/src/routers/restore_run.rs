// RestoreRun reconciler — replaces DrRestoreGuard + dr_start/dr_status/dr_apply +
// reconcile_restores + run_restore_reconciler + apply_one_restore.
//
// Phases: Validating → WaitingForStorage → RestoringVolumes → Applying →
// Succeeded|Partial|Failed. Same rationale as BackupRun (see backup_run.rs's module
// doc for the full argument) — every non-terminal phase has a deadline, and the
// Applying phase unconditionally scales every namespace's deployments back up —
// succeeded, failed, or skipped — so "restore didn't work" degrades to "app running
// with old/empty data", never to "app permanently stopped".
//
// ── Level-triggered, not edge-triggered ──────────────────────────────────────────────
//
// There is no spawned task. `start()` only creates the object. The reconcile tick calls
// `step()` once per non-terminal run, every tick: read the CRD's current phase and the
// real cluster state, do exactly one bounded unit of work, return. A crash between any
// two ticks is invisible to the next one — it just sees the same status and keeps going,
// because every phase is built from operations that are safe to redo (`ensure_*` helpers,
// idempotent `kubectl apply`, delete-if-exists checks). Anything a phase computes that a
// *later* phase needs (resolved namespaces, PVC catalog, restore point-in-time) is
// written into CRD status during that phase.
//
// ── Recovery is unskippable ──────────────────────────────────────────────────────────
//
// `step_applying` is the ONLY code that scales deployments back up, and terminal runs are
// never stepped again. So any path that jumps straight from a mutating phase to a terminal
// phase leaves every app in the restore at 0 replicas, permanently, with nothing able to
// fix it — which is exactly the live incident this whole reconciler was written to prevent,
// and it survived the previous rewrite because `reconcile_tick`'s timeout sweep set
// `Failed` (terminal) *before* the step loop ran, making `step_restoring`'s own
// past-deadline handling unreachable.
//
// Every give-up path now goes through `terminate()`, which routes to Applying whenever the
// run has already scaled something down, carrying `abortReason` so the eventual terminal
// phase is still honest about having failed. Only phases that provably haven't touched the
// cluster yet (Validating, WaitingForStorage) may fail flat.
//
// ── Bounded steps ────────────────────────────────────────────────────────────────────
//
// RestoringVolumes used to do the entire per-namespace setup — including a blocking
// `delete_pvc_and_wait` of up to 120s *per PVC*, sequentially — inside one `step()` call,
// against a 90s lease, while also blocking BackupRun's reconciliation (the tick loop is
// sequential). Now every volume carries its own sub-phase:
//
//   Pending → Deleting → Restoring → Succeeded|Failed     (or Pending → Skipped)
//
// and each tick advances each volume by one non-blocking observation, under an overall
// STEP_BUDGET_SECS wall-clock cap. Nothing inside a step ever waits on the cluster; it
// observes, records, and returns. Per-namespace one-time work (create namespace, apply
// YAML, record + zero replica counts) is guarded by a persisted `setupComplete` marker,
// and `scaledDeployments` is persisted *before* the scale-down so a crash in that window
// can never lose the replica counts needed to bring the apps back.

use crate::kubectl::Crd;
use crate::lease;
use crate::routers::backup_common::*;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::time::Instant;
use tokio::process::Command;

pub(crate) const RESTORE_RUN: Crd = Crd {
    group: "yolab.io",
    version: "v1alpha1",
    plural: "restoreruns",
    kind: "RestoreRun",
};

const LEASE_NAME: &str = "yolab-restore-reconciler";
const LEASE_DURATION_SECS: i64 = 90;
/// Restores are rare and their status objects are bigger (per-namespace/per-volume
/// detail) than BackupRun's — keep fewer around.
const KEEP_TERMINAL_RUNS: usize = 10;

/// Wall-clock cap on a single `step()` call. Must stay comfortably below
/// LEASE_DURATION_SECS: the lease is renewed once per tick, so a step that outlives it
/// lets another process take over mid-operation. Progress is persisted as it goes, so
/// running out of budget simply means the next tick picks up where this one stopped.
const STEP_BUDGET_SECS: u64 = 30;

/// How long a PVC may sit in `Deleting` before the volume is declared failed. Generous:
/// the pvc-protection finalizer only clears once every mounting pod is gone, and eviction
/// on a loaded node is not fast.
const PVC_DELETE_TIMEOUT_SECS: i64 = 180;

const PHASE_VALIDATING: &str = "Validating";
const PHASE_WAITING_STORAGE: &str = "WaitingForStorage";
const PHASE_RESTORING: &str = "RestoringVolumes";
const PHASE_APPLYING: &str = "Applying";
const PHASE_SUCCEEDED: &str = "Succeeded";
const PHASE_PARTIAL: &str = "Partial";
const PHASE_FAILED: &str = "Failed";

// Per-volume sub-phases within RestoringVolumes.
const VOL_PENDING: &str = "Pending";
const VOL_DELETING: &str = "Deleting";
const VOL_RESTORING: &str = "Restoring";
const VOL_SUCCEEDED: &str = "Succeeded";
const VOL_FAILED: &str = "Failed";
/// No backup snapshot exists for this PVC — the live PVC is left untouched rather than
/// being replaced with nothing.
const VOL_SKIPPED: &str = "Skipped";

fn is_terminal(phase: &str) -> bool {
    matches!(phase, PHASE_SUCCEEDED | PHASE_PARTIAL | PHASE_FAILED)
}

fn vol_is_terminal(phase: &str) -> bool {
    matches!(phase, VOL_SUCCEEDED | VOL_FAILED | VOL_SKIPPED)
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

// ── Pure decision functions ───────────────────────────────────────────────────
//
// Kept free of I/O so the transitions that actually matter — what a timeout does, when a
// volume advances, what the final phase is — are unit-testable. Every bug this module has
// shipped was in one of these decisions, not in the kubectl plumbing around them.

/// Where a run that can no longer make progress must go.
///
/// `Applying` is the only code that scales deployments back up, so any run that has
/// already scaled something down MUST pass through it even when it has failed — otherwise
/// the apps stay dark forever. A run that hasn't touched the cluster yet can fail flat.
///
/// Applying itself is the exception: it IS the recovery, so routing it back into itself
/// on a timeout would re-arm the deadline every tick and never terminate. By the time it
/// can time out the scale-up has already been attempted at least once, so it ends there.
fn abort_phase(current_phase: &str, ns_state: &[NamespaceState]) -> &'static str {
    if current_phase == PHASE_APPLYING {
        return PHASE_FAILED;
    }
    if ns_state.iter().any(|n| !n.scaled_deployments.is_empty()) {
        PHASE_APPLYING
    } else {
        PHASE_FAILED
    }
}

/// Maps a ReplicationDestination's latest mover result onto a terminal volume phase.
/// `None` means "still running, leave it alone".
fn rd_result_to_phase(result: Option<&str>) -> Option<&'static str> {
    match result.map(str::to_ascii_lowercase).as_deref() {
        Some("successful") => Some(VOL_SUCCEEDED),
        Some("failed") => Some(VOL_FAILED),
        _ => None,
    }
}

fn delete_timed_out(deleting_since: Option<&str>, now: DateTime<Utc>) -> bool {
    match deleting_since.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        Some(t) => (now - t.with_timezone(&Utc)).num_seconds() > PVC_DELETE_TIMEOUT_SECS,
        // No timestamp recorded (shouldn't happen) — don't strand the volume forever.
        None => true,
    }
}

/// Final phase for a run, from its per-volume outcomes. `aborted` is true when the run
/// reached Applying via `terminate()` rather than by completing normally — in that case
/// the best possible outcome is Partial, never Succeeded, even if every volume happened
/// to land Succeeded before the abort.
fn terminal_phase(volumes: &[&VolumeState], aborted: bool) -> &'static str {
    let total = volumes.len();
    if total == 0 {
        // No PVCs to restore — a YAML-only namespace applied fine, unless we got here
        // by aborting.
        return if aborted {
            PHASE_FAILED
        } else {
            PHASE_SUCCEEDED
        };
    }
    let succeeded = volumes.iter().filter(|v| v.phase == VOL_SUCCEEDED).count();
    let failed = volumes.iter().filter(|v| v.phase == VOL_FAILED).count();
    let skipped = volumes.iter().filter(|v| v.phase == VOL_SKIPPED).count();

    if failed == 0 && !aborted {
        return PHASE_SUCCEEDED; // includes the all-skipped case: nothing was lost
    }
    if succeeded == 0 && skipped == 0 {
        return PHASE_FAILED;
    }
    PHASE_PARTIAL
}

/// Newest snapshot id from `restic snapshots --json` output.
///
/// restic returns snapshots ascending by time, so `.first()` is the OLDEST — taking it
/// means "restore the latest backup" silently restores the oldest retained one. `--last`
/// / `--latest` are not used: `--last` is deprecated and removed in current restic, and
/// when the flag is rejected the command fails, which every caller then has to
/// distinguish from "no snapshots exist" (see `snapshots_exist`).
fn latest_snapshot_id(snapshots: &Value) -> Option<String> {
    snapshots
        .as_array()?
        .iter()
        .max_by_key(|s| s["time"].as_str().unwrap_or("").to_string())?["id"]
        .as_str()
        .map(String::from)
}

pub async fn is_active() -> bool {
    RESTORE_RUN.list().await.iter().any(|r| {
        let phase = r["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING);
        !is_terminal(phase)
    })
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

/// GET /api/backups/dr/status consumes this — the active run's full status (phase,
/// per-namespace/per-volume progress), or the most recently finished one.
pub async fn current_status() -> Value {
    let runs = RESTORE_RUN.list().await; // newest-created first
    let active = runs
        .iter()
        .find(|r| !is_terminal(r["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING)))
        .map(flatten_status);
    let last_finished = runs
        .iter()
        .find(|r| is_terminal(r["status"]["phase"].as_str().unwrap_or("")))
        .map(flatten_status);
    json!({ "active": active, "last": last_finished })
}

/// Creates a RestoreRun in Validating phase and returns immediately. There is no
/// spawned task — the next reconcile tick picks it up, and every tick after that.
pub async fn start(
    snapshot_id: Option<String>,
    all: bool,
    namespaces: Vec<String>,
    rebuild_storage: bool,
) -> anyhow::Result<String> {
    let name = format!("restore-{}", Utc::now().format("%Y%m%d%H%M%S"));
    RESTORE_RUN
        .create(
            &name,
            json!({
                "snapshotId": snapshot_id,
                "all": all,
                "namespaces": namespaces,
                "rebuildStorage": rebuild_storage,
            }),
            &[("app.kubernetes.io/managed-by", "yolab")],
        )
        .await?;
    RESTORE_RUN
        .patch_status(
            &name,
            json!({
                "phase": PHASE_VALIDATING,
                "phaseDeadline": deadline_after(120),
                "startedAt": Utc::now().to_rfc3339(),
            }),
        )
        .await?;
    Ok(name)
}

/// The single give-up path. Routes through Applying whenever the run has already scaled
/// deployments down, so the apps always get turned back on; records `abortReason` so the
/// terminal phase computed later is Partial/Failed rather than Succeeded.
///
/// Every failure path in this module goes through here. Nothing else may write a terminal
/// phase directly except `step_applying`, which is the end of the line by construction.
async fn terminate(name: &str, run: &Value, reason: impl std::fmt::Display) {
    let ns_state = parse_ns_state(run);
    let current = run["status"]["phase"].as_str().unwrap_or("");
    let target = abort_phase(current, &ns_state);
    if target == PHASE_APPLYING {
        tracing::warn!("restore-run {name}: {reason} — running recovery (scaling apps back up)");
        let _ = RESTORE_RUN
            .patch_status(
                name,
                json!({
                    "phase": PHASE_APPLYING,
                    "phaseDeadline": deadline_after(300),
                    "abortReason": reason.to_string(),
                }),
            )
            .await;
    } else {
        tracing::warn!("restore-run {name}: failed: {reason}");
        let _ = RESTORE_RUN
            .patch_status(
                name,
                json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": reason.to_string(),
                }),
            )
            .await;
    }
}

/// Advances `name` by exactly one bounded step from whatever phase it is CURRENTLY in.
async fn step(name: &str) {
    let Some(run) = RESTORE_RUN.get(name).await else {
        return;
    };
    let phase = run["status"]["phase"]
        .as_str()
        .unwrap_or(PHASE_VALIDATING)
        .to_string();

    let Some(cfg) = read_master_config().await else {
        terminate(name, &run, "backup not configured").await;
        return;
    };

    match phase.as_str() {
        PHASE_VALIDATING => step_validating(name, &run, &cfg).await,
        PHASE_REBUILDING => step_rebuilding_storage(name, &run).await,
        PHASE_WAITING_STORAGE => step_waiting_storage(name, &run).await,
        PHASE_RESTORING => step_restoring(name, &run, &cfg).await,
        PHASE_APPLYING => step_applying(name, &run).await,
        _ => {} // terminal — reconcile_tick filters these out before calling step()
    }
}

/// Validating: resolve the snapshot id, extract catalog.json, resolve the namespace
/// list, run the space pre-flight. Entirely read-only against the cluster, so safe to
/// redo in full on a retry.
async fn step_validating(name: &str, run: &Value, cfg: &BackupConfig) {
    let rebuild_storage = run["spec"]["rebuildStorage"].as_bool().unwrap_or(false);
    let requested_snapshot = run["spec"]["snapshotId"].as_str().map(String::from);
    let want_all = run["spec"]["all"].as_bool().unwrap_or(false);
    let requested_namespaces: Vec<String> = run["spec"]["namespaces"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let snapshot_id = match requested_snapshot {
        Some(id) => id,
        None => {
            let out = Command::new("restic")
                .args(["snapshots", "--json", "--tag", "cluster-backup"])
                .env("RESTIC_REPOSITORY", &repo)
                .env("RESTIC_PASSWORD", &cfg.restic_password)
                .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
                .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                .output()
                .await;
            // A failed query is NOT "no snapshots" — conflating the two is how a restore
            // silently turns into a no-op that reports success.
            let parsed = match out {
                Ok(o) if o.status.success() => serde_json::from_slice::<Value>(&o.stdout).ok(),
                Ok(o) => {
                    terminate(
                        name,
                        run,
                        format!(
                            "could not list snapshots: {}",
                            String::from_utf8_lossy(&o.stderr).trim()
                        ),
                    )
                    .await;
                    return;
                }
                Err(e) => {
                    terminate(name, run, format!("restic unavailable: {e}")).await;
                    return;
                }
            };
            match parsed.as_ref().and_then(latest_snapshot_id) {
                Some(id) => id,
                None => {
                    terminate(
                        name,
                        run,
                        "no snapshot specified and no cluster-backup snapshot exists",
                    )
                    .await;
                    return;
                }
            }
        }
    };

    let cat_target = format!("/tmp/yolab-dr-catalog-{}", random_hex(8));
    let restore_out = Command::new("restic")
        .args([
            "restore",
            &snapshot_id,
            "--target",
            &cat_target,
            "--include",
            "**/catalog.json",
        ])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await;
    let catalog: Value = match restore_out {
        Ok(o) if o.status.success() => {
            let find_out = Command::new("find")
                .args([&cat_target, "-name", "catalog.json", "-type", "f"])
                .output()
                .await;
            let cat_path = find_out
                .ok()
                .map(|f| String::from_utf8_lossy(&f.stdout).trim().to_string())
                .unwrap_or_default();
            let c = if !cat_path.is_empty() {
                tokio::fs::read(&cat_path)
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(json!({}))
            } else {
                json!({})
            };
            let _ = tokio::fs::remove_dir_all(&cat_target).await;
            c
        }
        Ok(o) => {
            let _ = tokio::fs::remove_dir_all(&cat_target).await;
            terminate(
                name,
                run,
                format!(
                    "could not extract catalog from snapshot: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            )
            .await;
            return;
        }
        Err(e) => {
            terminate(name, run, format!("restic unavailable: {e}")).await;
            return;
        }
    };

    let namespaces: Vec<String> = if want_all {
        catalog["namespaces"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        requested_namespaces
    };
    if namespaces.is_empty() {
        terminate(
            name,
            run,
            "no namespaces found — snapshot may predate this feature, or pass namespaces[] explicitly",
        )
        .await;
        return;
    }

    // Space pre-flight (ceph df talks to MON, available even mid-recovery).
    let total_pvc_bytes = catalog["total_pvc_bytes"].as_u64().unwrap_or(0);
    if total_pvc_bytes > 0 {
        match crate::kubectl::ceph_exec(&["df", "-f", "json"]).await {
            Ok(df_raw) => {
                if let Ok(df) = serde_json::from_str::<Value>(&df_raw) {
                    let avail = df["stats"]["total_avail_bytes"]
                        .as_u64()
                        .unwrap_or(u64::MAX);
                    let reclaimable = reclaimable_pvc_bytes(&namespaces).await;
                    let effective_avail = avail.saturating_add(reclaimable);
                    let need = total_pvc_bytes * 6 / 5;
                    if effective_avail < need {
                        terminate(
                            name,
                            run,
                            format!(
                            "insufficient storage: {avail} bytes free (+{reclaimable} reclaimable \
                             from PVCs being replaced), ~{need} bytes needed \
                             ({total_pvc_bytes} bytes of PVC data + 20% headroom). \
                             Add more disks or reduce replication before restoring."
                        ),
                        )
                        .await;
                        return;
                    }
                    tracing::info!("restore-run {name}: space pre-flight ok — {avail} free + {reclaimable} reclaimable, {need} needed");
                }
            }
            Err(e) => {
                tracing::warn!(
                    "restore-run {name}: space pre-flight skipped (ceph unavailable: {e})"
                )
            }
        }
    }

    let restore_as_of: Option<String> = Command::new("restic")
        .args(["snapshots", &snapshot_id, "--json"])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v.as_array()?.first()?["time"].as_str().map(String::from));

    let namespaces_status: Vec<Value> = namespaces
        .iter()
        .map(|ns| {
            NamespaceState {
                namespace: ns.clone(),
                setup_complete: false,
                scaled_deployments: Vec::new(),
                volumes: Vec::new(),
            }
            .to_json()
        })
        .collect();

    // resolvedNamespaces + catalog are written here so later phases (which only ever
    // read the CRD, never a local variable) can pick up exactly what Validating found.
    let _ = RESTORE_RUN
        .patch_status(
            name,
            json!({
                // Validating has proved the backup is readable and named what it
                // contains. Only now is it safe to consider destroying anything: a
                // rebuild that ran before knowing the snapshot is good would delete
                // the damaged copy and then discover it has nothing to put back.
                "phase": if rebuild_storage { PHASE_REBUILDING } else { PHASE_WAITING_STORAGE },
                "phaseDeadline": deadline_after(700),
                "snapshotId": snapshot_id,
                "restoreAsOf": restore_as_of,
                "namespaces": namespaces_status,
                "resolvedNamespaces": namespaces,
                "catalog": catalog,
                // Surfaced by the UI so the user can see which build the data came from.
                "restoredFromVersion": catalog["catalog_version"].clone(),
            }),
        )
        .await;
}

// ── RebuildingStorage ─────────────────────────────────────────────────────────
//
// The phase that exists because "restore from backup" was four Ceph commands and a
// paragraph of explanation.
//
// The restore path below assumes storage is working but empty. After the disaster
// backups exist FOR, it is neither: a disk dies, its placement groups are gone for
// good, and CephFS cannot be repaired — with its metadata pool holed, the directory
// tree is gone and the surviving data is unreadable because nothing knows what file it
// belongs to. Ceph has no repair for that and never will; the filesystem has to be
// thrown away and made again.
//
// Doing that by hand means purging the dead OSD, failing and removing the filesystem,
// deleting both pools (they cannot be reused — a new filesystem over an old metadata
// pool gets an fsid mismatch), dropping every PVC that points at subvolumes which no
// longer exist, and only then restoring. That is not a recovery story for someone who
// just wants their photos back.
//
// So it is a phase. Same reconciler, same one bounded step per tick, same status the
// page already renders — and the button says "restore", once.
//
// ── Why this is gated twice ──────────────────────────────────────────────────
//
// It deletes pools. Every guard below is load-bearing:
//
//   1. Only when the caller explicitly asked for it (`rebuildStorage`). Never
//      inferred, never a fallback when something else fails.
//   2. Only when the loss is UNRECOVERABLE — checked here, against the cluster, not
//      taken on trust from whoever clicked. Storage that could still come back must
//      never be deleted because a restore was started impatiently.
//
// If the second check says the data might return, the phase refuses and the run fails
// with an explanation, rather than helpfully destroying a recoverable cluster.

const PHASE_REBUILDING: &str = "RebuildingStorage";

/// Pools this phase may delete, and the reason each one cannot simply be left.
///
/// `images` is included because it is size 1 by design, so a lost disk holes it too —
/// but nothing in it is the owner's data, it is re-pulled from registries, and
/// images-store.nix recreates it unprompted.
const REBUILD_POOLS: &[&str] = &["yolab-fs-metadata", "yolab-fs-data0", "images"];

/// Whether the cluster's own state justifies deleting its pools.
///
/// Separate from the flag on the request on purpose. The flag is the owner saying "you
/// may"; this is the cluster saying "you must, because none of this is coming back".
/// Both are required, and this one is checked at the moment of destruction rather than
/// at the moment of clicking — minutes apart, and a disk can be reconnected in between.
fn rebuild_is_justified(loss: &crate::routers::ceph::PgLoss) -> bool {
    loss.unrecoverable && loss.stuck > 0
}

/// Whether an OSD in `ceph osd tree` is one the cluster has given up on.
///
/// Both halves matter. `down` alone is a disk that may be seconds from coming back —
/// a reboot, a flapping cable — and mon_osd_down_out_interval has not finished
/// deciding. Purging on `down` alone would destroy a cluster that was about to heal.
/// `out` is Ceph's own conclusion, ten minutes in, that it is not coming back.
fn osd_is_lost(node: &Value) -> bool {
    node["type"].as_str() == Some("osd")
        && node["status"].as_str() == Some("down")
        && node["reweight"].as_f64().unwrap_or(1.0) == 0.0
}

async fn ceph_ok(args: &[&str]) -> Result<String, String> {
    crate::ceph_cli::ceph(args).await.map_err(|e| e.to_string())
}

/// One bounded pass of the teardown, driven by `rebuildStep` in the status so a crash
/// resumes where it stopped rather than starting the destruction again.
async fn step_rebuilding_storage(name: &str, run: &Value) {
    let at = run["status"]["rebuildStep"]
        .as_str()
        .unwrap_or("check")
        .to_string();

    match at.as_str() {
        // ── Refuse unless the data is genuinely gone ─────────────────────────
        "check" => {
            let loss = crate::routers::ceph::assess_pg_loss().await;
            match loss.filter(rebuild_is_justified) {
                Some(l) => {
                    tracing::warn!(
                        "restore-run {name}: rebuilding storage — {} of {} placement groups are unrecoverable",
                        l.stuck, l.total
                    );
                    let _ = RESTORE_RUN.patch_status(name, json!({
                        "rebuildStep": "purge",
                        "rebuildNote": format!(
                            "{} of {} groups of data were lost with the disk and cannot be rebuilt. Recreating storage, then restoring from your backup.",
                            l.stuck, l.total
                        ),
                    })).await;
                }
                _ => {
                    // Either healthy, or unreadable. Both mean: do not delete anything.
                    let _ = RESTORE_RUN.patch_status(name, json!({
                        "phase": PHASE_FAILED,
                        "finishedAt": Utc::now().to_rfc3339(),
                        "error": "Storage was not rebuilt because nothing here is permanently lost. Recreating it would throw away data that is still there. If a disk is disconnected, reconnect it.",
                    })).await;
                }
            }
        }

        // ── The dead disks leave the cluster ─────────────────────────────────
        "purge" => {
            let mut purged = Vec::new();
            if let Ok(tree) = crate::ceph_cli::ceph_json(&["osd", "tree"]).await {
                for node in tree["nodes"].as_array().cloned().unwrap_or_default() {
                    if !osd_is_lost(&node) {
                        continue;
                    }
                    let Some(id) = node["id"].as_i64() else {
                        continue;
                    };
                    let id_s = id.to_string();
                    if ceph_ok(&["osd", "purge", &id_s, "--yes-i-really-mean-it"])
                        .await
                        .is_ok()
                    {
                        purged.push(id);
                    }
                }
            }
            tracing::info!("restore-run {name}: purged OSDs {purged:?}");
            let _ = RESTORE_RUN
                .patch_status(
                    name,
                    json!({ "rebuildStep": "teardown", "purgedOsds": purged }),
                )
                .await;
        }

        // ── The filesystem and its pools go ──────────────────────────────────
        "teardown" => {
            // `fs fail` first: a filesystem with a live MDS refuses to be removed, and
            // the MDS would otherwise keep trying to read a metadata pool that is about
            // to stop existing.
            let _ = ceph_ok(&["fs", "fail", "yolab-fs"]).await;
            let _ = ceph_ok(&["fs", "rm", "yolab-fs", "--yes-i-really-mean-it"]).await;

            // Off by default, and it should be: it is the guard against exactly this
            // command being run by accident. Lifted for the deletes and put back
            // immediately, whether or not they worked.
            let _ = ceph_ok(&["config", "set", "mon", "mon_allow_pool_delete", "true"]).await;
            let mut failures = Vec::new();
            for pool in REBUILD_POOLS {
                if let Err(e) = ceph_ok(&[
                    "osd",
                    "pool",
                    "delete",
                    pool,
                    pool,
                    "--yes-i-really-really-mean-it",
                ])
                .await
                {
                    // A pool that is already gone is not a failure.
                    if !e.contains("does not exist") {
                        failures.push(format!("{pool}: {e}"));
                    }
                }
            }
            let _ = ceph_ok(&["config", "set", "mon", "mon_allow_pool_delete", "false"]).await;

            if !failures.is_empty() {
                let _ = RESTORE_RUN.patch_status(name, json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": format!("Could not clear the damaged storage: {}", failures.join("; ")),
                })).await;
                return;
            }
            let _ = RESTORE_RUN
                .patch_status(name, json!({ "rebuildStep": "claims" }))
                .await;
        }

        // ── The volume claims go with them ───────────────────────────────────
        "claims" => {
            // Every PVC names a CephFS subvolume that no longer exists. Left in place
            // they bind forever and the restore has nowhere to write. The apps' own
            // objects are restored from the cluster snapshot afterwards, so removing
            // them here loses nothing that is not already gone.
            let _ = crate::kubectl::run(&[
                "delete",
                "pvc",
                "--all",
                "--all-namespaces",
                "--selector",
                "app.kubernetes.io/managed-by!=ignore",
                "--wait=false",
            ])
            .await;
            // Released PVs do not go on their own once their claim is deleted.
            let _ = crate::kubectl::run(&["delete", "pv", "--all", "--wait=false"]).await;
            let _ = RESTORE_RUN
                .patch_status(name, json!({ "rebuildStep": "recreate" }))
                .await;
        }

        // ── And it is built again ────────────────────────────────────────────
        "recreate" => {
            let _ = crate::cephfs::ensure().await;
            let _ = RESTORE_RUN
                .patch_status(
                    name,
                    json!({
                        "phase": PHASE_WAITING_STORAGE,
                        "phaseDeadline": deadline_after(900),
                        "rebuildStep": "done",
                    }),
                )
                .await;
        }

        _ => {}
    }
}

/// WaitingForStorage: a single non-blocking check of whether CephFS is mountable right
/// now. Stays in this phase until it observes Ready, or until the deadline passes.
///
/// This used to read `.status.phase` off Rook's CephFilesystem CR. That CR no
/// longer exists — CephFS is created by local-api's cephfs reconciler — so
/// readiness is asked of Ceph directly: the filesystem must exist and have an
/// MDS that is actually `active`. A filesystem with no active MDS is present in
/// `fs ls` but cannot be mounted, so checking existence alone would let a
/// restore start against storage that then fails to bind.
async fn step_waiting_storage(name: &str, run: &Value) {
    let ready = match crate::ceph_cli::ceph_json(&["fs", "status", "yolab-fs"]).await {
        Ok(v) => v["mdsmap"]
            .as_array()
            .map(|m| {
                m.iter()
                    .any(|d| d["state"].as_str().unwrap_or("") == "active")
            })
            .unwrap_or(false),
        Err(_) => false,
    };
    if !ready {
        tracing::info!("restore-run {name}: yolab-fs has no active MDS yet — waiting");
        return;
    }
    let _ = run; // deadline handled by the sweep
    let _ = RESTORE_RUN
        .patch_status(
            name,
            json!({ "phase": PHASE_RESTORING, "phaseDeadline": deadline_after(5400) }),
        )
        .await;
}

// ── RestoringVolumes ──────────────────────────────────────────────────────────

/// One bounded pass: finish any namespace that still needs one-time setup, then advance
/// every non-terminal volume by exactly one observation. Never blocks on the cluster.
async fn step_restoring(name: &str, run: &Value, cfg: &BackupConfig) {
    let started = Instant::now();
    let mut ns_state = parse_ns_state(run);

    // A run created before `resolvedNamespaces` existed, or one whose namespaces array
    // got truncated, still needs entries to work from.
    let resolved: Vec<String> = run["status"]["resolvedNamespaces"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    for ns in &resolved {
        if !ns_state.iter().any(|s| &s.namespace == ns) {
            ns_state.push(NamespaceState {
                namespace: ns.clone(),
                setup_complete: false,
                scaled_deployments: Vec::new(),
                volumes: Vec::new(),
            });
        }
    }

    // ── Phase A: one-time per-namespace setup ────────────────────────────────
    let mut i = 0;
    while i < ns_state.len() {
        if !ns_state[i].setup_complete {
            setup_namespace(name, &mut ns_state, i, run, cfg).await;
            persist_namespaces(name, &ns_state).await;
            if started.elapsed().as_secs() >= STEP_BUDGET_SECS {
                return; // next tick continues with the next namespace
            }
        }
        i += 1;
    }

    // ── Phase B: advance each volume by one observation ──────────────────────
    let rd_items = list_replication_destinations().await;
    let live_pvcs = list_pvc_keys().await;
    let restore_as_of = run["status"]["restoreAsOf"].as_str().map(String::from);
    let catalog = run["status"]["catalog"].clone();
    let now = Utc::now();
    let mut budget_hit = false;

    for si in 0..ns_state.len() {
        for vi in 0..ns_state[si].volumes.len() {
            if vol_is_terminal(&ns_state[si].volumes[vi].phase) {
                continue;
            }
            if started.elapsed().as_secs() >= STEP_BUDGET_SECS {
                budget_hit = true;
                break;
            }
            advance_volume(
                name,
                &mut ns_state,
                si,
                vi,
                cfg,
                &catalog,
                restore_as_of.as_deref(),
                &rd_items,
                &live_pvcs,
                now,
            )
            .await;
        }
        if budget_hit {
            break;
        }
    }

    persist_namespaces(name, &ns_state).await;

    let all_done = ns_state
        .iter()
        .flat_map(|s| &s.volumes)
        .all(|v| vol_is_terminal(&v.phase));
    if all_done && !budget_hit {
        let _ = RESTORE_RUN
            .patch_status(
                name,
                json!({ "phase": PHASE_APPLYING, "phaseDeadline": deadline_after(300) }),
            )
            .await;
    }
}

/// One-time work for a namespace: create it, re-apply its backed-up objects (which is
/// what restores its `yolab.io/*` annotations, so the app keeps its identity, config and
/// outputs in the UI), record the original replica counts, then scale to zero.
///
/// `scaledDeployments` is persisted BEFORE the scale-down: a crash in that window would
/// otherwise lose the counts, and a redo would read the already-zeroed deployments and
/// "restore" them to 0.
async fn setup_namespace(
    name: &str,
    ns_state: &mut [NamespaceState],
    idx: usize,
    run: &Value,
    cfg: &BackupConfig,
) {
    let ns = ns_state[idx].namespace.clone();
    let snapshot_id = run["status"]["snapshotId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let catalog = run["status"]["catalog"].clone();
    let repo = cfg.restic_repo("cluster-backup");

    let ns_exists = Command::new("kubectl")
        .args(["get", "namespace", &ns])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ns_exists {
        if let Err(e) = kubectl_apply(
            &json!({
                "apiVersion": "v1", "kind": "Namespace",
                "metadata": { "name": &ns, "labels": { "yolab.io/managed": "true" } }
            })
            .to_string(),
        )
        .await
        {
            tracing::warn!("restore-run {name}: {ns}: create namespace: {e}");
        }
    }

    // Apply the backed-up objects. The export now leads with the Namespace itself, so
    // this is also what puts yolab.io/app-id, yolab.io/config and yolab.io/outputs back.
    let yaml_target = format!("/tmp/yolab-dr-yaml-{}", random_hex(8));
    let pattern = format!("**/{ns}.yaml");
    let r = Command::new("restic")
        .args([
            "restore",
            &snapshot_id,
            "--target",
            &yaml_target,
            "--include",
            &pattern,
        ])
        .env("RESTIC_REPOSITORY", &repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await;
    if let Ok(o) = r {
        if o.status.success() {
            if let Ok(f) = Command::new("find")
                .args([&yaml_target, "-name", &format!("{ns}.yaml"), "-type", "f"])
                .output()
                .await
            {
                let yaml_path = String::from_utf8_lossy(&f.stdout).trim().to_string();
                if !yaml_path.is_empty() {
                    if let Ok(bytes) = tokio::fs::read(&yaml_path).await {
                        if let Err(e) = kubectl_apply(&String::from_utf8_lossy(&bytes)).await {
                            tracing::warn!("restore-run {name}: {ns}: YAML apply partial: {e}");
                        }
                    }
                }
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&yaml_target).await;

    // Record replica counts, but never overwrite counts a previous attempt already
    // recorded — after a crash the deployments may already be at 0, and re-reading them
    // would bake that in as "the original".
    if ns_state[idx].scaled_deployments.is_empty() {
        ns_state[idx].scaled_deployments = read_deployment_scales(&ns).await;
        persist_namespaces(name, ns_state).await;
    }

    let _ = Command::new("kubectl")
        .args(["scale", "deployment", "--all", "-n", &ns, "--replicas=0"])
        .output()
        .await;

    // Seed the volume list from the snapshot's catalog.
    if ns_state[idx].volumes.is_empty() {
        ns_state[idx].volumes = catalog["services"]
            .as_array()
            .and_then(|svcs| {
                svcs.iter()
                    .find(|s| s["namespace"].as_str() == Some(ns.as_str()))
            })
            .and_then(|s| s["pvcs"].as_array())
            .map(|pvcs| {
                pvcs.iter()
                    .filter_map(|p| {
                        Some(VolumeState {
                            pvc: p["name"].as_str()?.to_string(),
                            phase: VOL_PENDING.to_string(),
                            deleting_since: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    ns_state[idx].setup_complete = true;
}

/// Advance one volume by exactly one non-blocking observation.
#[allow(clippy::too_many_arguments)]
async fn advance_volume(
    name: &str,
    ns_state: &mut [NamespaceState],
    si: usize,
    vi: usize,
    cfg: &BackupConfig,
    catalog: &Value,
    restore_as_of: Option<&str>,
    rd_items: &[Value],
    live_pvcs: &std::collections::HashSet<(String, String)>,
    now: DateTime<Utc>,
) {
    let ns = ns_state[si].namespace.clone();
    let pvc = ns_state[si].volumes[vi].pvc.clone();
    let phase = ns_state[si].volumes[vi].phase.clone();

    match phase.as_str() {
        VOL_PENDING => {
            if let Err(e) = ensure_restic_secret(&ns, &pvc, cfg).await {
                tracing::warn!("restore-run {name}: {ns}/{pvc}: restic secret: {e}");
            }
            let _ = ensure_replication_source(
                &PvcInfo {
                    namespace: ns.clone(),
                    name: pvc.clone(),
                },
                false,
            )
            .await;

            let pvc_repo = cfg.restic_repo(&format!("volsync/{ns}/{}", canonical_pvc_id(&pvc)));
            restic_unlock(
                &pvc_repo,
                &cfg.restic_password,
                &cfg.access_key_id,
                &cfg.secret_access_key,
            )
            .await;

            match snapshots_exist(&pvc_repo, cfg).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        "restore-run {name}: {ns}/{pvc}: no backup snapshot found — PVC preserved"
                    );
                    ns_state[si].volumes[vi].phase = VOL_SKIPPED.to_string();
                    return;
                }
                Err(e) => {
                    // Could not tell — treat as failure rather than silently skipping,
                    // which would report a successful restore that changed nothing.
                    tracing::warn!("restore-run {name}: {ns}/{pvc}: snapshot check failed: {e}");
                    ns_state[si].volumes[vi].phase = VOL_FAILED.to_string();
                    return;
                }
            }

            // Non-blocking delete; the Deleting branch below waits it out across ticks.
            let _ = Command::new("kubectl")
                .args([
                    "delete",
                    "pvc",
                    &pvc,
                    "-n",
                    &ns,
                    "--wait=false",
                    "--ignore-not-found",
                ])
                .output()
                .await;
            ns_state[si].volumes[vi].phase = VOL_DELETING.to_string();
            ns_state[si].volumes[vi].deleting_since = Some(now.to_rfc3339());
        }

        VOL_DELETING => {
            if live_pvcs.contains(&(ns.clone(), pvc.clone())) {
                if delete_timed_out(ns_state[si].volumes[vi].deleting_since.as_deref(), now) {
                    tracing::warn!(
                        "restore-run {name}: {ns}/{pvc}: PVC still present after \
                         {PVC_DELETE_TIMEOUT_SECS}s — a pod may still be mounting it"
                    );
                    ns_state[si].volumes[vi].phase = VOL_FAILED.to_string();
                }
                return; // still deleting — check again next tick
            }

            let capacity = catalog_capacity(catalog, &ns, &pvc);
            if let Err(e) =
                ensure_destination_pvc(&pvc, &ns, &capacity, "yolab-cephfs", "ReadWriteMany").await
            {
                tracing::warn!("restore-run {name}: {ns}/{pvc}: create pvc: {e}");
                ns_state[si].volumes[vi].phase = VOL_FAILED.to_string();
                return;
            }

            annotate_ns_privileged_movers(&ns).await;
            let secret_name = format!("{}{RESTIC_SECRET_SUFFIX}", canonical_pvc_id(&pvc));
            let mut restic_spec = json!({
                "repository": secret_name,
                "copyMethod": "Direct",
                "cacheStorageClassName": "yolab-cephfs",
                "destinationPVC": pvc,
                "moverSecurityContext": { "runAsUser": 0, "runAsGroup": 0, "fsGroup": 0 }
            });
            if let Some(t) = restore_as_of {
                restic_spec["restoreAsOf"] = Value::String(t.to_string());
            }
            let dest_name = format!("emergency-restore-{}", canonical_pvc_id(&pvc));
            let manifest = json!({
                "apiVersion": "volsync.backube/v1alpha1",
                "kind": "ReplicationDestination",
                "metadata": {
                    "name": dest_name, "namespace": ns,
                    "labels": { "app.kubernetes.io/managed-by": "yolab" }
                },
                "spec": {
                    "trigger": { "manual": format!("dr-{}", now.format("%Y%m%d%H%M%S")) },
                    "restic": restic_spec
                }
            });
            match kubectl_apply(&manifest.to_string()).await {
                Ok(_) => ns_state[si].volumes[vi].phase = VOL_RESTORING.to_string(),
                Err(e) => {
                    tracing::warn!("restore-run {name}: {ns}/{pvc}: RD: {e}");
                    ns_state[si].volumes[vi].phase = VOL_FAILED.to_string();
                }
            }
        }

        VOL_RESTORING => {
            let dest_name = format!("emergency-restore-{}", canonical_pvc_id(&pvc));
            let result = rd_items
                .iter()
                .find(|i| {
                    i["metadata"]["name"].as_str() == Some(dest_name.as_str())
                        && i["metadata"]["namespace"].as_str() == Some(ns.as_str())
                })
                .and_then(|i| i["status"]["latestMoverStatus"]["result"].as_str());
            if let Some(next) = rd_result_to_phase(result) {
                ns_state[si].volumes[vi].phase = next.to_string();
            }
        }

        _ => {}
    }
}

/// Applying: scale every namespace's deployments back to their ORIGINAL replica count,
/// clean up completed ReplicationDestinations, compute the terminal phase. Safe to redo
/// in full — scaling to an already-current count is a no-op, RD delete ignores not-found.
///
/// This is the only code that turns the apps back on, and the only place a terminal phase
/// is written for a run that got as far as touching the cluster.
async fn step_applying(name: &str, run: &Value) {
    let ns_state = parse_ns_state(run);
    let abort_reason = run["status"]["abortReason"].as_str().map(String::from);

    for state in &ns_state {
        // Unconditional: whatever happened to the data, the app must not stay dark.
        for deploy in &state.scaled_deployments {
            if let Err(e) = scale_deployment(&state.namespace, &deploy.name, deploy.replicas).await
            {
                tracing::warn!(
                    "restore-run {name}: {}/{}: scale to {}: {e}",
                    state.namespace,
                    deploy.name,
                    deploy.replicas
                );
            }
        }
        for vol in &state.volumes {
            if vol.phase == VOL_SUCCEEDED || vol.phase == VOL_FAILED {
                let dest_name = format!("emergency-restore-{}", canonical_pvc_id(&vol.pvc));
                delete_replication_destination_without_touching_pvc(&dest_name, &state.namespace)
                    .await;
            }
        }
    }

    let all_volumes: Vec<&VolumeState> = ns_state.iter().flat_map(|s| &s.volumes).collect();
    let succeeded = all_volumes
        .iter()
        .filter(|v| v.phase == VOL_SUCCEEDED)
        .count();
    let total = all_volumes.len();
    let phase = terminal_phase(&all_volumes, abort_reason.is_some());

    let mut status = json!({
        "phase": phase,
        "finishedAt": Utc::now().to_rfc3339(),
        "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
    });
    if let Some(reason) = &abort_reason {
        status["error"] = Value::String(reason.clone());
    }
    let _ = RESTORE_RUN.patch_status(name, status).await;
    tracing::info!("restore-run {name}: {phase} ({succeeded}/{total} volumes restored)");
}

// ── Cluster observation helpers (dumb I/O, no decisions) ─────────────────────

async fn list_replication_destinations() -> Vec<Value> {
    Command::new("kubectl")
        .args(["get", "replicationdestination", "-A", "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
}

/// (namespace, name) of every PVC that currently exists — one call, so the Deleting
/// branch can check every volume without a `kubectl get` each.
async fn list_pvc_keys() -> std::collections::HashSet<(String, String)> {
    Command::new("kubectl")
        .args(["get", "pvc", "-A", "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| {
            Some((
                i["metadata"]["namespace"].as_str()?.to_string(),
                i["metadata"]["name"].as_str()?.to_string(),
            ))
        })
        .collect()
}

async fn read_deployment_scales(ns: &str) -> Vec<DeploymentScale> {
    Command::new("kubectl")
        .args(["get", "deployments", "-n", ns, "-o", "json"])
        .output()
        .await
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            Some(DeploymentScale {
                name: d["metadata"]["name"].as_str()?.to_string(),
                // An absent spec.replicas means 1 per the Deployment defaulting rules.
                replicas: d["spec"]["replicas"].as_u64().unwrap_or(1) as u32,
            })
        })
        .collect()
}

/// Whether a per-PVC restic repo has any snapshot. `Err` means "couldn't tell" and must
/// NOT be collapsed into `false` — that is what turns a broken restore into a silent
/// "Skipped, everything fine".
async fn snapshots_exist(repo: &str, cfg: &BackupConfig) -> anyhow::Result<bool> {
    let out = Command::new("restic")
        .args(["snapshots", "--json"])
        .env("RESTIC_REPOSITORY", repo)
        .env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A repo that was never initialised legitimately has no snapshots.
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(false);
        }
        anyhow::bail!("{}", stderr.trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(v.as_array().map(|a| !a.is_empty()).unwrap_or(false))
}

fn catalog_capacity(catalog: &Value, ns: &str, pvc: &str) -> String {
    catalog["services"]
        .as_array()
        .and_then(|svcs| svcs.iter().find(|s| s["namespace"].as_str() == Some(ns)))
        .and_then(|s| s["pvcs"].as_array())
        .and_then(|pvcs| pvcs.iter().find(|p| p["name"].as_str() == Some(pvc)))
        .and_then(|p| p["capacity"].as_str())
        .unwrap_or("10Gi")
        .to_string()
}

async fn persist_namespaces(name: &str, ns_state: &[NamespaceState]) {
    let _ = RESTORE_RUN
        .patch_status(
            name,
            json!({ "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>() }),
        )
        .await;
}

// ── State model ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
struct VolumeState {
    pvc: String,
    phase: String,
    /// When the PVC delete was issued — bounds the Deleting sub-phase.
    deleting_since: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct DeploymentScale {
    name: String,
    replicas: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct NamespaceState {
    namespace: String,
    setup_complete: bool,
    scaled_deployments: Vec<DeploymentScale>,
    volumes: Vec<VolumeState>,
}

impl NamespaceState {
    fn to_json(&self) -> Value {
        json!({
            "namespace": self.namespace,
            "setupComplete": self.setup_complete,
            "scaledDeployments": self.scaled_deployments.iter()
                .map(|d| json!({ "name": d.name, "replicas": d.replicas }))
                .collect::<Vec<_>>(),
            "volumes": self.volumes.iter().map(|v| {
                let mut o = json!({ "pvc": v.pvc, "phase": v.phase });
                if let Some(ts) = &v.deleting_since {
                    o["deletingSince"] = Value::String(ts.clone());
                }
                o
            }).collect::<Vec<_>>(),
        })
    }
}

/// Accepts both the current `[{name, replicas}]` shape and the older bare-`[name]` shape,
/// so a run created by a previous build still finishes correctly after an upgrade.
fn parse_scaled_deployments(v: &Value) -> Vec<DeploymentScale> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            if let Some(n) = d.as_str() {
                // Old shape recorded names only; 1 is what that build always scaled to.
                return Some(DeploymentScale {
                    name: n.to_string(),
                    replicas: 1,
                });
            }
            Some(DeploymentScale {
                name: d["name"].as_str()?.to_string(),
                replicas: d["replicas"].as_u64().unwrap_or(1) as u32,
            })
        })
        .collect()
}

/// Reconstructs `NamespaceState` from `status.namespaces` — the only place this data
/// lives between ticks (never in a task-local variable), so any tick, in any process,
/// reads the exact same state the previous tick left behind.
fn parse_ns_state(run: &Value) -> Vec<NamespaceState> {
    run["status"]["namespaces"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|n| NamespaceState {
            namespace: n["namespace"].as_str().unwrap_or("").to_string(),
            setup_complete: n["setupComplete"].as_bool().unwrap_or(false),
            scaled_deployments: parse_scaled_deployments(&n["scaledDeployments"]),
            volumes: n["volumes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|v| VolumeState {
                    pvc: v["pvc"].as_str().unwrap_or("").to_string(),
                    phase: v["phase"].as_str().unwrap_or(VOL_PENDING).to_string(),
                    deleting_since: v["deletingSince"].as_str().map(String::from),
                })
                .collect(),
        })
        .collect()
}

// ── Reconcile tick ────────────────────────────────────────────────────────────

/// Only the lease holder acts. Times out any run stuck past its phase deadline — routing
/// it through recovery rather than straight to terminal — then advances every still-active
/// run by exactly one step.
pub async fn reconcile_tick(holder: &str) {
    let Some(_guard) = lease::acquire(LEASE_NAME, holder, LEASE_DURATION_SECS).await else {
        return;
    };

    let mut seen_terminal = 0usize;
    for run in RESTORE_RUN.list().await {
        // newest-created first
        if is_terminal(run["status"]["phase"].as_str().unwrap_or("")) {
            seen_terminal += 1;
            if seen_terminal > KEEP_TERMINAL_RUNS {
                if let Some(name) = run["metadata"]["name"].as_str() {
                    RESTORE_RUN.delete(name).await;
                }
            }
        }
    }

    // Timeout sweep. Unlike before, a timed-out run is NOT marked Failed here — that is
    // terminal, and terminal runs are never stepped, so it would strand every scaled-down
    // deployment at 0 replicas forever. `terminate` routes it into Applying instead when
    // there is anything to recover, and the run reaches a terminal phase from there.
    let mut swept: std::collections::HashSet<String> = std::collections::HashSet::new();
    for run in RESTORE_RUN.list().await {
        let phase = run["status"]["phase"]
            .as_str()
            .unwrap_or(PHASE_VALIDATING)
            .to_string();
        if is_terminal(&phase) {
            continue;
        }
        if let Some(dl) = parse_deadline(&run) {
            if Utc::now() > dl {
                let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                terminate(&name, &run, format!("timed out in phase {phase}")).await;
                swept.insert(name);
            }
        }
    }

    // Advance every still-active run by exactly one step. Runs that were just swept are
    // skipped this tick — they have a fresh Applying deadline and get stepped next tick.
    for run in RESTORE_RUN.list().await {
        let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || swept.contains(&name) {
            continue;
        }
        if is_terminal(run["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING)) {
            continue;
        }
        step(&name).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(name: &str, scaled: &[(&str, u32)], vols: &[(&str, &str)]) -> NamespaceState {
        NamespaceState {
            namespace: name.into(),
            setup_complete: true,
            scaled_deployments: scaled
                .iter()
                .map(|(n, r)| DeploymentScale {
                    name: (*n).into(),
                    replicas: *r,
                })
                .collect(),
            volumes: vols
                .iter()
                .map(|(p, ph)| VolumeState {
                    pvc: (*p).into(),
                    phase: (*ph).into(),
                    deleting_since: None,
                })
                .collect(),
        }
    }

    // ── abort_phase: the invariant that keeps apps from staying dark ──────────

    #[test]
    fn abort_routes_through_applying_once_anything_was_scaled_down() {
        // THE regression test for this module: a run that scaled deployments down must
        // reach Applying even when it is giving up, because Applying is the only code
        // that scales them back up. Marking it Failed here is what left two real apps at
        // 0 replicas until a human noticed.
        let state = [ns(
            "yolab-gitea",
            &[("gitea", 1)],
            &[("gitea-data", VOL_RESTORING)],
        )];
        assert_eq!(abort_phase(PHASE_RESTORING, &state), PHASE_APPLYING);
    }

    // ── RebuildingStorage ─────────────────────────────────────────────────────
    //
    // This phase deletes pools. These pin the two questions that decide whether it is
    // allowed to, because getting either wrong turns a recoverable outage into a
    // permanent one.

    use crate::routers::ceph::PgLoss;

    fn pgloss(stuck: u32, total: u32, unrecoverable: bool) -> PgLoss {
        PgLoss {
            stuck,
            total,
            unrecoverable,
        }
    }

    /// The case this exists for: a disk died in a cluster keeping one copy, and the
    /// data is not coming back however long anyone waits.
    #[test]
    fn permanent_loss_justifies_rebuilding() {
        assert!(rebuild_is_justified(&pgloss(63, 81, true)));
    }

    /// The case that must never be rebuilt. Replicated data that is briefly
    /// unavailable IS coming back, and deleting the pools would turn a self-healing
    /// outage into total loss — with the owner having asked for a restore, which reads
    /// as the safe choice.
    #[test]
    fn recoverable_loss_never_justifies_rebuilding() {
        assert!(!rebuild_is_justified(&pgloss(63, 81, false)));
        assert!(!rebuild_is_justified(&pgloss(1, 81, false)));
    }

    /// Nothing stuck means nothing lost, whatever else the flag says.
    #[test]
    fn a_healthy_cluster_is_never_rebuilt() {
        assert!(!rebuild_is_justified(&pgloss(0, 81, true)));
        assert!(!rebuild_is_justified(&pgloss(0, 81, false)));
    }

    fn osd(id: i64, status: &str, reweight: f64) -> Value {
        json!({"type": "osd", "id": id, "status": status, "reweight": reweight})
    }

    /// `out` is Ceph's own conclusion, ten minutes in, that a disk is not returning.
    #[test]
    fn only_a_disk_ceph_has_given_up_on_is_purged() {
        assert!(osd_is_lost(&osd(1, "down", 0.0)));
    }

    /// The dangerous near-miss: a disk that is down but still `in` may be seconds from
    /// coming back — a reboot, a loose cable — and mon_osd_down_out_interval has not
    /// finished deciding. Purging it destroys a cluster that was about to heal.
    #[test]
    fn a_disk_that_is_merely_down_is_left_alone() {
        assert!(!osd_is_lost(&osd(1, "down", 1.0)));
    }

    #[test]
    fn a_healthy_disk_is_left_alone() {
        assert!(!osd_is_lost(&osd(0, "up", 1.0)));
        // Drained on purpose — up, but weighted out. Its data has already moved.
        assert!(!osd_is_lost(&osd(0, "up", 0.0)));
    }

    /// Hosts and roots appear in the same array and have neither status nor reweight.
    #[test]
    fn non_osd_tree_nodes_are_never_purged() {
        assert!(!osd_is_lost(
            &json!({"type": "host", "id": -3, "name": "node1"})
        ));
        assert!(!osd_is_lost(
            &json!({"type": "root", "id": -1, "name": "default"})
        ));
        assert!(!osd_is_lost(&json!({})));
    }

    #[test]
    fn abort_fails_flat_when_nothing_was_touched() {
        // Validating/WaitingForStorage never mutate the cluster — nothing to recover.
        let state = [ns("yolab-gitea", &[], &[])];
        assert_eq!(abort_phase(PHASE_VALIDATING, &state), PHASE_FAILED);
        assert_eq!(abort_phase(PHASE_WAITING_STORAGE, &[]), PHASE_FAILED);
    }

    #[test]
    fn abort_routes_through_applying_even_with_no_volumes() {
        // A namespace with no PVCs still had its deployments scaled to 0.
        let state = [ns("yolab-cinny", &[("cinny", 2)], &[])];
        assert_eq!(abort_phase(PHASE_RESTORING, &state), PHASE_APPLYING);
    }

    #[test]
    fn aborting_from_applying_terminates_instead_of_looping() {
        // Applying IS the recovery. Routing it back into itself on a timeout would
        // re-arm the deadline on every tick and the run would never finish — a restore
        // stuck "Bringing services back up" forever, which reads to the user exactly
        // like the hang this whole design exists to prevent.
        let state = [ns(
            "yolab-gitea",
            &[("gitea", 1)],
            &[("gitea-data", VOL_SUCCEEDED)],
        )];
        assert_eq!(abort_phase(PHASE_APPLYING, &state), PHASE_FAILED);
    }

    // ── terminal_phase ───────────────────────────────────────────────────────

    #[test]
    fn terminal_all_succeeded_is_success() {
        let s = ns("a", &[], &[("p1", VOL_SUCCEEDED), ("p2", VOL_SUCCEEDED)]);
        let v: Vec<&VolumeState> = s.volumes.iter().collect();
        assert_eq!(terminal_phase(&v, false), PHASE_SUCCEEDED);
    }

    #[test]
    fn terminal_all_skipped_is_success_not_failure() {
        // Nothing was restored because nothing had ever been backed up — the live PVCs
        // were deliberately left alone. That is not a failed restore.
        let s = ns("a", &[], &[("p1", VOL_SKIPPED)]);
        let v: Vec<&VolumeState> = s.volumes.iter().collect();
        assert_eq!(terminal_phase(&v, false), PHASE_SUCCEEDED);
    }

    #[test]
    fn terminal_all_failed_is_failure() {
        let s = ns("a", &[], &[("p1", VOL_FAILED), ("p2", VOL_FAILED)]);
        let v: Vec<&VolumeState> = s.volumes.iter().collect();
        assert_eq!(terminal_phase(&v, false), PHASE_FAILED);
    }

    #[test]
    fn terminal_mixed_is_partial() {
        let s = ns(
            "a",
            &[("d", 1)],
            &[("p1", VOL_SUCCEEDED), ("p2", VOL_FAILED)],
        );
        let v: Vec<&VolumeState> = s.volumes.iter().collect();
        assert_eq!(terminal_phase(&v, false), PHASE_PARTIAL);
    }

    #[test]
    fn terminal_aborted_run_is_never_reported_as_success() {
        // Even if every volume finished, reaching Applying via terminate() means the run
        // hit a timeout or a hard error — saying "Succeeded" would be a lie.
        let s = ns("a", &[("d", 1)], &[("p1", VOL_SUCCEEDED)]);
        let v: Vec<&VolumeState> = s.volumes.iter().collect();
        assert_eq!(terminal_phase(&v, true), PHASE_PARTIAL);
        assert_eq!(terminal_phase(&[], true), PHASE_FAILED);
    }

    // ── volume progression ───────────────────────────────────────────────────

    #[test]
    fn rd_result_mapping_is_case_insensitive_and_conservative() {
        assert_eq!(rd_result_to_phase(Some("Successful")), Some(VOL_SUCCEEDED));
        assert_eq!(rd_result_to_phase(Some("successful")), Some(VOL_SUCCEEDED));
        assert_eq!(rd_result_to_phase(Some("Failed")), Some(VOL_FAILED));
        // Anything else (missing status, in-progress, unknown string) means "keep waiting".
        assert_eq!(rd_result_to_phase(None), None);
        assert_eq!(rd_result_to_phase(Some("")), None);
        assert_eq!(rd_result_to_phase(Some("InProgress")), None);
    }

    #[test]
    fn delete_timeout_respects_the_recorded_timestamp() {
        let now = Utc::now();
        let recent = (now - chrono::Duration::seconds(10)).to_rfc3339();
        let ancient = (now - chrono::Duration::seconds(PVC_DELETE_TIMEOUT_SECS + 1)).to_rfc3339();
        assert!(!delete_timed_out(Some(&recent), now));
        assert!(delete_timed_out(Some(&ancient), now));
        // A missing timestamp must not strand the volume in Deleting forever.
        assert!(delete_timed_out(None, now));
    }

    // ── snapshot selection ───────────────────────────────────────────────────

    #[test]
    fn latest_snapshot_takes_newest_not_first() {
        // restic returns ascending by time; taking .first() restores the OLDEST backup.
        let snaps = json!([
            { "id": "oldest", "time": "2026-07-01T00:00:00Z" },
            { "id": "middle", "time": "2026-07-15T00:00:00Z" },
            { "id": "newest", "time": "2026-07-30T00:00:00Z" }
        ]);
        assert_eq!(latest_snapshot_id(&snaps).as_deref(), Some("newest"));
    }

    #[test]
    fn latest_snapshot_handles_empty_and_malformed() {
        assert_eq!(latest_snapshot_id(&json!([])), None);
        assert_eq!(latest_snapshot_id(&json!({})), None);
    }

    // ── status round-tripping ────────────────────────────────────────────────

    #[test]
    fn ns_state_round_trips_through_status_json() {
        let original = vec![ns(
            "yolab-gitea",
            &[("gitea", 3)],
            &[("gitea-data", VOL_RESTORING)],
        )];
        let encoded = json!({
            "status": { "namespaces": original.iter().map(NamespaceState::to_json).collect::<Vec<_>>() }
        });
        assert_eq!(parse_ns_state(&encoded), original);
    }

    #[test]
    fn parse_accepts_legacy_bare_string_deployments() {
        // Runs created before replica counts were recorded must still finish after an
        // upgrade, defaulting to the 1 replica that build would have applied.
        let legacy = json!({ "status": { "namespaces": [{
            "namespace": "yolab-gitea",
            "scaledDeployments": ["gitea", "gateway"],
            "volumes": [{ "pvc": "gitea-data", "phase": "Restoring" }]
        }]}});
        let parsed = parse_ns_state(&legacy);
        assert_eq!(
            parsed[0].scaled_deployments,
            vec![
                DeploymentScale {
                    name: "gitea".into(),
                    replicas: 1
                },
                DeploymentScale {
                    name: "gateway".into(),
                    replicas: 1
                },
            ]
        );
        // Legacy status had no setupComplete — must not re-run destructive setup blindly;
        // false here is safe because setup is idempotent and re-reads nothing it shouldn't.
        assert!(!parsed[0].setup_complete);
    }

    #[test]
    fn replica_counts_survive_the_round_trip() {
        // The bug this guards: Applying used to scale everything to 1 unconditionally,
        // silently rewriting the scale of any app that ran more than one replica.
        let state = [ns("a", &[("web", 3), ("worker", 2)], &[])];
        let encoded = json!({ "status": { "namespaces": state.iter().map(NamespaceState::to_json).collect::<Vec<_>>() } });
        let parsed = parse_ns_state(&encoded);
        assert_eq!(parsed[0].scaled_deployments[0].replicas, 3);
        assert_eq!(parsed[0].scaled_deployments[1].replicas, 2);
    }
}
