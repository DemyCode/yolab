// RestoreRun reconciler — replaces DrRestoreGuard + dr_start/dr_status/dr_apply +
// reconcile_restores + run_restore_reconciler + apply_one_restore.
//
// Phases: Validating → WaitingForStorage → RestoringVolumes → Applying →
// Succeeded|Partial|Failed. Same rationale as BackupRun (see backup_run.rs's module
// doc) — the previous design's ConfigMap-lock + "does any ReplicationDestination
// exist" checks meant a stuck restore blocked every future backup/restore forever,
// and every failure branch in the per-PVC loop was a bare `continue` after the
// namespace had already been scaled to 0 — so a single bad PVC (no backup snapshot,
// a delete timeout, ...) could leave a whole app dark with no path back up. Here,
// every non-terminal phase has a deadline, and the Applying phase unconditionally
// scales every namespace's deployments back up — succeeded, failed, or skipped —
// so "restore didn't work" degrades to "app running with old/empty data", never to
// "app permanently stopped".

use crate::kubectl::Crd;
use crate::lease;
use crate::routers::backup_common::*;
use chrono::Utc;
use serde_json::{json, Value};
use std::time::Duration;
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

pub async fn is_active() -> bool {
    RESTORE_RUN.list().await.iter().any(|r| {
        let phase = r["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING);
        !is_terminal(phase)
    })
}

/// GET /api/backups/dr/status consumes this — the active run's full status (phase,
/// per-namespace/per-volume progress), or the most recently finished one.
pub async fn current_status() -> Value {
    let runs = RESTORE_RUN.list().await; // newest-created first
    let active = runs.iter().find(|r| !is_terminal(r["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING))).cloned();
    let last_finished = runs.iter().find(|r| is_terminal(r["status"]["phase"].as_str().unwrap_or(""))).cloned();
    json!({ "active": active, "last": last_finished })
}

/// Creates a RestoreRun and spawns its execution. `snapshot_id` of `None` resolves to
/// the latest cluster-backup snapshot once execution reaches Validating.
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
    tokio::spawn(execute(name.clone()));
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

async fn execute(name: String) {
    let Some(cfg) = read_master_config().await else {
        fail(&name, "backup not configured").await;
        return;
    };
    let run = match RESTORE_RUN.get(&name).await {
        Some(r) => r,
        None => return,
    };
    let requested_snapshot = run["spec"]["snapshotId"].as_str().map(String::from);
    let want_all = run["spec"]["all"].as_bool().unwrap_or(false);
    let requested_namespaces: Vec<String> = run["spec"]["namespaces"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    // ── Validating ────────────────────────────────────────────────────────────
    let snapshot_id = match requested_snapshot {
        Some(id) => id,
        None => {
            let latest = Command::new("restic")
                .args(["snapshots", "--json", "--tag", "cluster-backup", "--last"])
                .env("RESTIC_REPOSITORY", &repo).env("RESTIC_PASSWORD", &cfg.restic_password)
                .env("AWS_ACCESS_KEY_ID", &cfg.access_key_id).env("AWS_SECRET_ACCESS_KEY", &cfg.secret_access_key)
                .output().await.ok()
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .and_then(|v| v.as_array()?.first()?["id"].as_str().map(String::from));
            match latest {
                Some(id) => id,
                None => { fail(&name, "no snapshot specified and no cluster-backup snapshot exists").await; return; }
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
            fail(&name, format!("could not extract catalog from snapshot: {}", String::from_utf8_lossy(&o.stderr).trim())).await;
            return;
        }
        Err(e) => { fail(&name, format!("restic unavailable: {e}")).await; return; }
    };

    let namespaces: Vec<String> = if want_all {
        catalog["namespaces"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    } else {
        requested_namespaces
    };
    if namespaces.is_empty() {
        fail(&name, "no namespaces found — snapshot may predate this feature, or pass namespaces[] explicitly").await;
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
                        fail(&name, format!(
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
    let _ = RESTORE_RUN.patch_status(&name, json!({
        "phase": PHASE_WAITING_STORAGE,
        "phaseDeadline": deadline_after(700),
        "snapshotId": snapshot_id,
        "restoreAsOf": restore_as_of,
        "namespaces": namespaces_status,
    })).await;

    // ── WaitingForStorage ─────────────────────────────────────────────────────
    if let Err(e) = wait_for_cephfs_ready(600).await {
        fail(&name, e).await;
        return;
    }

    // ── RestoringVolumes ──────────────────────────────────────────────────────
    let restoring_deadline = Utc::now() + chrono::Duration::seconds(5400);
    let _ = RESTORE_RUN.patch_status(&name, json!({
        "phase": PHASE_RESTORING,
        "phaseDeadline": restoring_deadline.to_rfc3339(),
    })).await;

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
                            } else {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                        }
                    }
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(&yaml_target).await;

        // Record what we're about to scale down so Applying can always undo it.
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

            if let Err(e) = ensure_restic_secret(ns, pvc_name, &cfg).await {
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

    let _ = RESTORE_RUN.patch_status(&name, json!({ "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>() })).await;

    // Poll RDs until every non-Skipped/non-Failed volume reaches a terminal state,
    // or the phase deadline passes.
    loop {
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
        let _ = RESTORE_RUN.patch_status(&name, json!({ "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>() })).await;

        if all_terminal || Utc::now() > restoring_deadline {
            for state in ns_state.iter_mut() {
                for vol in state.volumes.iter_mut() {
                    if vol.phase == "Restoring" {
                        tracing::warn!("restore-run {name}: {}/{}: timed out", state.namespace, vol.pvc);
                        vol.phase = "Failed".to_string();
                    }
                }
            }
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    // ── Applying ──────────────────────────────────────────────────────────────
    let _ = RESTORE_RUN.patch_status(&name, json!({
        "phase": PHASE_APPLYING,
        "phaseDeadline": deadline_after(300),
        "namespaces": ns_state.iter().map(NamespaceState::to_json).collect::<Vec<_>>(),
    })).await;

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

    let _ = RESTORE_RUN.patch_status(&name, json!({
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

/// Reconcile tick: only the lease holder acts. Times out any run stuck past its phase
/// deadline. Unlike BackupRun there's no scheduling decision here — restores are only
/// ever started explicitly (manual snapshot pick, or the DR banner's emergency restore).
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

    for run in RESTORE_RUN.list().await {
        let phase = run["status"]["phase"].as_str().unwrap_or(PHASE_VALIDATING).to_string();
        if is_terminal(&phase) {
            continue;
        }
        let deadline = run["status"]["phaseDeadline"].as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        if let Some(dl) = deadline {
            if Utc::now() > dl {
                let name = run["metadata"]["name"].as_str().unwrap_or("").to_string();
                tracing::warn!("restore-run {name}: timed out in phase {phase} — the executing task should have already handled this; forcing terminal");
                let _ = RESTORE_RUN.patch_status(&name, json!({
                    "phase": PHASE_FAILED,
                    "finishedAt": Utc::now().to_rfc3339(),
                    "error": format!("timed out in phase {phase} (executor task may have died)"),
                })).await;
            }
        }
    }
}
