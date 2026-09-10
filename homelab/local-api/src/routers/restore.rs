//! Per-app restore — the replacement for the whole-cluster RestoreRun.
//!
//! Restoring one application is: scale its deployments to zero, recreate its PVCs
//! from their VolSync restic repos, re-apply the app's backed-up K8s objects, and
//! scale back up. The app's data and config are both taken from a chosen
//! `cluster-backup` snapshot (the id the history picker selects).
//!
//! A restore is recorded in a ConfigMap with the same three-state model as a
//! backup: *running* (this process is driving it), *succeeded*, or *failed*
//! (including "was running when the process died"). A watchdog guarantees that a
//! crashed restore never leaves the app at zero replicas: it scales any "running
//! but no longer in-flight" app back to its recorded replica counts.

use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routers::backup_common::*;
use chrono::Utc;
use tokio::process::Command;

const RESTORES_CONFIGMAP: &str = "yolab-restores";
const RESTORES_NS: &str = "kube-system";
const MAX_RESTORES: usize = 50;

/// How long to wait for a PVC to actually disappear before failing the volume.
const PVC_DELETE_TIMEOUT_SECS: u64 = 180;
/// How long to wait for a VolSync ReplicationDestination to finish pulling a volume.
const RD_TIMEOUT_SECS: u64 = 3600;
const WATCHDOG_TICK_SECS: u64 = 30;

/// Ids this process is actively restoring. Lost when the process dies — which is
/// exactly what lets the watchdog distinguish "running" from "crashed".
static RESTORE_IN_FLIGHT: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Clone, Serialize, Deserialize)]
struct DeploymentScale {
    name: String,
    replicas: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct RestoreSet {
    id: String,
    namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<String>,
    started_at: String,
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scaled_deployments: Vec<DeploymentScale>,
}

// ── ConfigMap records ──────────────────────────────────────────────────────────

fn parse_sets(raw: &str) -> Vec<RestoreSet> {
    serde_json::from_str::<Vec<RestoreSet>>(raw).unwrap_or_default()
}

fn upsert(sets: &mut Vec<RestoreSet>, set: RestoreSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_RESTORES);
}

async fn read_sets() -> Vec<RestoreSet> {
    let Ok(v) = crate::kubectl::get_json(&[
        "get",
        "configmap",
        RESTORES_CONFIGMAP,
        "-n",
        RESTORES_NS,
        "-o",
        "json",
    ])
    .await
    else {
        return Vec::new();
    };
    parse_sets(v["data"]["sets"].as_str().unwrap_or("[]"))
}

async fn write_sets(sets: &[RestoreSet]) {
    let raw = serde_json::to_string(sets).unwrap_or_else(|_| "[]".into());
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": RESTORES_CONFIGMAP,
            "namespace": RESTORES_NS,
            "labels": { "app.kubernetes.io/managed-by": "yolab" },
        },
        "data": { "sets": raw },
    });
    let _ = crate::kubectl::apply(&manifest.to_string()).await;
}

async fn patch_set(id: &str, update: impl FnOnce(&mut RestoreSet)) {
    let mut sets = read_sets().await;
    if let Some(s) = sets.iter_mut().find(|s| s.id == id) {
        update(s);
        write_sets(&sets).await;
    }
}

// ── The operation ──────────────────────────────────────────────────────────────

/// Starts restoring one app and returns immediately. The work runs detached; the
/// watchdog catches a crash and scales the app back up.
pub(crate) async fn start(
    namespace: &str,
    snapshot_id: Option<String>,
) -> anyhow::Result<String> {
    let Some(cfg) = read_master_config().await else {
        anyhow::bail!("backup not configured");
    };

    // Resolve the snapshot up front so the record (and the page) always shows the
    // concrete id being restored, even for "restore latest".
    let resolved = resolve_snapshot(&cfg, snapshot_id).await?;
    let Some(snapshot_id) = resolved else {
        anyhow::bail!("no cluster-backup snapshot to restore from");
    };

    // Record the live replica counts BEFORE scaling down, so a crash mid-restore can
    // still bring the app back to a running state.
    let scaled_deployments = read_deployment_scales(namespace).await;

    let id = format!("rs-{}", random_hex(8));
    let set = RestoreSet {
        id: id.clone(),
        namespace: namespace.to_string(),
        snapshot_id: Some(snapshot_id.clone()),
        started_at: Utc::now().to_rfc3339(),
        state: "running".to_string(),
        finished_at: None,
        error: None,
        scaled_deployments,
    };
    let mut sets = read_sets().await;
    upsert(&mut sets, set);
    write_sets(&sets).await;

    RESTORE_IN_FLIGHT.lock().unwrap().push(id.clone());

    let task_id = id.clone();
    let ns = namespace.to_string();
    tokio::spawn(async move {
        let result = run_restore(&ns, &snapshot_id, &cfg).await;
        record_done(&task_id, &result).await;
        {
            let mut guard = RESTORE_IN_FLIGHT.lock().unwrap();
            guard.retain(|s| s != &task_id);
        }
    });

    Ok(id)
}

async fn record_done(id: &str, result: &anyhow::Result<()>) {
    let finished_at = Utc::now().to_rfc3339();
    match result {
        Ok(()) => {
            patch_set(id, |s| {
                s.state = "succeeded".to_string();
                s.finished_at = Some(finished_at);
            })
            .await
        }
        Err(e) => {
            patch_set(id, |s| {
                s.state = "failed".to_string();
                s.finished_at = Some(finished_at);
                s.error = Some(e.to_string());
            })
            .await
        }
    }
}

/// The actual restore. Everything here is safe to redo and bounded by per-volume
/// timeouts; the watchdog is what turns an interrupted run into "app back up".
async fn run_restore(namespace: &str, snapshot_id: &str, cfg: &BackupConfig) -> anyhow::Result<()> {
    // 1. Scale the app down so its pods release the PVCs being replaced.
    let _ = crate::kubectl::run(&[
        "scale",
        "deployment",
        "--all",
        "-n",
        namespace,
        "--replicas=0",
    ])
    .await;

    // 2. Pull the app's config + catalog from the chosen snapshot.
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let catalog = extract_json_file(&repo, cfg, snapshot_id, "catalog.json").await?;
    let ns_yaml = extract_file(&repo, cfg, snapshot_id, &format!("**/{namespace}.yaml")).await?;
    let restore_as_of = snapshot_time(&repo, cfg, snapshot_id).await;

    // 3. Recreate each PVC from its own VolSync restic repo.
    for (pvc, capacity) in catalog_pvcs(&catalog, namespace) {
        restore_volume(namespace, &pvc, &capacity, cfg, restore_as_of.as_deref()).await?;
    }

    // 4. Re-apply the app's backed-up objects (deploy/secret/configmap/etc.), which
    //    restores its config and brings it back up at its recorded replica counts.
    if let Some(path) = ns_yaml {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            if let Err(e) = kubectl_apply(&String::from_utf8_lossy(&bytes)).await {
                anyhow::bail!("apply {namespace}.yaml: {e}");
            }
        }
    }

    tracing::info!("restore: {namespace} restored from {snapshot_id}");
    Ok(())
}

async fn restore_volume(
    namespace: &str,
    pvc: &str,
    capacity: &str,
    cfg: &BackupConfig,
    restore_as_of: Option<&str>,
) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(pvc);
    let pvc_repo = cfg.restic_repo(&format!("volsync/{namespace}/{cid}"));
    restic_unlock(
        &pvc_repo,
        &cfg.restic_password,
        &cfg.access_key_id,
        &cfg.secret_access_key,
    )
    .await;

    // No backup for this PVC — leave it untouched rather than destroy it for nothing.
    if !snapshots_exist(&pvc_repo, cfg).await? {
        tracing::warn!("restore: {namespace}/{pvc}: no backup snapshot — keeping as-is");
        return Ok(());
    }

    // Delete the live PVC and wait for it to actually go away.
    let _ = crate::kubectl::run(&[
        "delete",
        "pvc",
        pvc,
        "-n",
        namespace,
        "--wait=false",
        "--ignore-not-found",
    ])
    .await;
    wait_for_pvc_deleted(namespace, pvc).await?;

    annotate_ns_privileged_movers(namespace).await;
    ensure_destination_pvc(pvc, namespace, capacity, "yolab-cephfs", "ReadWriteMany").await?;

    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");
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
    let dest_name = format!("emergency-restore-{cid}");
    let manifest = json!({
        "apiVersion": "volsync.backube/v1alpha1",
        "kind": "ReplicationDestination",
        "metadata": {
            "name": dest_name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "yolab" }
        },
        "spec": {
            "trigger": { "manual": format!("dr-{}", Utc::now().format("%Y%m%d%H%M%S")) },
            "restic": restic_spec
        }
    });
    kubectl_apply(&manifest.to_string()).await?;

    wait_for_rd(namespace, &dest_name).await?;
    delete_replication_destination_without_touching_pvc(&dest_name, namespace).await;
    Ok(())
}

async fn wait_for_pvc_deleted(namespace: &str, pvc: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(PVC_DELETE_TIMEOUT_SECS);
    while std::time::Instant::now() < deadline {
        let exists = crate::kubectl::run(&["get", "pvc", pvc, "-n", namespace])
            .await
            .is_ok();
        if !exists {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    anyhow::bail!("PVC still present after {PVC_DELETE_TIMEOUT_SECS}s — a pod may still be mounting it")
}

async fn wait_for_rd(namespace: &str, dest_name: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(RD_TIMEOUT_SECS);
    loop {
        let v = crate::kubectl::get_json(&[
            "get",
            "replicationdestination",
            dest_name,
            "-n",
            namespace,
            "-o",
            "json",
        ])
        .await
        .ok();
        let result = v.as_ref().and_then(|v| {
            v["status"]["latestMoverStatus"]["result"]
                .as_str()
                .map(String::from)
        });
        match result.as_deref() {
            Some("Successful") => return Ok(()),
            Some("Failed") => anyhow::bail!("ReplicationDestination reported failure"),
            _ => {
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("restore timed out after {}s", RD_TIMEOUT_SECS);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

// ── Cluster observation helpers ────────────────────────────────────────────────

async fn read_deployment_scales(ns: &str) -> Vec<DeploymentScale> {
    crate::kubectl::get_json(&["get", "deployments", "-n", ns, "-o", "json"])
        .await
        .ok()
        .and_then(|v| v["items"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            Some(DeploymentScale {
                name: d["metadata"]["name"].as_str()?.to_string(),
                replicas: d["spec"]["replicas"].as_u64().unwrap_or(1) as u32,
            })
        })
        .collect()
}

async fn snapshots_exist(repo: &str, cfg: &BackupConfig) -> anyhow::Result<bool> {
    let out = restic(repo, cfg, &["snapshots", "--json"]).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(false);
        }
        anyhow::bail!("{}", stderr.trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(v.as_array().map(|a| !a.is_empty()).unwrap_or(false))
}

/// Resolves the snapshot id to restore from: the caller's explicit choice, or the
/// newest `cluster-backup` snapshot.
async fn resolve_snapshot(
    cfg: &BackupConfig,
    requested: Option<String>,
) -> anyhow::Result<Option<String>> {
    if requested.is_some() {
        return Ok(requested);
    }
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let out = restic(&repo, cfg, &["snapshots", "--json", "--tag", "cluster-backup"]).await?;
    if !out.status.success() {
        anyhow::bail!(
            "could not list snapshots: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(newest_snapshot_id(&v))
}

fn newest_snapshot_id(snapshots: &Value) -> Option<String> {
    snapshots
        .as_array()?
        .iter()
        .max_by_key(|s| s["time"].as_str().unwrap_or("").to_string())?["id"]
        .as_str()
        .map(String::from)
}

async fn snapshot_time(repo: &str, cfg: &BackupConfig, id: &str) -> Option<String> {
    let out = restic(repo, cfg, &["snapshots", id, "--json"]).await.ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?.first()?["time"].as_str().map(String::from)
}

/// Extracts a single named file from a snapshot and returns its path.
async fn extract_file(
    repo: &str,
    cfg: &BackupConfig,
    snapshot_id: &str,
    pattern: &str,
) -> anyhow::Result<Option<String>> {
    let target = format!("/tmp/yolab-restore-{}", random_hex(8));
    let out = restic(
        repo,
        cfg,
        &["restore", snapshot_id, "--target", &target, "--include", pattern],
    )
    .await?;
    if !out.status.success() {
        let _ = tokio::fs::remove_dir_all(&target).await;
        anyhow::bail!(
            "restic restore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let find = Command::new("find")
        .args([&target, "-type", "f"])
        .output()
        .await;
    Ok(find.ok().and_then(|f| {
        let p = String::from_utf8_lossy(&f.stdout).trim().to_string();
        if p.is_empty() {
            None
        } else {
            Some(p)
        }
    }))
}

/// Extracts catalog.json (or any single JSON file) and parses it.
async fn extract_json_file(
    repo: &str,
    cfg: &BackupConfig,
    snapshot_id: &str,
    filename: &str,
) -> anyhow::Result<Value> {
    match extract_file(repo, cfg, snapshot_id, &format!("**/{filename}")).await? {
        Some(path) => {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| anyhow::anyhow!("read {filename}: {e}"))?;
            serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("parse {filename}: {e}"))
        }
        None => Ok(json!({"namespaces": []})),
    }
}

fn catalog_pvcs(catalog: &Value, namespace: &str) -> Vec<(String, String)> {
    catalog["services"]
        .as_array()
        .and_then(|svcs| svcs.iter().find(|s| s["namespace"].as_str() == Some(namespace)))
        .and_then(|s| s["pvcs"].as_array())
        .map(|pvcs| {
            pvcs.iter()
                .filter_map(|p| {
                    Some((
                        p["name"].as_str()?.to_string(),
                        p["capacity"].as_str().unwrap_or("10Gi").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Read side ──────────────────────────────────────────────────────────────────

fn in_flight_ids() -> Vec<String> {
    RESTORE_IN_FLIGHT.lock().unwrap().clone()
}

/// Every recorded restore, newest first, classified into running/succeeded/failed.
pub(crate) async fn list() -> Vec<Value> {
    let sets = read_sets().await;
    let in_flight: std::collections::HashSet<String> = in_flight_ids().into_iter().collect();
    sets.iter()
        .map(|s| {
            let state = classify(s, in_flight.contains(&s.id));
            json!({
                "id": s.id,
                "namespace": s.namespace,
                "snapshot_id": s.snapshot_id,
                "started_at": s.started_at,
                "finished_at": s.finished_at,
                "error": s.error,
                "state": state,
            })
        })
        .collect()
}

pub(crate) async fn is_running() -> bool {
    let sets = read_sets().await;
    let in_flight: std::collections::HashSet<String> = in_flight_ids().into_iter().collect();
    sets.iter()
        .any(|s| s.state == "running" && in_flight.contains(&s.id))
}

fn classify(s: &RestoreSet, in_flight: bool) -> &'static str {
    match s.state.as_str() {
        "succeeded" => "succeeded",
        "failed" => "failed",
        _ => {
            if in_flight {
                "running"
            } else {
                "failed"
            }
        }
    }
}

/// Background loop: any restore that is recorded "running" but no longer in flight
/// means the driving process died — scale that app back up so it never stays dark.
pub(crate) async fn run_watchdog() {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let in_flight: std::collections::HashSet<String> = in_flight_ids().into_iter().collect();
        let crashed: Vec<RestoreSet> = read_sets()
            .await
            .into_iter()
            .filter(|s| s.state == "running" && !in_flight.contains(&s.id))
            .collect();
        for set in crashed {
            tracing::warn!(
                "restore {} ({}) crashed — scaling back up",
                set.id,
                set.namespace
            );
            for d in &set.scaled_deployments {
                let _ = scale_deployment(&set.namespace, &d.name, d.replicas).await;
            }
            patch_set(&set.id, |s| {
                s.state = "failed".to_string();
                s.finished_at = Some(Utc::now().to_rfc3339());
                s.error = Some("interrupted — scaled back up".to_string());
            })
            .await;
        }
        tokio::time::sleep(Duration::from_secs(WATCHDOG_TICK_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: &str, state: &str) -> RestoreSet {
        RestoreSet {
            id: id.into(),
            namespace: "yolab-gitea".into(),
            snapshot_id: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            state: state.into(),
            finished_at: if state == "running" {
                None
            } else {
                Some("2026-01-01T00:05:00Z".into())
            },
            error: None,
            scaled_deployments: vec![],
        }
    }

    #[test]
    fn succeeded_is_succeeded() {
        assert_eq!(classify(&set("a", "succeeded"), false), "succeeded");
    }

    #[test]
    fn failed_is_failed() {
        assert_eq!(classify(&set("a", "failed"), false), "failed");
    }

    #[test]
    fn running_is_running_only_while_in_flight() {
        assert_eq!(classify(&set("a", "running"), true), "running");
        assert_eq!(classify(&set("a", "running"), false), "failed");
    }

    #[test]
    fn upsert_replaces_by_id_and_keeps_newest_first() {
        let mut sets = vec![set("a", "running"), set("b", "succeeded")];
        upsert(&mut sets, set("a", "succeeded"));
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].id, "a");
        assert_eq!(sets[0].state, "succeeded");
    }

    #[test]
    fn parse_sets_ignores_garbage() {
        assert!(parse_sets("nope").is_empty());
        assert!(parse_sets("{}").is_empty());
    }

    #[test]
    fn newest_snapshot_id_picks_latest() {
        let v = json!([
            {"id": "old", "time": "2026-01-01T00:00:00Z"},
            {"id": "new", "time": "2026-01-02T00:00:00Z"},
        ]);
        assert_eq!(newest_snapshot_id(&v).as_deref(), Some("new"));
    }

    #[test]
    fn newest_snapshot_id_is_none_when_empty() {
        assert_eq!(newest_snapshot_id(&json!([])), None);
        assert_eq!(newest_snapshot_id(&json!({})), None);
    }

    #[test]
    fn catalog_pvcs_finds_names_and_capacity() {
        let catalog = json!({
            "services": [
                {"namespace": "yolab-gitea", "pvcs": [{"name": "gitea-data", "capacity": "5Gi"}]},
                {"namespace": "yolab-other", "pvcs": [{"name": "other-data", "capacity": "5Gi"}]},
            ]
        });
        assert_eq!(
            catalog_pvcs(&catalog, "yolab-gitea"),
            vec![("gitea-data".to_string(), "5Gi".to_string())]
        );
    }

    #[test]
    fn catalog_pvcs_defaults_capacity() {
        let catalog = json!({
            "services": [{"namespace": "yolab-gitea", "pvcs": [{"name": "gitea-data"}]}]
        });
        assert_eq!(
            catalog_pvcs(&catalog, "yolab-gitea"),
            vec![("gitea-data".to_string(), "10Gi".to_string())]
        );
    }

    #[test]
    fn catalog_pvcs_is_empty_for_unknown_namespace() {
        assert!(catalog_pvcs(&json!({"services": []}), "yolab-nope").is_empty());
    }
}
