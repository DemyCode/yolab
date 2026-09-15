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
//! *running* (a node is still driving it — see the record's `Claim`), *restorable*
//! (the cluster snapshot completed), or *crashed* (started, but its driver is gone,
//! or it failed). There is no phase machine and no deadline; several sets may be in
//! flight at once.
//!
//! The scheduler is cluster-scoped. It used to run on every node, and each node only
//! knew about the sets IT was driving, so two nodes could each start the daily backup
//! in the same window.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::host::RealHost;
use crate::ops::{self, Claim, Claimed, InFlight, Liveness};
use crate::records::Store;
use crate::routers::apps::{ANN_APP_ID, ANN_CHART_REPO, ANN_CHART_VERSION};
use crate::routers::backup_common::*;
use crate::runtime::{Activity, Controller, Ctx, Requirement, Scope, Tick};
use chrono::{DateTime, Utc};
use tokio::process::Command;

pub(crate) const SETS: Store = Store {
    name: "yolab-backups",
    namespace: "kube-system",
    key: "sets",
};
const MAX_SETS: usize = 50;

/// How long the newest restorable backup may be un-refreshed before the scheduler
/// starts a new one.
const SCHEDULE_INTERVAL_HOURS: i64 = 24;
const SCHEDULE_TICK_SECS: u64 = 300;

/// The one restic call that can legitimately run long: the full B2 upload of the
/// cluster-state snapshot.
const CLUSTER_BACKUP_TIMEOUT_SECS: u64 = 3600;
const PRUNE_TIMEOUT_SECS: u64 = 600;

/// Ids this process is actively backing up.
pub(crate) static IN_FLIGHT: InFlight = InFlight::new();

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
    #[serde(flatten)]
    pub claim: Claim,
}

impl Claimed for BackupSet {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_running(&self) -> bool {
        self.state == "running"
    }
    fn claim(&self) -> &Claim {
        &self.claim
    }
    fn claim_mut(&mut self) -> &mut Claim {
        &mut self.claim
    }
}

/// The three states the page shows. `Crashed` covers both "failed" and "was running
/// when its driver died" — either way it is not something you can restore from.
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

fn upsert(sets: &mut Vec<BackupSet>, set: BackupSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_SETS);
}

async fn read_sets() -> anyhow::Result<Vec<BackupSet>> {
    Ok(SETS.read(&RealHost).await?)
}

async fn record_running(id: &str, triggered_by: &str) -> anyhow::Result<()> {
    let set = BackupSet {
        id: id.to_string(),
        triggered_by: triggered_by.to_string(),
        started_at: Utc::now().to_rfc3339(),
        state: "running".to_string(),
        finished_at: None,
        snapshot_id: None,
        error: None,
        services: vec![],
        claim: Claim::mine(Utc::now()),
    };
    SETS.update(&RealHost, |sets: &mut Vec<BackupSet>| {
        upsert(sets, set.clone())
    })
    .await?;
    Ok(())
}

async fn record_done(id: &str, result: &anyhow::Result<(String, Vec<ServiceSummary>)>) {
    let finished_at = Utc::now().to_rfc3339();
    let written = SETS
        .update(&RealHost, |sets: &mut Vec<BackupSet>| {
            let Some(s) = sets.iter_mut().find(|s| s.id == id) else {
                return;
            };
            s.finished_at = Some(finished_at.clone());
            match result {
                Ok((snapshot_id, services)) => {
                    s.state = "succeeded".to_string();
                    s.snapshot_id = Some(snapshot_id.clone());
                    s.error = None;
                    s.services = services.clone();
                }
                Err(e) => {
                    s.state = "failed".to_string();
                    s.snapshot_id = None;
                    s.error = Some(e.to_string());
                    s.services = vec![];
                }
            }
        })
        .await;
    written.warn_on_err(format!(
        "backup {id}: could not record the outcome — it will read as crashed"
    ));
}

// ── Pure decision functions ────────────────────────────────────────────────────

fn classify(set: &BackupSet, liveness: Liveness) -> SetState {
    match set.state.as_str() {
        "succeeded" => SetState::Restorable,
        "failed" => SetState::Crashed,
        _ if liveness.is_live() => SetState::Running,
        _ => SetState::Crashed,
    }
}

/// Whether a backup is due, based only on the age of the newest restorable one.
/// The "is something already running" gate is separate (`running_anywhere`), so a
/// crashed set must NOT read as "in progress" here, or a crash would stop
/// scheduling forever.
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
    if let Some(why) = crate::heal::backups_blocked().await {
        anyhow::bail!("not backing up: {why}");
    }
    let id = new_id();
    let guard = IN_FLIGHT.claim(&id);
    record_running(&id, triggered_by).await?;

    let task_id = id.clone();
    tokio::spawn(async move {
        // Dropped when the task ends, panics included.
        let _guard = guard;
        let result = run_set(&task_id, &cfg).await;
        // Record the terminal state before dropping the claim, so the page never
        // briefly reads a finished set as "crashed".
        record_done(&task_id, &result).await;
    });

    Ok(id)
}

/// The two halves of one backup, in order. Everything here is safe to redo and bounded
/// by the restic timeouts, so a crash simply leaves a "running" record that the next
/// tick classifies as crashed.
async fn run_set(id: &str, cfg: &BackupConfig) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    // 1. Volumes: trigger every managed PVC, then WAIT for each upload to finish.
    //
    // This used to be fire-and-forget, and the cluster snapshot below then started
    // seconds before the volume uploads did. Restore uses the cluster snapshot's time
    // as VolSync's `restoreAsOf`, so it could never see the volume snapshots taken
    // by its own backup — observed 2026-09-14: cluster snapshot 12:10:02, the app's
    // only volume snapshot 12:10:05, and a restore of it would have found "No
    // eligible snapshots", exited successfully, and left the app empty.
    //
    let pvcs = list_user_pvcs().await?;
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for pvc in &pvcs {
        annotate_ns_privileged_movers(&pvc.namespace).await;
        if let Err(e) = ensure_restic_secret(&pvc.namespace, &pvc.name, cfg).await {
            failures.push(format!(
                "{}/{}: could not write its backup credentials: {e}",
                pvc.namespace, pvc.name
            ));
            continue;
        }
        let before = replication_source(&pvc.namespace, &pvc.name).await;
        match ensure_replication_source(pvc, true).await {
            Ok(Some(trigger)) => pending.push(PendingSync {
                pvc: pvc.clone(),
                trigger,
                logs_before: mover_logs(&before),
            }),
            Ok(None) => {}
            Err(e) => failures.push(format!(
                "{}/{}: could not start: {e}",
                pvc.namespace, pvc.name
            )),
        }
    }
    let (sync_failures, synced) = wait_for_volume_syncs(&pending).await;
    failures.extend(sync_failures);

    // Which restic snapshot each upload produced, so a restore takes exactly that one
    // rather than whatever a timestamp happens to select.
    let mut pinned = HashMap::new();
    for (pvc, rs) in &synced {
        match pin_volume_snapshot(cfg, pvc, rs).await {
            Some(snap) => {
                pinned.insert((pvc.namespace.clone(), pvc.name.clone()), snap);
            }
            None => failures.push(format!(
                "{}/{}: uploaded, but its snapshot could not be identified",
                pvc.namespace, pvc.name
            )),
        }
    }

    // 2. Cluster state, tagged with the set id. Taken even when a volume failed, so
    //    every other app still has a restorable backup from this run.
    let (snapshot_id, services) = snapshot_cluster(cfg, id, &pinned).await?;

    // 3. Retention. Best-effort: if it fails or is skipped this run, the next one
    //    prunes whatever it left behind.
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    restic_timeout(
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
    .await
    .warn_on_err("backup: retention (forget --prune) — the next run prunes instead");

    if !failures.is_empty() {
        anyhow::bail!(
            "the app settings were saved, but {} volume(s) did not back up: {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok((snapshot_id, services))
}

// ── Waiting for volume uploads ─────────────────────────────────────────────────

/// How long one backup may wait for all volume uploads before calling them failed.
const VOLUME_SYNC_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
const VOLUME_SYNC_POLL: Duration = Duration::from_secs(10);

struct PendingSync {
    pvc: PvcInfo,
    trigger: String,
    logs_before: Option<String>,
}

#[derive(Debug, PartialEq)]
enum SyncOutcome {
    Done,
    Failed(String),
    Pending,
}

async fn replication_source(namespace: &str, pvc: &str) -> Value {
    crate::kubectl::get_json(&[
        "get",
        "replicationsource",
        &replication_source_name(pvc),
        "-n",
        namespace,
        "-o",
        "json",
    ])
    .await
    .unwrap_or(Value::Null)
}

fn mover_logs(rs: &Value) -> Option<String> {
    rs["status"]["latestMoverStatus"]["logs"]
        .as_str()
        .map(str::to_string)
}

/// VolSync records `lastManualSync` = the trigger value only once that sync has
/// completed. A failed attempt shows up as a new `latestMoverStatus` with result
/// `Failed` — new meaning its logs differ from what was there before this trigger.
fn sync_outcome(rs: &Value, trigger: &str, logs_before: Option<&str>) -> SyncOutcome {
    let status = &rs["status"];
    let result = status["latestMoverStatus"]["result"].as_str();
    let logs = status["latestMoverStatus"]["logs"].as_str();
    if status["lastManualSync"].as_str() == Some(trigger) {
        return match result {
            Some("Failed") => SyncOutcome::Failed(last_line(logs)),
            _ => SyncOutcome::Done,
        };
    }
    if result == Some("Failed") && logs != logs_before {
        return SyncOutcome::Failed(last_line(logs));
    }
    SyncOutcome::Pending
}

fn last_line(logs: Option<&str>) -> String {
    logs.and_then(|l| l.lines().rev().find(|x| !x.trim().is_empty()))
        .unwrap_or("the upload failed")
        .trim()
        .to_string()
}

/// A volume snapshot pinned by id, as recorded in `catalog.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeSnapshot {
    pub id: String,
    /// restic's own timestamp for it, verbatim.
    pub time: String,
}

/// The snapshot VolSync's restic mover just saved: its log ends with
/// `snapshot f46c4413 saved`.
fn saved_snapshot_id(logs: &str) -> Option<String> {
    logs.lines().rev().find_map(|l| {
        let rest = l.trim().strip_prefix("snapshot ")?;
        let id = rest.strip_suffix(" saved")?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
    })
}

/// `restic snapshots <id> --json` → the full id and time of that one snapshot.
fn parse_pinned(v: &Value) -> Option<VolumeSnapshot> {
    let s = v.as_array()?.first()?;
    Some(VolumeSnapshot {
        id: s["id"].as_str()?.to_string(),
        time: s["time"].as_str()?.to_string(),
    })
}

async fn pin_volume_snapshot(
    cfg: &BackupConfig,
    pvc: &PvcInfo,
    rs: &Value,
) -> Option<VolumeSnapshot> {
    let short = saved_snapshot_id(mover_logs(rs)?.as_str())?;
    let repo = cfg.restic_repo(&format!(
        "volsync/{}/{}",
        pvc.namespace,
        canonical_pvc_id(&pvc.name)
    ));
    let out = restic(&repo, cfg, &["snapshots", "--no-lock", "--json", &short])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_pinned(&serde_json::from_slice(&out.stdout).ok()?)
}

/// Failures, and the source status of every volume that finished.
async fn wait_for_volume_syncs(pending: &[PendingSync]) -> (Vec<String>, Vec<(PvcInfo, Value)>) {
    let deadline = std::time::Instant::now() + VOLUME_SYNC_TIMEOUT;
    let mut waiting: Vec<&PendingSync> = pending.iter().collect();
    let mut failures = Vec::new();
    let mut synced = Vec::new();
    while !waiting.is_empty() {
        let mut still = Vec::new();
        for p in waiting {
            let rs = replication_source(&p.pvc.namespace, &p.pvc.name).await;
            match sync_outcome(&rs, &p.trigger, p.logs_before.as_deref()) {
                SyncOutcome::Done => synced.push((p.pvc.clone(), rs)),
                SyncOutcome::Failed(why) => {
                    failures.push(format!("{}/{}: {why}", p.pvc.namespace, p.pvc.name))
                }
                SyncOutcome::Pending => still.push(p),
            }
        }
        waiting = still;
        if waiting.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            for p in &waiting {
                failures.push(format!(
                    "{}/{}: still uploading after {}h",
                    p.pvc.namespace,
                    p.pvc.name,
                    VOLUME_SYNC_TIMEOUT.as_secs() / 3600
                ));
            }
            break;
        }
        tokio::time::sleep(VOLUME_SYNC_POLL).await;
    }
    (failures, synced)
}

// ── Cluster-state snapshot (etcd + K8s objects + catalog) ──────────────────────

/// Snapshots etcd, exports every managed namespace's objects, and pushes the staging
/// directory to restic tagged with `tag` (and `cluster-backup`, so restore can find it).
/// Returns the restic snapshot id and a summary of the services captured.
async fn snapshot_cluster(
    cfg: &BackupConfig,
    tag: &str,
    pinned: &PinnedVolumes,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let tmp_dir = "/var/lib/yolab/backup-staging".to_string();

    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let result = snapshot_cluster_inner(cfg, tag, &tmp_dir, pinned).await;
    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    result
}

/// (namespace, pvc name) → the snapshot this backup took of it.
type PinnedVolumes = HashMap<(String, String), VolumeSnapshot>;

/// One volume's catalog entry. A pinned snapshot is what restore takes; without one
/// (a volume whose upload failed) restore falls back to the newest snapshot before
/// the cluster snapshot, or refuses.
fn catalog_pvc(name: &str, capacity: &str, pinned: Option<&VolumeSnapshot>) -> Value {
    let mut v = json!({ "name": name, "capacity": capacity });
    if let Some(s) = pinned {
        v["snapshot_id"] = Value::String(s.id.clone());
        v["snapshot_time"] = Value::String(s.time.clone());
    }
    v
}

async fn snapshot_cluster_inner(
    cfg: &BackupConfig,
    tag: &str,
    tmp_dir: &str,
    pinned: &PinnedVolumes,
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
                            std::fs::remove_file(entry.path())
                                .warn_on_err("cluster-backup: remove the local etcd snapshot copy");
                        }
                        crate::kubectl::run(&[
                            "delete",
                            "etcdsnapshotfile",
                            fname_str.as_ref(),
                            "--ignore-not-found",
                        ])
                        .await
                        .warn_on_err("cluster-backup: delete the etcdsnapshotfile record");
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

        // Every read here is `?`. A failed read used to become an empty list, and
        // the backup was then recorded restorable without that app's objects —
        // discovered only when a restore could not find them. A namespace that
        // vanished since it was listed is the one legitimate absence.
        let Some(ns_obj) = crate::kubectl::get_opt(&["get", "namespace", ns, "-o", "json"]).await?
        else {
            continue;
        };
        items.push(ns_obj.clone());
        let ns_obj = Some(ns_obj);

        let raw = crate::kubectl::run(&[
            "get",
            "deploy,svc,secret,configmap",
            "-n",
            ns,
            "-o",
            "json",
            "--ignore-not-found",
        ])
        .await?;
        let workloads: Vec<Value> = if raw.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str::<Value>(&raw)?["items"]
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("kubectl get -n {ns}: no items list"))?
        };
        items.extend(workloads.iter().cloned());

        let sanitized = sanitize_k8s_items_for_backup(&items);
        if !sanitized.is_empty() {
            let list = json!({ "apiVersion": "v1", "kind": "List", "items": sanitized });
            let s = serde_json::to_string_pretty(&list)?;
            tokio::fs::write(format!("{tmp_dir}/{ns}.yaml"), s.as_bytes()).await?;
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
                let snap = pinned.get(&(ns.clone(), name.clone()));
                Some(catalog_pvc(&name, &capacity, snap))
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
    tokio::fs::write(format!("{tmp_dir}/catalog.json"), catalog.to_string()).await?;

    // 3. Init restic repo if needed.
    cfg.unlock("cluster-backup").await;
    let check = restic(&repo, cfg, &["snapshots", "--no-lock"]).await;
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
    let out = restic(
        repo,
        cfg,
        &["snapshots", "--no-lock", "--json", "--tag", tag],
    )
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

fn liveness_of(s: &BackupSet) -> Liveness {
    s.liveness(&crate::system::hostname(), &IN_FLIGHT)
}

/// Every recorded set, newest first, classified into the three page states.
pub(crate) async fn list() -> anyhow::Result<Vec<Value>> {
    let sets = read_sets().await?;
    Ok(sets
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "triggered_by": s.triggered_by,
                "started_at": s.started_at,
                "finished_at": s.finished_at,
                "snapshot_id": s.snapshot_id,
                "error": s.error,
                "services": s.services,
                "state": state_str(classify(s, liveness_of(s))),
                "node": s.claim.owner,
            })
        })
        .collect())
}

/// Whether any set is running on any node — the single-flight gate for "start
/// another". `Err` when the records cannot be read.
pub(crate) async fn running_anywhere() -> anyhow::Result<bool> {
    let sets = read_sets().await?;
    Ok(sets
        .iter()
        .any(|s| s.is_running() && liveness_of(s).is_live()))
}

/// Hours since the newest restorable backup, or `None` if none ever succeeded or
/// the records cannot be read.
pub(crate) async fn last_ok_age_hours() -> Option<i64> {
    let sets = read_sets().await.ok()?;
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

/// Starts a scheduled backup when the newest restorable one is older than the
/// interval and nothing is running anywhere. Cluster-scoped: one node schedules.
pub struct BackupSchedulerController;

impl Controller for BackupSchedulerController {
    fn name(&self) -> &'static str {
        "backup-scheduler"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(SCHEDULE_TICK_SECS)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[Activity::Restore]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(60)
    }
    async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
        if read_master_config().await.is_none() {
            return Ok(Tick::Idle("backups are not enabled".into()));
        }
        if running_anywhere().await? {
            return Ok(Tick::Idle("a backup is already running".into()));
        }
        let sets = read_sets().await?;
        if !should_schedule(&sets, Utc::now()) {
            return Ok(Tick::Idle("the newest backup is recent enough".into()));
        }
        let id = start("schedule").await?;
        tracing::info!("backup: scheduled {id}");
        Ok(Tick::Done)
    }
}

pub(crate) fn start_heartbeat() {
    ops::spawn_heartbeat::<BackupSet>(SETS, &IN_FLIGHT);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(last_manual: &str, result: &str, logs: &str) -> Value {
        json!({"status": {
            "lastManualSync": last_manual,
            "latestMoverStatus": {"result": result, "logs": logs},
        }})
    }

    #[test]
    fn a_volume_is_backed_up_only_once_volsync_records_this_trigger() {
        let old = rs("backup-1", "Successful", "snapshot a saved");
        assert_eq!(
            sync_outcome(&old, "backup-2", Some("snapshot a saved")),
            SyncOutcome::Pending,
            "the previous run's success is not this run's"
        );
        let new = rs("backup-2", "Successful", "snapshot b saved");
        assert_eq!(
            sync_outcome(&new, "backup-2", Some("snapshot a saved")),
            SyncOutcome::Done
        );
    }

    #[test]
    fn a_new_failure_fails_the_volume_but_an_old_one_does_not() {
        let stale = rs("backup-1", "Failed", "old error");
        assert_eq!(
            sync_outcome(&stale, "backup-2", Some("old error")),
            SyncOutcome::Pending
        );
        let fresh = rs(
            "backup-1",
            "Failed",
            "...\nFatal: unable to open repository\n",
        );
        assert_eq!(
            sync_outcome(&fresh, "backup-2", Some("old error")),
            SyncOutcome::Failed("Fatal: unable to open repository".into())
        );
    }

    /// The exact log VolSync 0.16's restic mover left on the live cluster.
    #[test]
    fn the_saved_snapshot_is_read_from_the_mover_log() {
        let logs = "=== Initialize Dir ===\ncreated restic repository 9bc0b3e83e at s3:https://x\n\
                    no parent snapshot found, will read all files\n\
                    Added to the repository: 669.094 MiB (628.814 MiB stored)\n\
                    processed 2158 files, 680.417 MiB in 0:20\nsnapshot f46c4413 saved\n\
                    Restic completed in 25s";
        assert_eq!(saved_snapshot_id(logs), Some("f46c4413".into()));
    }

    #[test]
    fn a_log_without_a_saved_snapshot_pins_nothing() {
        assert_eq!(
            saved_snapshot_id("Fatal: repository is already locked"),
            None
        );
        assert_eq!(saved_snapshot_id("snapshot  saved"), None);
        assert_eq!(saved_snapshot_id("snapshot not-hex saved"), None);
        assert_eq!(saved_snapshot_id(""), None);
    }

    #[test]
    fn the_last_saved_snapshot_wins_when_a_mover_retried() {
        let logs = "snapshot aaaa1111 saved\nerror: prune failed\nsnapshot bbbb2222 saved";
        assert_eq!(saved_snapshot_id(logs), Some("bbbb2222".into()));
    }

    #[test]
    fn a_pinned_snapshot_keeps_restics_full_id_and_exact_time() {
        let v = json!([{"id": "f46c4413abcdef", "short_id": "f46c4413",
                        "time": "2026-09-14T12:10:05.417123456Z"}]);
        assert_eq!(
            parse_pinned(&v),
            Some(VolumeSnapshot {
                id: "f46c4413abcdef".into(),
                time: "2026-09-14T12:10:05.417123456Z".into()
            })
        );
        assert_eq!(parse_pinned(&json!([])), None);
        assert_eq!(parse_pinned(&json!([{"id": "x"}])), None);
    }

    #[test]
    fn a_catalog_volume_carries_its_snapshot_only_when_it_has_one() {
        let snap = VolumeSnapshot {
            id: "abc".into(),
            time: "2026-09-14T12:10:05Z".into(),
        };
        assert_eq!(
            catalog_pvc("data", "5Gi", Some(&snap)),
            json!({"name": "data", "capacity": "5Gi", "snapshot_id": "abc",
                   "snapshot_time": "2026-09-14T12:10:05Z"})
        );
        assert_eq!(
            catalog_pvc("data", "5Gi", None),
            json!({"name": "data", "capacity": "5Gi"})
        );
    }

    #[test]
    fn a_source_that_cannot_be_read_is_still_pending() {
        assert_eq!(
            sync_outcome(&Value::Null, "backup-2", None),
            SyncOutcome::Pending
        );
    }

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
            claim: Claim::default(),
        }
    }

    #[test]
    fn a_succeeded_set_is_restorable_regardless_of_in_flight() {
        assert_eq!(
            classify(&set("a", "succeeded"), Liveness::Driving),
            SetState::Restorable
        );
        assert_eq!(
            classify(&set("a", "succeeded"), Liveness::Abandoned),
            SetState::Restorable
        );
    }

    #[test]
    fn a_failed_set_is_crashed() {
        assert_eq!(
            classify(&set("a", "failed"), Liveness::Driving),
            SetState::Crashed
        );
    }

    #[test]
    fn a_running_set_is_running_only_while_someone_drives_it() {
        assert_eq!(
            classify(&set("a", "running"), Liveness::Driving),
            SetState::Running
        );
        assert_eq!(
            classify(&set("a", "running"), Liveness::Remote),
            SetState::Running
        );
        assert_eq!(
            classify(&set("a", "running"), Liveness::Abandoned),
            SetState::Crashed
        );
    }

    #[test]
    fn an_unknown_state_reads_as_crashed_when_not_in_flight() {
        assert_eq!(
            classify(&set("a", "weird"), Liveness::Abandoned),
            SetState::Crashed
        );
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
