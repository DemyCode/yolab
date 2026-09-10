//! The backup itself, reduced to what it actually is.
//!
//! A backup is one id-tagged operation with two independent halves:
//!
//!   1. every managed PVC's VolSync ReplicationSource is stamped with a fresh manual
//!      trigger, and
//!   2. the cluster state (etcd snapshot + K8s object export) is pushed to restic,
//!      tagged with the same id.
//!
//! The set's lifecycle is recorded in a ConfigMap so the page can show three states:
//! *running* (this process is still driving it), *restorable* (the cluster snapshot
//! completed), or *crashed* (started, but the process that owned it is gone, or it
//! failed). There is deliberately no phase machine, no deadline, no watchdog and no
//! cross-process lock: a set is fire-and-forget, and several may be in flight at once.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routers::apps::{ANN_APP_ID, ANN_CHART_REPO, ANN_CHART_VERSION};
use crate::routers::backup_common::*;
use chrono::{DateTime, Utc};
use tokio::process::Command;

const SETS_CONFIGMAP: &str = "yolab-backups";
const SETS_NS: &str = "kube-system";
const MAX_SETS: usize = 50;

/// How long the newest restorable backup may be un-refreshed before the scheduler
/// starts a new one.
const SCHEDULE_INTERVAL_HOURS: i64 = 24;
const SCHEDULE_TICK_SECS: u64 = 300;

/// The one restic call that can legitimately run long: the full B2 upload of the
/// cluster-state snapshot.
const CLUSTER_BACKUP_TIMEOUT_SECS: u64 = 3600;
const PRUNE_TIMEOUT_SECS: u64 = 600;

/// Ids this process is actively backing up. A ConfigMap record that says "running" but
/// is not in here means the process that started it died — the definition of "crashed".
/// A `Vec` rather than a set because there are never more than a handful in flight.
static IN_FLIGHT: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ServiceSummary {
    instance_name: String,
    pvc_count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct BackupSet {
    pub id: String,
    #[serde(default)]
    pub triggered_by: String,
    pub started_at: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSummary>,
}

/// The three states the page shows. `Crashed` covers both "failed" and "was running
/// when the process died" — either way it is not something you can restore from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SetState {
    Running,
    Restorable,
    Crashed,
}

fn state_str(s: SetState) -> &'static str {
    match s {
        SetState::Running => "running",
        SetState::Restorable => "restorable",
        SetState::Crashed => "crashed",
    }
}

pub(crate) fn new_id() -> String {
    format!("bk-{}", random_hex(8))
}

// ── ConfigMap records ──────────────────────────────────────────────────────────

fn parse_sets(raw: &str) -> Vec<BackupSet> {
    serde_json::from_str::<Vec<BackupSet>>(raw).unwrap_or_default()
}

fn upsert(sets: &mut Vec<BackupSet>, set: BackupSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_SETS);
}

async fn read_sets() -> Vec<BackupSet> {
    let Ok(v) = crate::kubectl::get_json(&[
        "get",
        "configmap",
        SETS_CONFIGMAP,
        "-n",
        SETS_NS,
        "-o",
        "json",
    ])
    .await
    else {
        return Vec::new();
    };
    parse_sets(v["data"]["sets"].as_str().unwrap_or("[]"))
}

async fn write_sets(sets: &[BackupSet]) {
    let raw = serde_json::to_string(sets).unwrap_or_else(|_| "[]".into());
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": SETS_CONFIGMAP,
            "namespace": SETS_NS,
            "labels": { "app.kubernetes.io/managed-by": "yolab" },
        },
        "data": { "sets": raw },
    });
    let _ = crate::kubectl::apply(&manifest.to_string()).await;
}

async fn record_running(id: &str, triggered_by: &str) {
    let mut sets = read_sets().await;
    upsert(
        &mut sets,
        BackupSet {
            id: id.to_string(),
            triggered_by: triggered_by.to_string(),
            started_at: Utc::now().to_rfc3339(),
            state: "running".to_string(),
            finished_at: None,
            snapshot_id: None,
            error: None,
            services: vec![],
        },
    );
    write_sets(&sets).await;
}

async fn record_done(id: &str, result: &anyhow::Result<(String, Vec<ServiceSummary>)>) {
    let mut sets = read_sets().await;
    let finished_at = Utc::now().to_rfc3339();
    let set = match result {
        Ok((snapshot_id, services)) => BackupSet {
            id: id.to_string(),
            triggered_by: sets
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.triggered_by.clone())
                .unwrap_or_default(),
            started_at: sets
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.started_at.clone())
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
            state: "succeeded".to_string(),
            finished_at: Some(finished_at),
            snapshot_id: Some(snapshot_id.clone()),
            error: None,
            services: services.clone(),
        },
        Err(e) => BackupSet {
            id: id.to_string(),
            triggered_by: sets
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.triggered_by.clone())
                .unwrap_or_default(),
            started_at: sets
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.started_at.clone())
                .unwrap_or_else(|| Utc::now().to_rfc3339()),
            state: "failed".to_string(),
            finished_at: Some(finished_at),
            snapshot_id: None,
            error: Some(e.to_string()),
            services: vec![],
        },
    };
    upsert(&mut sets, set);
    write_sets(&sets).await;
}

// ── Pure decision functions ────────────────────────────────────────────────────

fn classify(set: &BackupSet, in_flight: bool) -> SetState {
    match set.state.as_str() {
        "succeeded" => SetState::Restorable,
        "failed" => SetState::Crashed,
        _ => {
            if in_flight {
                SetState::Running
            } else {
                SetState::Crashed
            }
        }
    }
}

/// Whether a backup is due, based only on the age of the newest restorable one.
/// The "is something already running" gate is separate (`is_running`), so a crashed
/// set — recorded "running" but no longer driven by any process — must NOT read as
/// "in progress" here, or a crash would stop scheduling forever.
fn should_schedule(sets: &[BackupSet], now: DateTime<Utc>) -> bool {
    let last_ok = sets
        .iter()
        .find(|s| s.state == "succeeded")
        .and_then(|s| s.finished_at.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    match last_ok {
        Some(t) => (now - t).num_hours() >= SCHEDULE_INTERVAL_HOURS,
        None => true,
    }
}

// ── The operation ──────────────────────────────────────────────────────────────

/// Starts a backup set and returns immediately. The work runs detached on a spawned
/// task, so it survives this (HTTP) caller ending and several sets can overlap.
pub(crate) async fn start(triggered_by: &str) -> anyhow::Result<String> {
    let Some(cfg) = read_master_config().await else {
        anyhow::bail!("backup not configured");
    };
    let id = new_id();
    IN_FLIGHT.lock().unwrap().push(id.clone());
    record_running(&id, triggered_by).await;

    let task_id = id.clone();
    tokio::spawn(async move {
        let result = run_set(&task_id, &cfg).await;
        // Record the terminal state before dropping the in-flight claim, so the page
        // never briefly reads a finished set as "crashed".
        record_done(&task_id, &result).await;
        {
            let mut guard = IN_FLIGHT.lock().unwrap();
            guard.retain(|s| s != &task_id);
        }
    });

    Ok(id)
}

/// The two halves of one backup, in order. Everything here is safe to redo and bounded
/// by the restic timeouts, so a crash simply leaves a "running" record that the next
/// tick classifies as crashed.
async fn run_set(id: &str, cfg: &BackupConfig) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    // 1. Volumes: trigger every managed PVC. Fire-and-forget by design — VolSync's
    //    mover runs in the background and its status is read separately.
    let pvcs = list_user_pvcs().await?;
    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        let _ = ensure_restic_secret(&pvc.namespace, &pvc.name, cfg).await;
        let _ = ensure_replication_source(pvc, true).await;
    }

    // 2. Cluster state, tagged with the set id.
    let (snapshot_id, services) = snapshot_cluster(cfg, id).await?;

    // 3. Retention. Best-effort: if it fails or is skipped this run, the next one
    //    prunes whatever it left behind.
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let _ = restic_timeout(
        &repo,
        cfg,
        &[
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
        ],
        Duration::from_secs(PRUNE_TIMEOUT_SECS),
    )
    .await;

    Ok((snapshot_id, services))
}

// ── Cluster-state snapshot (etcd + K8s objects + catalog) ──────────────────────

/// Snapshots etcd, exports every managed namespace's objects, and pushes the staging
/// directory to restic tagged with `tag` (and `cluster-backup`, so restore can find it).
/// Returns the restic snapshot id and a summary of the services captured.
async fn snapshot_cluster(
    cfg: &BackupConfig,
    tag: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let tmp_dir = "/var/lib/yolab/backup-staging".to_string();

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    tokio::fs::create_dir_all(&tmp_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let result = snapshot_cluster_inner(cfg, tag, &tmp_dir).await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    result
}

async fn snapshot_cluster_inner(
    cfg: &BackupConfig,
    tag: &str,
    tmp_dir: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let date = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let repo = cfg.restic_repo("cluster-backup");

    // 1. etcd snapshot — archived as etcd.db in this restic snapshot, consumed only by
    //    the external dr-restore script (restore_run restores volumes + K8s objects).
    let snap_name = format!("yolab-cluster-{date}");
    let snap_saved = Command::new("k3s")
        .args(["etcd-snapshot", "save", &format!("--name={snap_name}")])
        .kill_on_drop(true)
        .output()
        .await;

    if let Ok(o) = &snap_saved {
        if o.status.success() {
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
                        let _ = crate::kubectl::run(&[
                            "delete",
                            "etcdsnapshotfile",
                            fname_str.as_ref(),
                            "--ignore-not-found",
                        ])
                        .await;
                        break;
                    }
                }
            }
        } else {
            tracing::warn!(
                "cluster-backup: etcd-snapshot: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
    }

    // 2. Export K8s objects for all yolab-managed namespaces.
    let namespaces = list_managed_namespaces().await?;
    let mut services: Vec<Value> = Vec::new();

    for ns in &namespaces {
        let mut items: Vec<Value> = Vec::new();

        let ns_obj: Option<Value> =
            crate::kubectl::get_json(&["get", "namespace", ns, "-o", "json"])
                .await
                .ok();
        if let Some(v) = &ns_obj {
            items.push(v.clone());
        }

        let workloads: Vec<Value> = crate::kubectl::get_json(&[
            "get",
            "deploy,svc,secret,configmap",
            "-n",
            ns,
            "-o",
            "json",
            "--ignore-not-found",
        ])
        .await
        .ok()
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

        let ann = ns_obj
            .as_ref()
            .and_then(|v| v["metadata"]["annotations"].as_object().cloned())
            .unwrap_or_default();
        let app_id = ann
            .get(ANN_APP_ID)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
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

        let pvcs: Vec<Value> = crate::kubectl::get_json(&["get", "pvc", "-n", ns, "-o", "json"])
            .await
            .ok()
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

    // 3. Init restic repo if needed.
    cfg.unlock("cluster-backup").await;
    let check = restic(&repo, cfg, &["snapshots"]).await;
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        let init = restic(&repo, cfg, &["init"]).await?;
        if !init.status.success() {
            anyhow::bail!(
                "restic init failed: {}",
                String::from_utf8_lossy(&init.stderr).trim()
            );
        }
    }

    // 4. Backup, tagged with the set id and the stable `cluster-backup` tag restore
    //    looks for.
    let backup = restic_timeout(
        &repo,
        cfg,
        &["backup", tmp_dir, "--tag", "cluster-backup", "--tag", tag],
        Duration::from_secs(CLUSTER_BACKUP_TIMEOUT_SECS),
    )
    .await?;
    if !backup.status.success() {
        anyhow::bail!(
            "restic backup failed: {}",
            String::from_utf8_lossy(&backup.stderr).trim()
        );
    }

    newest_snapshot_id(&repo, cfg, tag)
        .await
        .ok_or_else(|| anyhow::anyhow!("backup completed but no snapshot id could be read"))
        .map(|snapshot_id| (snapshot_id, summarize_services(&services)))
}

fn summarize_services(services: &[Value]) -> Vec<ServiceSummary> {
    services
        .iter()
        .map(|s| ServiceSummary {
            instance_name: s["instance_name"].as_str().unwrap_or("").to_string(),
            pvc_count: s["pvcs"].as_array().map(|a| a.len()).unwrap_or(0),
        })
        .collect()
}

/// The newest restic snapshot id carrying `tag`, if any.
async fn newest_snapshot_id(repo: &str, cfg: &BackupConfig, tag: &str) -> Option<String> {
    let out = restic(repo, cfg, &["snapshots", "--json", "--tag", tag])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?
        .iter()
        .max_by_key(|s| s["time"].as_str().unwrap_or("").to_string())
        .and_then(|s| s["id"].as_str().map(String::from))
}

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

fn built_hash() -> Option<String> {
    std::fs::read_to_string("/var/lib/yolab/built-hash")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── Read side ──────────────────────────────────────────────────────────────────

fn in_flight_ids() -> Vec<String> {
    IN_FLIGHT.lock().unwrap().clone()
}

/// Every recorded set, newest first, classified into the three page states.
pub(crate) async fn list() -> Vec<Value> {
    let sets = read_sets().await;
    let in_flight: HashSet<String> = in_flight_ids().into_iter().collect();
    sets.iter()
        .map(|s| {
            let state = classify(s, in_flight.contains(&s.id));
            json!({
                "id": s.id,
                "triggered_by": s.triggered_by,
                "started_at": s.started_at,
                "finished_at": s.finished_at,
                "snapshot_id": s.snapshot_id,
                "error": s.error,
                "services": s.services,
                "state": state_str(state),
            })
        })
        .collect()
}

/// Whether any set is currently running — the single-flight gate for "start another".
pub(crate) async fn is_running() -> bool {
    let sets = read_sets().await;
    let in_flight: HashSet<String> = in_flight_ids().into_iter().collect();
    sets.iter()
        .any(|s| s.state == "running" && in_flight.contains(&s.id))
}

/// Hours since the newest restorable backup, or `None` if none ever succeeded.
pub(crate) async fn last_ok_age_hours() -> Option<i64> {
    let sets = read_sets().await;
    sets.iter()
        .find(|s| s.state == "succeeded")
        .and_then(|s| s.finished_at.as_deref())
        .and_then(hours_since)
}

/// True while any VolSync mover pod is actively pushing PVC data — a belt-and-braces
/// signal that volume work is in flight even outside a recorded set.
pub(crate) async fn volsync_mover_running() -> bool {
    crate::kubectl::run(&[
        "get",
        "pods",
        "-A",
        "-l",
        "app.kubernetes.io/created-by=volsync",
        "--field-selector=status.phase=Running",
        "-o",
        "name",
    ])
    .await
    .map(|s| s.lines().any(|l| l.contains("volsync-src-")))
    .unwrap_or(false)
}

/// Background loop that starts a scheduled backup when the newest restorable one is
/// older than the interval and nothing is running. A crashed record (running but no
/// longer in flight) is not treated as "in progress" and never blocks a fresh start.
pub(crate) async fn run_scheduler() {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        if read_master_config().await.is_some() && !is_running().await {
            let sets = read_sets().await;
            if should_schedule(&sets, Utc::now()) {
                if let Err(e) = start("schedule").await {
                    tracing::warn!("backup: scheduled start failed: {e}");
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(SCHEDULE_TICK_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: &str, state: &str) -> BackupSet {
        BackupSet {
            id: id.into(),
            triggered_by: "manual".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            state: state.into(),
            finished_at: if state == "running" {
                None
            } else {
                Some("2026-01-01T00:05:00Z".into())
            },
            snapshot_id: None,
            error: None,
            services: vec![],
        }
    }

    #[test]
    fn a_succeeded_set_is_restorable_regardless_of_in_flight() {
        assert_eq!(classify(&set("a", "succeeded"), true), SetState::Restorable);
        assert_eq!(
            classify(&set("a", "succeeded"), false),
            SetState::Restorable
        );
    }

    #[test]
    fn a_failed_set_is_crashed() {
        assert_eq!(classify(&set("a", "failed"), false), SetState::Crashed);
    }

    #[test]
    fn a_running_set_is_running_only_while_in_flight() {
        assert_eq!(classify(&set("a", "running"), true), SetState::Running);
        assert_eq!(classify(&set("a", "running"), false), SetState::Crashed);
    }

    #[test]
    fn an_unknown_state_reads_as_crashed_when_not_in_flight() {
        assert_eq!(classify(&set("a", "weird"), false), SetState::Crashed);
    }

    #[test]
    fn upsert_replaces_by_id_and_keeps_newest_first() {
        let mut sets = vec![set("a", "running"), set("b", "succeeded")];
        upsert(&mut sets, set("a", "succeeded"));
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].id, "a");
        assert_eq!(sets[0].state, "succeeded");
        assert_eq!(sets[1].id, "b");
    }

    #[test]
    fn upsert_caps_the_list() {
        let mut sets: Vec<BackupSet> = (0..100)
            .map(|i| set(&format!("bk-{i}"), "succeeded"))
            .collect();
        upsert(&mut sets, set("bk-new", "running"));
        assert_eq!(sets.len(), MAX_SETS);
        assert_eq!(sets[0].id, "bk-new");
    }

    #[test]
    fn parse_sets_ignores_garbage() {
        assert!(parse_sets("not json").is_empty());
        assert!(parse_sets("{}").is_empty());
    }

    #[test]
    fn parse_sets_round_trips() {
        let sets = vec![set("a", "succeeded"), set("b", "running")];
        let raw = serde_json::to_string(&sets).unwrap();
        let parsed = parse_sets(&raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "a");
        assert_eq!(parsed[1].state, "running");
    }

    #[test]
    fn new_id_is_unique_enough() {
        assert_ne!(new_id(), new_id());
        assert!(new_id().starts_with("bk-"));
    }

    // ── should_schedule ────────────────────────────────────────────────────────

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn never_backed_up_means_due() {
        assert!(should_schedule(&[], at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn a_recent_success_is_not_due() {
        let mut s = set("a", "succeeded");
        s.finished_at = Some("2025-12-31T23:00:00Z".into());
        assert!(!should_schedule(&[s], at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn an_old_success_is_due() {
        let mut s = set("a", "succeeded");
        s.finished_at = Some("2025-12-01T00:00:00Z".into());
        assert!(should_schedule(&[s], at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn a_crashed_set_does_not_block_scheduling() {
        // A record left "running" by a process that died must not read as in-progress
        // here — the running gate is is_running() — or a crash stops backups forever.
        let mut s = set("a", "succeeded");
        s.finished_at = Some("2025-12-01T00:00:00Z".into());
        let crashed = set("b", "running");
        assert!(should_schedule(&[crashed, s], at("2026-01-01T00:00:00Z")));
    }

    // ── collect_images ─────────────────────────────────────────────────────────

    fn workload(init: &[&str], main: &[&str]) -> Value {
        json!({"spec": {"template": {"spec": {
            "initContainers": init.iter().map(|i| json!({"image": i})).collect::<Vec<_>>(),
            "containers": main.iter().map(|i| json!({"image": i})).collect::<Vec<_>>(),
        }}}})
    }

    #[test]
    fn collect_images_deduplicates_and_sorts() {
        let images = collect_images(&[
            workload(&["migrate:1.0"], &["app:2.0"]),
            workload(&[], &["app:2.0"]),
        ]);
        assert_eq!(images, vec!["app:2.0", "migrate:1.0"]);
    }

    #[test]
    fn collect_images_handles_empty() {
        assert!(collect_images(&[]).is_empty());
        assert!(collect_images(&[json!({})]).is_empty());
    }

    // ── summarize_services ─────────────────────────────────────────────────────

    #[test]
    fn summarize_services_maps_names_to_names_and_counts() {
        let services = vec![
            json!({"instance_name": "gitea", "pvcs": [{"name": "a"}, {"name": "b"}]}),
            json!({"instance_name": "filebrowser", "pvcs": []}),
        ];
        let summary = summarize_services(&services);
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].instance_name, "gitea");
        assert_eq!(summary[0].pvc_count, 2);
        assert_eq!(summary[1].instance_name, "filebrowser");
        assert_eq!(summary[1].pvc_count, 0);
    }
}
