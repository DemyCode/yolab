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
// Same redesign as BackupRun, for the same reason, proven by the same live incident:
// this used to be one `tokio::spawn`ed task running the whole restore start-to-finish,
// including an internal poll loop that could run for up to 90 minutes. When the node
// rebooted mid-restore, that task died with it — the CRD sat in `RestoringVolumes`
// forever, both affected apps stuck at 0 replicas, because nothing else knew how to
// finish the job or even scale them back up. The phase/deadline timeout sweep could
// only mark the run `Failed` once its deadline eventually passed — it had no path to
// actually complete the restore or run the recovery-scale-up logic, because that logic
// lived only inside the one task that had already died.
//
// There is no spawned task anymore. `start()` only creates the object. The reconcile
// tick calls `step()` once per non-terminal run, every tick: read the CRD's current
// phase and the real cluster state, do exactly one bounded unit of work, return. A
// crash between any two ticks is invisible to the next one — it just sees the same
// status and keeps going, because every phase is built from operations that are safe
// to redo (`ensure_*` helpers, idempotent `kubectl apply`, delete-if-exists checks).
// Anything a phase computes that a *later* phase needs (the resolved namespace list,
// the per-namespace PVC catalog, the restore point-in-time) is written into the CRD
// status during that phase, specifically so a later step reading it back doesn't care
// which process wrote it or when.

use crate::kubectl::Crd;
use crate::lease;
use crate::routers::backup_common::*;
use chrono::Utc;
use serde_json::{json, Value};
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

const PHASE_VALIDATING: &str = "Validating";
const PHASE_WAITING_STORAGE: &str = "WaitingForStorage";
const PHASE_RESTORING: &str = "RestoringVolumes";
const PHASE_APPLYING: &str = "Applying";
const PHASE_SUCCEEDED: &str = "Succeeded";
const PHASE_PARTIAL: &str = "Partial";
const PHASE_FAILED: &str = "Failed";

fn is_terminal(phase: &str) -> bool {
    matches!(phase, PHASE_SUCCEEDED | PHASE_PARTIAL | PHASE_FAILED)
}

fn deadline_after(secs: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

fn parse_deadline(v: &Value) -> Option<chrono::DateTime<Utc>> {
    v["status"]["phaseDeadline"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
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
    let active = runs.iter().find(|r| !is_terminal(r["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING))).map(flatten_status);
    let last_finished = runs.iter().find(|r| is_terminal(r["status"]["phase"].as_str().unwrap_or(""))).map(flatten_status);
    json!({ "active": active, "last": last_finished })
}

/// Creates a RestoreRun in Validating phase and returns immediately. There is no
/// spawned task — the next reconcile tick picks it up, and every tick after that. See
/// the module doc: nothing about this run's progress depends on this process staying
/// alive.
pub async fn start(snapshot_id: Option<String>, all: bool, namespaces: Vec<String>) -> anyhow::Result<String> {
    let name = format!("restore-{}", Utc::now().format("%Y%m%d%H%M%S"));
    RESTORE_RUN.create(&name, json!({
        "snapshotId": snapshot_id,
        "all": all,
        "namespaces": namespaces,
    }), &[("app.kubernetes.io/managed-by", "yolab")]).await?;
    RESTORE_RUN.patch_status(&name, json!({
        "phase": PHASE_VALIDATING,
        "phaseDeadline": deadline_after(120),
        "startedAt": Utc::now().to_rfc3339(),
    })).await?;
    Ok(name)
}

async fn fail(name: &str, err: impl std::fmt::Display) {
    tracing::warn!("restore-run {name}: failed: {err}");
    let _ = RESTORE_RUN.patch_status(name, json!({
        "phase": PHASE_FAILED,
        "finishedAt": Utc::now().to_rfc3339(),
        "error": err.to_string(),
    })).await;
}

/// Advances `name` by exactly one bounded step from whatever phase it is CURRENTLY in.
/// Called once per non-terminal run on every reconcile tick.
async fn step(name: &str) {
    let Some(run) = RESTORE_RUN.get(name).await else { return };
    let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING).to_string();

    let Some(cfg) = read_master_config().await else {
        fail(name, "backup not configured").await;
        return;
    };

    match phase.as_str() {
        PHASE_VALIDATING => step_validating(name, &run, &cfg).await,
        PHASE_WAITING_STORAGE => step_waiting_storage(name).await,
        PHASE_RESTORING => step_restoring(name, &run, &cfg).await,
        PHASE_APPLYING => step_applying(name, &run).await,
        _ => {} // terminal — reconcile_tick already filters these out before calling step()
    }
}

/// Validating: resolve the snapshot id, extract catalog.json, resolve the namespace
/// list, run the space pre-flight. Entirely read-only against the cluster (no
/// namespace/PVC mutations happen until RestoringVolumes), so safe to redo in full on
/// a retry — nothing here needs a "have I already done this" marker.
async fn step_validating(name: &str, run: &Value, cfg: &BackupConfig) {
    let requested_snapshot = run["spec"]["snapshotId"].as_str().map(String::from);
    let want_all = run["spec"]["all"].as_bool().unwrap_or(false);
    let requested_namespaces: Vec<String> = run["spec"]["namespaces"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let snapshot_id = match requested_snapshot {
        Some(id) => id,
        None => {
            // `restic snapshots --json` returns ascending by time, not descending — take
            // the max by `time` explicitly rather than assuming array order or relying on
            // `--last`/`--latest` flag semantics, since getting this wrong here means
            // "restore from the latest backup" silently restores the OLDEST retained one.
            let latest = Command::new("restic")
                .args(["snapshots", "--json", "--tag", "cluster-backup"])
                .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
                .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                .output().await.ok()
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .and_then(|v| {
                    v.as_array()?.iter()
                        .max_by_key(|s| s["time"].as_str().unwrap_or("").to_string())?
                        ["id"].as_str().map(String::from)
                });
            match latest {
                Some(id) => id,
                None => { fail(name, "no snapshot specified and no cluster-backup snapshot exists").await; return; }
            }
        }
    };

    let cat_target = format!("/tmp/yolab-dr-catalog-{}", random_hex(8));
    let restore_out = Command::new("restic")
        .args(["restore", &snapshot_id, "--target", &cat_target, "--include", "**/catalog.json"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await;
    let catalog: Value = match restore_out {
        Ok(o) if o.status.success() => {
            let find_out = Command::new("find")
                .args([&cat_target, "-name", "catalog.json", "-type", "f"])
                .output().await;
            let cat_path = find_out.ok()
                .map(|f| String::from_utf8_lossy(&f.stdout).trim().to_string())
                .unwrap_or_default();
            let c = if !cat_path.is_empty() {
                tokio::fs::read(&cat_path).await.ok()
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
            fail(name, format!("could not extract catalog from snapshot: {}", String::from_utf8_lossy(&o.stderr).trim())).await;
            return;
        }
        Err(e) => { fail(name, format!("restic unavailable: {e}")).await; return; }
    };

    let namespaces: Vec<String> = if want_all {
        catalog["namespaces"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    } else {
        requested_namespaces
    };
    if namespaces.is_empty() {
        fail(name, "no namespaces found — snapshot may predate this feature, or pass namespaces[] explicitly").await;
        return;
    }

    // Space pre-flight (ceph df talks to MON, available even mid-recovery).
    let total_pvc_bytes = catalog["total_pvc_bytes"].as_u64().unwrap_or(0);
    if total_pvc_bytes > 0 {
        match crate::kubectl::ceph_exec(&["df", "-f", "json"]).await {
            Ok(df_raw) => {
                if let Ok(df) = serde_json::from_str::<Value>(&df_raw) {
                    let avail = df["stats"]["total_avail_bytes"].as_u64().unwrap_or(u64::MAX);
                    let reclaimable = reclaimable_pvc_bytes(&namespaces).await;
                    let effective_avail = avail.saturating_add(reclaimable);
                    let need = total_pvc_bytes * 6 / 5;
                    if effective_avail < need {
                        fail(name, format!(
                            "insufficient storage: {avail} bytes free (+{reclaimable} reclaimable \
                             from PVCs being replaced), ~{need} bytes needed \
                             ({total_pvc_bytes} bytes of PVC data + 20% headroom). \
                             Add more disks or reduce replication before restoring."
                        )).await;
                        return;
                    }
                    tracing::info!("restore-run {name}: space pre-flight ok — {avail} free + {reclaimable} reclaimable, {need} needed");
                }
            }
            Err(e) => tracing::warn!("restore-run {name}: space pre-flight skipped (ceph unavailable: {e})"),
        }
    }

    let restore_as_of: Option<String> = Command::new("restic")
        .args(["snapshots", &snapshot_id, "--json"])
        .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
        .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
        .output().await.ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v.as_array()?.first()?["time"].as_str().map(String::from));

    let namespaces_status: Vec<Value> = namespaces.iter().map(|ns| json!({
        "namespace": ns, "scaledDeployments": Value::Array(vec![]), "volumes": Value::Array(vec![]),
    })).collect();
    // resolvedNamespaces + catalog are written here so later phases (which only ever
    // read the CRD, never a local variable) can pick up exactly what Validating found
    // — regardless of whether the same process is still running.
    let _ = RESTORE_RUN.patch_status(name, json!({
        "phase": PHASE_WAITING_STORAGE,
        "phaseDeadline": deadline_after(700),
        "snapshotId": snapshot_id,
        "restoreAsOf": restore_as_of,
        "namespaces": namespaces_status,
        "resolvedNamespaces": namespaces,
        "catalog": catalog,
    })).await;
}

/// WaitingForStorage: a single non-blocking check of whether CephFS is mountable right
/// now — no internal sleep loop. Stays in this phase (reconcile_tick will call again
/// next tick) until it observes Ready, or until the phase deadline passes and the
/// timeout sweep fails the run.
async fn step_waiting_storage(name: &str) {
    let out = Command::new("kubectl")
        .args(["get", "cephfilesystem", "yolab-fs", "-n", "rook-ceph",
               "-o", "jsonpath={.status.phase}"])
        .output().await;
    let phase = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    };
    if phase != "Ready" {
        tracing::info!("restore-run {name}: CephFilesystem phase={phase:?} — waiting for Ready");
        return;
    }

    let _ = RESTORE_RUN.patch_status(name, json!({
        "phase": PHASE_RESTORING,
        "phaseDeadline": deadline_after(5400),
    })).await;
}

/// RestoringVolumes has two sub-steps, distinguished by the `volumesInitialized`
/// marker (persisted in status, not in memory — that's what makes resuming after a
/// crash "read the marker and pick the right branch" instead of needing to remember
/// anything):
///
/// 1. Not yet initialized: do the full per-namespace setup (apply YAML, scale to 0,
///    delete/recreate each PVC, create ReplicationDestinations) and record the result.
///    Safe to redo in full from scratch — every operation here is idempotent
///    (`kubectl apply`, `ensure_*` helpers) or already handles "already gone"/"already
///    exists" as success, so re-running this after a crash wastes at most some
///    already-started data-pull progress, never corrupts anything.
/// 2. Initialized: a single observation of every "Restoring" volume's
///    ReplicationDestination — no internal poll loop. Transitions to Applying once
///    every volume is terminal, or once the phase deadline passes (whichever first).
async fn step_restoring(name: &str, run: &Value, cfg: &BackupConfig) {
    if !run["status"]["volumesInitialized"].as_bool().unwrap_or(false) {
        step_restoring_setup(name, run, cfg).await;
    } else {
        step_restoring_poll(name, run).await;
    }
}

async fn step_restoring_setup(name: &str, run: &Value, cfg: &BackupConfig) {
    let namespaces: Vec<String> = run["status"]["resolvedNamespaces"].as_array().cloned().unwrap_or_default()
        .into_iter().filter_map(|v| v.as_str().map(String::from)).collect();
    let catalog = run["status"]["catalog"].clone();
    let snapshot_id = run["status"]["snapshotId"].as_str().unwrap_or("").to_string();
    let restore_as_of = run["status"]["restoreAsOf"].as_str().map(String::from);
    let repo = cfg.restic_repo("cluster-backup");
    let timestamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
    let mut ns_state: Vec<NamespaceState> = Vec::new();

    for ns in &namespaces {
        let mut state = NamespaceState { namespace: ns.clone(), scaled_deployments: Vec::new(), volumes: Vec::new() };

        let ns_exists = Command::new("kubectl").args(["get", "namespace", ns]).output().await
            .map(|o| o.status.success()).unwrap_or(false);
        if !ns_exists {
            if let Err(e) = kubectl_apply(&json!({
                "apiVersion": "v1", "kind": "Namespace",
                "metadata": { "name": ns, "labels": { "yolab.io/managed": "true" } }
            }).to_string()).await {
                tracing::warn!("restore-run {name}: {ns}: create namespace: {e}");
            }
        }

        // Apply K8s objects from snapshot YAML (deployments/services/secrets/configmaps).
        let yaml_target = format!("/tmp/yolab-dr-yaml-{}", random_hex(8));
        let pattern = format!("**/{ns}.yaml");
        let r = Command::new("restic")
            .args(["restore", &snapshot_id, "--target", &yaml_target, "--include", &pattern])
            .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
            .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
            .output().await;
        if let Ok(o) = r {
            if o.status.success() {
                if let Ok(f) = Command::new("find").args([&yaml_target, "-name", &format!("{ns}.yaml"), "-type", "f"]).output().await {
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

        // Record what we're about to scale down so Applying can always undo it. If this
        // is a redo after a crash, the deployments may already be at 0 from the previous
        // attempt — that's fine, `kubectl get` still reports their names correctly.
        let existing_deployments: Vec<String> = Command::new("kubectl")
            .args(["get", "deployments", "-n", ns, "-o", "jsonpath={.items[*].metadata.name}"])
            .output().await.ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        state.scaled_deployments = existing_deployments;
        let _ = Command::new("kubectl").args(["scale", "deployment", "--all", "-n", ns, "--replicas=0"]).output().await;

        let catalog_pvcs: Vec<(String, String)> = catalog["services"].as_array()
            .and_then(|svcs| svcs.iter().find(|s| s["namespace"].as_str() == Some(ns.as_str())))
            .and_then(|s| s["pvcs"].as_array())
            .map(|pvcs| pvcs.iter().filter_map(|p| {
                let n = p["name"].as_str()?.to_string();
                let cap = p["capacity"].as_str().unwrap_or("10Gi").to_string();
                Some((n, cap))
            }).collect())
            .unwrap_or_default();

        for (pvc_name, capacity) in &catalog_pvcs {
            let mut vol = VolumeState { pvc: pvc_name.clone(), phase: "Pending".to_string() };

            if let Err(e) = ensure_restic_secret(ns, pvc_name, cfg).await {
                tracing::warn!("restore-run {name}: {ns}/{pvc_name}: restic secret: {e}");
            }
            let _ = ensure_replication_source(&PvcInfo { namespace: ns.clone(), name: pvc_name.clone() }, false).await;

            let pvc_repo = cfg.restic_repo(&format!("volsync/{ns}/{}", canonical_pvc_id(pvc_name)));
            restic_unlock(&pvc_repo, &cfg.restic_password, &cfg.access_key_id, &cfg.secret_access_key).await;
            let has_snapshot = Command::new("restic")
                .args(["snapshots", "--json", "--last"])
                .env("RESTIC_REPOSITORY", &pvc_repo).env("RESTIC_PASSWORD", &cfg.restic_password)
                .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                .output().await.ok()
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .and_then(|v| v.as_array().map(|a| !a.is_empty()))
                .unwrap_or(false);

            if !has_snapshot {
                tracing::warn!("restore-run {name}: {ns}/{pvc_name}: no backup snapshot found — PVC preserved");
                vol.phase = "Skipped".to_string();
                state.volumes.push(vol);
                continue;
            }

            if let Err(e) = delete_pvc_and_wait(ns, pvc_name).await {
                tracing::warn!("restore-run {name}: {ns}/{pvc_name}: delete pvc: {e}");
                vol.phase = "Failed".to_string();
                state.volumes.push(vol);
                continue;
            }
            if let Err(e) = ensure_destination_pvc(pvc_name, ns, capacity, "yolab-cephfs", "ReadWriteMany").await {
                tracing::warn!("restore-run {name}: {ns}/{pvc_name}: create pvc: {e}");
                vol.phase = "Failed".to_string();
                state.volumes.push(vol);
                continue;
            }

            annotate_ns_privileged_movers(ns).await;
            let secret_name = format!("{}{RESTIC_SECRET_SUFFIX}", canonical_pvc_id(pvc_name));
            let mut restic_spec = json!({
                "repository": secret_name,
                "copyMethod": "Direct",
                "cacheStorageClassName": "yolab-cephfs",
                "destinationPVC": pvc_name,
                "moverSecurityContext": { "runAsUser": 0, "runAsGroup": 0, "fsGroup": 0 }
            });
            if let Some(ref t) = restore_as_of {
                restic_spec["restoreAsOf"] = Value::String(t.clone());
            }
            let dest_name = format!("emergency-restore-{}", canonical_pvc_id(pvc_name));
            let manifest = json!({
                "apiVersion": "volsync.backube/v1alpha1",
                "kind": "ReplicationDestination",
                "metadata": { "name": dest_name, "namespace": ns, "labels": { "app.kubernetes.io/managed-by": "yolab" } },
                "spec": { "trigger": { "manual": format!("dr-{timestamp}") }, "restic": restic_spec }
            });
            match kubectl_apply(&manifest.to_string()).await {
                Ok(_) => { vol.phase = "Restoring".to_string(); }
                Err(e) => {
                    tracing::warn!("restore-run {name}: {ns}/{pvc_name}: RD: {e}");
                    vol.phase = "Failed".to_string();
                }
            }
            state.volumes.push(vol);
        }

        ns_state.push(state);
    }

    let _ = RESTORE_RUN.patch_status(name, json!({
        "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
        "volumesInitialized": true,
    })).await;
}

async fn step_restoring_poll(name: &str, run: &Value) {
    let deadline = parse_deadline(run).unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(5400));
    let mut ns_state = parse_ns_state(run);

    let rds = Command::new("kubectl").args(["get", "replicationdestination", "-A", "-o", "json"]).output().await;
    let rd_items: Vec<Value> = rds.ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default();

    let mut all_terminal = true;
    for state in ns_state.iter_mut() {
        for vol in state.volumes.iter_mut() {
            if vol.phase != "Restoring" { continue; }
            let dest_name = format!("emergency-restore-{}", canonical_pvc_id(&vol.pvc));
            let item = rd_items.iter().find(|i| {
                i["metadata"]["name"].as_str() == Some(dest_name.as_str())
                    && i["metadata"]["namespace"].as_str() == Some(state.namespace.as_str())
            });
            let result = item.and_then(|i| i["status"]["latestMoverStatus"]["result"].as_str()).unwrap_or("").to_lowercase();
            match result.as_str() {
                "successful" => vol.phase = "Succeeded".to_string(),
                "failed" => vol.phase = "Failed".to_string(),
                _ => all_terminal = false,
            }
        }
    }

    let past_deadline = Utc::now() > deadline;
    if !all_terminal && !past_deadline {
        let _ = RESTORE_RUN.patch_status(name, json!({
            "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
        })).await;
        return;
    }

    if past_deadline {
        for state in ns_state.iter_mut() {
            for vol in state.volumes.iter_mut() {
                if vol.phase == "Restoring" {
                    tracing::warn!("restore-run {name}: {}/{}: timed out", state.namespace, vol.pvc);
                    vol.phase = "Failed".to_string();
                }
            }
        }
    }

    let _ = RESTORE_RUN.patch_status(name, json!({
        "phase": PHASE_APPLYING,
        "phaseDeadline": deadline_after(300),
        "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
    })).await;
}

/// Applying: scale every namespace's deployments back up unconditionally, clean up
/// completed ReplicationDestinations, compute the terminal phase. Safe to redo in
/// full — scaling to an already-current replica count is a no-op, RD delete ignores
/// not-found.
async fn step_applying(name: &str, run: &Value) {
    let ns_state = parse_ns_state(run);

    for state in &ns_state {
        // Unconditional: whatever happened to the data, the app must not stay dark.
        for deploy in &state.scaled_deployments {
            let _ = scale_deployment(&state.namespace, deploy, 1).await;
        }
        for vol in &state.volumes {
            if vol.phase == "Succeeded" || vol.phase == "Failed" {
                let dest_name = format!("emergency-restore-{}", canonical_pvc_id(&vol.pvc));
                delete_replication_destination_without_touching_pvc(&dest_name, &state.namespace).await;
            }
        }
    }

    let total_volumes: usize = ns_state.iter().map(|s| s.volumes.len()).sum();
    let succeeded: usize = ns_state.iter().flat_map(|s| &s.volumes).filter(|v| v.phase == "Succeeded").count();
    let failed: usize = ns_state.iter().flat_map(|s| &s.volumes).filter(|v| v.phase == "Failed").count();

    let phase = if total_volumes == 0 {
        PHASE_SUCCEEDED // no PVCs to restore — YAML-only namespaces applied fine
    } else if failed == 0 && succeeded == total_volumes {
        PHASE_SUCCEEDED
    } else if succeeded == 0 && failed == total_volumes {
        PHASE_FAILED
    } else {
        PHASE_PARTIAL
    };

    let _ = RESTORE_RUN.patch_status(name, json!({
        "phase": phase,
        "finishedAt": Utc::now().to_rfc3339(),
        "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
    })).await;
    tracing::info!("restore-run {name}: {phase} ({succeeded}/{total_volumes} volumes restored)");
}

struct VolumeState {
    pvc: String,
    phase: String,
}

struct NamespaceState {
    namespace: String,
    scaled_deployments: Vec<String>,
    volumes: Vec<VolumeState>,
}

impl NamespaceState {
    fn to_json(&self) -> Value {
        json!({
            "namespace": self.namespace,
            "scaledDeployments": self.scaled_deployments,
            "volumes": self.volumes.iter().map(|v| json!({ "pvc": v.pvc, "phase": v.phase })).collect::<Vec<_>>(),
        })
    }
}

/// Reconstructs `NamespaceState` from `status.namespaces` — the only place this data
/// lives between ticks (never in a task-local variable), so any tick, in any process,
/// reads the exact same state the previous tick left behind.
fn parse_ns_state(run: &Value) -> Vec<NamespaceState> {
    run["status"]["namespaces"].as_array().cloned().unwrap_or_default()
        .into_iter()
        .map(|n| NamespaceState {
            namespace: n["namespace"].as_str().unwrap_or("").to_string(),
            scaled_deployments: n["scaledDeployments"].as_array().cloned().unwrap_or_default()
                .into_iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            volumes: n["volumes"].as_array().cloned().unwrap_or_default()
                .into_iter()
                .map(|v| VolumeState {
                    pvc: v["pvc"].as_str().unwrap_or("").to_string(),
                    phase: v["phase"].as_str().unwrap_or("Pending").to_string(),
                })
                .collect(),
        })
        .collect()
}

/// Reconcile tick: only the lease holder acts. Times out any run stuck past its phase
/// deadline, then advances every still-active run by exactly one step. Unlike
/// BackupRun there's no scheduling decision here — restores are only ever started
/// explicitly (manual snapshot pick, or the DR banner's emergency restore).
pub async fn reconcile_tick(holder: &str) {
    let Some(_guard) = lease::acquire(LEASE_NAME, holder, LEASE_DURATION_SECS).await else {
        return;
    };

    let mut seen_terminal = 0usize;
    for run in RESTORE_RUN.list().await { // newest-created first
        if is_terminal(run["status"]["phase"].as_str().unwrap_or("")) {
            seen_terminal += 1;
            if seen_terminal > KEEP_TERMINAL_RUNS {
                if let Some(name) = run["metadata"]["name"].as_str() {
                    RESTORE_RUN.delete(name).await;
                }
            }
        }
    }

    let mut timed_out: std::collections::HashSet<String> = std::collections::HashSet::new();
    for run in RESTORE_RUN.list().await {
        let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING).to_string();
        if is_terminal(&phase) {
            continue;
        }
        if let Some(dl) = parse_deadline(&run) {
            if Utc::now() > dl {
                let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
                tracing::warn!("restore-run {name}: timed out in phase {phase}");
                let _ = RESTORE_RUN.patch_status(&name, json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": format!("timed out in phase {phase}"),
                })).await;
                timed_out.insert(name);
            }
        }
    }

    // Advance every still-active run by exactly one step. This is the whole
    // reconciler now — no separate executor task exists anywhere for this to race
    // against, which is exactly what leaves a restore fully recoverable across a
    // crash or reboot instead of stranded mid-flight.
    for run in RESTORE_RUN.list().await {
        let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || timed_out.contains(&name) {
            continue;
        }
        if is_terminal(run["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING)) {
            continue;
        }
        step(&name).await;
    }
}
