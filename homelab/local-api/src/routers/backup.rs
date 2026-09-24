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

const SCHEDULE_TICK_SECS: u64 = 300;

const CLUSTER_BACKUP_TIMEOUT_SECS: u64 = 3600;
const PRUNE_TIMEOUT_SECS: u64 = 600;

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
    #[serde(default)]
    pub namespace: String,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackupTarget {
    Cluster,
    App(String),
}

impl BackupTarget {
    fn namespace(&self) -> &str {
        match self {
            BackupTarget::Cluster => "",
            BackupTarget::App(ns) => ns,
        }
    }
    fn is_cluster(&self) -> bool {
        matches!(self, BackupTarget::Cluster)
    }
}

impl Claimed for BackupSet {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_running(&self) -> bool {
        self.state == RUNNING
    }
    fn claim(&self) -> &Claim {
        &self.claim
    }
    fn claim_mut(&mut self) -> &mut Claim {
        &mut self.claim
    }
}

pub(crate) const QUEUED: &str = "queued";
pub(crate) const RUNNING: &str = "running";
pub(crate) const SUCCEEDED: &str = "succeeded";
pub(crate) const FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SetState {
    Queued,
    Running,
    Restorable,
    Crashed,
}

fn state_str(s: SetState) -> &'static str {
    match s {
        SetState::Queued => "queued",
        SetState::Running => "running",
        SetState::Restorable => "restorable",
        SetState::Crashed => "crashed",
    }
}

pub(crate) fn new_id() -> String {
    format!("bk-{}", random_hex(8))
}

fn upsert(sets: &mut Vec<BackupSet>, set: BackupSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_SETS);
}

async fn read_sets() -> anyhow::Result<Vec<BackupSet>> {
    Ok(SETS.read(&RealHost).await?)
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
                    s.state = SUCCEEDED.to_string();
                    s.snapshot_id = Some(snapshot_id.clone());
                    s.error = None;
                    s.services = services.clone();
                }
                Err(e) => {
                    s.state = FAILED.to_string();
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

fn classify(set: &BackupSet, liveness: Liveness) -> SetState {
    match set.state.as_str() {
        SUCCEEDED => SetState::Restorable,
        FAILED => SetState::Crashed,
        QUEUED => SetState::Queued,
        _ if liveness.is_live() => SetState::Running,
        _ => SetState::Crashed,
    }
}

fn last_ok_for(sets: &[BackupSet], namespace: &str) -> Option<DateTime<Utc>> {
    sets.iter()
        .filter(|s| s.namespace == namespace && s.state == SUCCEEDED)
        .find_map(|s| s.finished_at.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn running_for(sets: &[BackupSet], namespace: &str) -> bool {
    sets.iter()
        .any(|s| s.namespace == namespace && s.is_running() && liveness_of(s).is_live())
}

pub(crate) fn queued_for<'a>(sets: &'a [BackupSet], namespace: &str) -> Option<&'a BackupSet> {
    sets.iter()
        .find(|s| s.namespace == namespace && s.state == QUEUED)
}

fn anything_running(sets: &[BackupSet]) -> bool {
    sets.iter()
        .any(|s| s.is_running() && liveness_of(s).is_live())
}

pub(crate) fn next_in_line(sets: &[BackupSet]) -> Option<&BackupSet> {
    sets.iter()
        .filter(|s| s.state == QUEUED)
        .min_by(|a, b| a.started_at.cmp(&b.started_at))
}

const DR_SCHEDULE: &str = "0 4 * * *";

pub(crate) async fn start(triggered_by: &str) -> anyhow::Result<String> {
    enqueue(BackupTarget::Cluster, triggered_by).await
}

pub(crate) async fn start_app(namespace: &str, triggered_by: &str) -> anyhow::Result<String> {
    enqueue(BackupTarget::App(namespace.to_string()), triggered_by).await
}

async fn enqueue(target: BackupTarget, triggered_by: &str) -> anyhow::Result<String> {
    if read_master_config().await.is_none() {
        anyhow::bail!("backup not configured");
    }
    if let Some(why) = crate::heal::backups_blocked().await {
        anyhow::bail!("not backing up: {why}");
    }

    let namespace = target.namespace().to_string();
    let id = new_id();
    let queued = BackupSet {
        id: id.clone(),
        triggered_by: triggered_by.to_string(),
        namespace: namespace.clone(),
        started_at: Utc::now().to_rfc3339(),
        state: QUEUED.to_string(),
        finished_at: None,
        snapshot_id: None,
        error: None,
        services: vec![],
        claim: Claim::default(),
    };

    let mut existing = None;
    SETS.update(&RealHost, |sets: &mut Vec<BackupSet>| {
        existing = queued_for(sets, &namespace)
            .map(|s| s.id.clone())
            .or_else(|| {
                sets.iter()
                    .find(|s| {
                        s.namespace == namespace && s.is_running() && liveness_of(s).is_live()
                    })
                    .map(|s| s.id.clone())
            });
        if existing.is_none() {
            upsert(sets, queued.clone());
        }
    })
    .await?;

    crate::runtime::wake("backup-scheduler");
    Ok(existing.unwrap_or(id))
}

pub(crate) struct Promoted {
    id: String,
    target: BackupTarget,
    guard: ops::InFlightGuard,
}

async fn promote_next() -> anyhow::Result<Option<Promoted>> {
    let sets = read_sets().await?;
    if anything_running(&sets) {
        return Ok(None);
    }
    let Some(next) = next_in_line(&sets) else {
        return Ok(None);
    };
    let (id, target) = (next.id.clone(), target_of(next));

    let guard = IN_FLIGHT.claim(&id);
    let mut won = false;
    let claim_id = id.clone();
    SETS.update(&RealHost, |sets: &mut Vec<BackupSet>| {
        won = false;
        if anything_running(sets) {
            return;
        }
        let Some(s) = sets
            .iter_mut()
            .find(|s| s.id == claim_id && s.state == QUEUED)
        else {
            return;
        };
        s.state = RUNNING.to_string();
        s.started_at = Utc::now().to_rfc3339();
        s.claim = Claim::mine(Utc::now());
        won = true;
    })
    .await?;

    if !won {
        drop(guard);
        return Ok(None);
    }
    Ok(Some(Promoted { id, target, guard }))
}

fn target_of(set: &BackupSet) -> BackupTarget {
    if set.namespace.is_empty() {
        BackupTarget::Cluster
    } else {
        BackupTarget::App(set.namespace.clone())
    }
}

fn spawn_run(cfg: BackupConfig, promoted: Promoted) {
    let Promoted { id, target, guard } = promoted;
    tokio::spawn(async move {
        let _guard = guard;
        let result = run_set(&cfg, &target, &id).await;
        record_done(&id, &result).await;
        crate::runtime::wake("backup-scheduler");
    });
}

async fn run_set(
    cfg: &BackupConfig,
    target: &BackupTarget,
    run_id: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let pvcs: Vec<PvcInfo> = list_user_pvcs()
        .await?
        .into_iter()
        .filter(|p| match target {
            BackupTarget::Cluster => true,
            BackupTarget::App(ns) => &p.namespace == ns,
        })
        .collect();
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

    let (snapshot_id, services) = snapshot_cluster(cfg, &pinned, target, run_id).await?;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeSnapshot {
    pub id: String,
    pub time: String,
}

fn saved_snapshot_id(logs: &str) -> Option<String> {
    logs.lines().rev().find_map(|l| {
        let rest = l.trim().strip_prefix("snapshot ")?;
        let id = rest.strip_suffix(" saved")?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
    })
}

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

type PinnedVolumes = HashMap<(String, String), VolumeSnapshot>;

fn catalog_pvc(name: &str, capacity: &str, pinned: Option<&VolumeSnapshot>) -> Value {
    let mut v = json!({ "name": name, "capacity": capacity });
    if let Some(s) = pinned {
        v["snapshot_id"] = Value::String(s.id.clone());
        v["snapshot_time"] = Value::String(s.time.clone());
    }
    v
}

pub(crate) const STAGING_ROOT: &str = "/var/lib/yolab/backup-staging";

pub(crate) fn staging_dir(root: &str, run_id: &str) -> String {
    format!("{root}/{run_id}")
}

pub(crate) fn stale_staging_dirs(present: &[String], keep: &[String]) -> Vec<String> {
    present
        .iter()
        .filter(|name| !keep.contains(name))
        .cloned()
        .collect()
}

async fn sweep_staging(keep: &[String]) {
    let Ok(mut entries) = tokio::fs::read_dir(STAGING_ROOT).await else {
        return;
    };
    let mut present = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            present.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    for name in stale_staging_dirs(&present, keep) {
        tokio::fs::remove_dir_all(staging_dir(STAGING_ROOT, &name))
            .await
            .debug_on_err(format!(
                "backup: clear the leftover staging directory {name}"
            ));
    }
}

async fn snapshot_cluster(
    cfg: &BackupConfig,
    pinned: &PinnedVolumes,
    target: &BackupTarget,
    run_id: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let tmp_dir = staging_dir(STAGING_ROOT, run_id);

    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let result = snapshot_cluster_inner(cfg, &tmp_dir, pinned, target).await;
    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    result
}

const CLUSTER_SCOPED_EXPORT: &[&str] = &[
    "namespaces",
    "customresourcedefinitions.apiextensions.k8s.io",
    "storageclasses.storage.k8s.io",
    "persistentvolumes",
    "priorityclasses.scheduling.k8s.io",
    "clusterroles.rbac.authorization.k8s.io",
    "clusterrolebindings.rbac.authorization.k8s.io",
    "volumeattachments.storage.k8s.io",
    "volumesnapshotclasses.snapshot.storage.k8s.io",
    "csidrivers.storage.k8s.io",
    "csinodes.storage.k8s.io",
    "mutatingwebhookconfigurations.admissionregistration.k8s.io",
    "validatingwebhookconfigurations.admissionregistration.k8s.io",
    "ingressclasses.networking.k8s.io",
    "runtimeclasses.node.k8s.io",
];

async fn export_cluster_scoped(tmp_dir: &str) -> anyhow::Result<()> {
    let mut items: Vec<Value> = Vec::new();
    for res in CLUSTER_SCOPED_EXPORT {
        match crate::kubectl::run(&["get", res, "-o", "json", "--ignore-not-found"]).await {
            Ok(raw) if !raw.trim().is_empty() => match serde_json::from_str::<Value>(&raw) {
                Ok(v) => {
                    if let Some(list) = v["items"].as_array() {
                        items.extend(list.iter().cloned());
                    } else {
                        items.push(v);
                    }
                }
                Err(e) => tracing::warn!("cluster-backup: parse {res}: {e}"),
            },
            Ok(_) => {}
            Err(e) => tracing::warn!("cluster-backup: export {res}: {e}"),
        }
    }
    let sanitized = sanitize_k8s_items_for_backup(&items);
    let list = json!({ "apiVersion": "v1", "kind": "List", "items": sanitized });
    tokio::fs::write(
        format!("{tmp_dir}/cluster-resources.yaml"),
        serde_json::to_string_pretty(&list)?,
    )
    .await?;
    Ok(())
}

async fn snapshot_cluster_inner(
    cfg: &BackupConfig,
    tmp_dir: &str,
    pinned: &PinnedVolumes,
    target: &BackupTarget,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let date = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let repo = cfg.restic_repo("cluster-backup");

    let namespaces: Vec<String> = match target {
        BackupTarget::Cluster => list_managed_namespaces().await?,
        BackupTarget::App(ns) => vec![ns.clone()],
    };
    let include_etcd = target.is_cluster();

    let mut pvcs_by_ns: HashMap<String, Vec<PvcInfo>> = HashMap::new();
    for pvc in list_user_pvcs().await? {
        pvcs_by_ns
            .entry(pvc.namespace.clone())
            .or_default()
            .push(pvc);
    }

    if include_etcd {
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
                                std::fs::remove_file(entry.path()).warn_on_err(
                                    "cluster-backup: remove the local etcd snapshot copy",
                                );
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
    }

    if include_etcd {
        if let Err(e) = export_cluster_scoped(tmp_dir).await {
            tracing::warn!("cluster-backup: cluster-scoped export: {e}");
        }
    }

    let mut services: Vec<Value> = Vec::new();

    for ns in &namespaces {
        let mut items: Vec<Value> = Vec::new();

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

        let pvcs: Vec<Value> = pvcs_by_ns
            .get(ns)
            .map(|list| {
                list.iter()
                    .map(|p| {
                        let snap = pinned.get(&(ns.clone(), p.name.clone()));
                        catalog_pvc(&p.name, &p.capacity, snap)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let images = collect_images(&workloads);

        let definition = crate::routers::apps::read_definition_opt(ns).await;
        let (service_name, resources, volumes, backup_policy) = match &definition {
            Some(d) => (
                d.service_name.clone(),
                serde_json::to_value(&d.resources).unwrap_or(Value::Null),
                serde_json::to_value(&d.volumes).unwrap_or(Value::Null),
                serde_json::to_value(&d.backup).unwrap_or(Value::Null),
            ),
            None => (String::new(), Value::Null, Value::Null, Value::Null),
        };

        services.push(json!({
            "namespace": ns,
            "app_id": app_id,
            "chart_repo": chart_repo,
            "chart_version": chart_version,
            "instance_name": ns.strip_prefix("yolab-").unwrap_or(ns),
            "service_name": service_name,
            "resources": resources,
            "volumes": volumes,
            "backup": backup_policy,
            "definition": definition,
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

    let manifest = rebuild_manifest(&services).await;
    tokio::fs::write(
        format!("{tmp_dir}/manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )
    .await?;

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

    let scope_tag = match target {
        BackupTarget::Cluster => "scope:cluster".to_string(),
        BackupTarget::App(ns) => format!("namespace:{ns}"),
    };
    let backup = restic_timeout(
        &repo,
        cfg,
        &[
            "backup",
            tmp_dir,
            "--tag",
            "cluster-backup",
            "--tag",
            &scope_tag,
        ],
        Duration::from_secs(CLUSTER_BACKUP_TIMEOUT_SECS),
    )
    .await?;
    if !backup.status.success() {
        anyhow::bail!(
            "restic backup failed: {}",
            String::from_utf8_lossy(&backup.stderr).trim()
        );
    }

    newest_snapshot_id(&repo, cfg, &scope_tag)
        .await
        .ok_or_else(|| anyhow::anyhow!("backup completed but no snapshot id could be read"))
        .map(|snapshot_id| (snapshot_id, summarize_services(&services)))
}

async fn rebuild_manifest(services: &[Value]) -> Value {
    let (nodes, node_cpu_millicores, node_memory_bytes) =
        match crate::kubectl::get_json(&["get", "nodes", "-o", "json"]).await {
            Ok(v) => {
                let items = v["items"].as_array().cloned().unwrap_or_default();
                let mut cpu = 0u64;
                let mut mem = 0u64;
                for n in &items {
                    if let Some(c) = n["status"]["allocatable"]["cpu"].as_str() {
                        cpu += crate::routers::apps::parse_cpu_millicores(c);
                    }
                    if let Some(m) = n["status"]["allocatable"]["memory"].as_str() {
                        mem += crate::routers::apps::parse_memory_bytes(m);
                    }
                }
                (items.len(), cpu, mem)
            }
            Err(_) => (0, 0, 0),
        };

    let apps: Vec<Value> = services
        .iter()
        .map(|s| {
            json!({
                "instance_name": s["instance_name"],
                "app_id": s["app_id"],
                "chart_repo": s["chart_repo"],
                "chart_version": s["chart_version"],
                "service_name": s["service_name"],
                "resources": s["resources"],
                "volumes": s["volumes"],
                "backup": s["backup"],
            })
        })
        .collect();

    json!({
        "generated_at": Utc::now().to_rfc3339(),
        "cluster": {
            "nodes": nodes,
            "allocatable_cpu_millicores": node_cpu_millicores,
            "allocatable_memory_bytes": node_memory_bytes,
        },
        "apps": apps,
    })
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

fn liveness_of(s: &BackupSet) -> Liveness {
    s.liveness(&crate::system::hostname(), &IN_FLIGHT)
}

pub(crate) async fn list() -> anyhow::Result<Vec<Value>> {
    let sets = read_sets().await?;
    Ok(sets
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "triggered_by": s.triggered_by,
                "namespace": s.namespace,
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

pub(crate) async fn app_backup_status() -> HashMap<String, (Option<String>, bool)> {
    let Ok(sets) = read_sets().await else {
        return HashMap::new();
    };
    let mut map: HashMap<String, (Option<String>, bool)> = HashMap::new();
    for s in &sets {
        if s.namespace.is_empty() {
            continue;
        }
        let e = map.entry(s.namespace.clone()).or_insert((None, false));
        if s.is_running() && liveness_of(s).is_live() {
            e.1 = true;
        }
        if e.0.is_none() && s.state == SUCCEEDED {
            e.0 = s.finished_at.clone();
        }
    }
    map
}

pub(crate) async fn last_ok_age_hours() -> Option<i64> {
    let sets = read_sets().await.ok()?;
    sets.iter()
        .find(|s| s.state == SUCCEEDED)
        .and_then(|s| s.finished_at.as_deref())
        .and_then(hours_since)
}

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
        let Some(cfg) = read_master_config().await else {
            return Ok(Tick::Idle("backups are not enabled".into()));
        };
        let sets = read_sets().await?;
        let now = Utc::now();

        let mut queued = 0usize;
        for ns in list_managed_namespaces().await? {
            if running_for(&sets, &ns) || queued_for(&sets, &ns).is_some() {
                continue;
            }
            let Some(def) = crate::routers::apps::read_definition_opt(&ns).await else {
                continue;
            };
            if !def.backup.enabled {
                continue;
            }
            let Ok(schedule) = crate::cron::Cron::parse(&def.backup.schedule) else {
                tracing::warn!(
                    "{ns}: backup schedule {:?} is not a valid cron expression — skipping",
                    def.backup.schedule
                );
                continue;
            };
            if !schedule.due(last_ok_for(&sets, &ns), now) {
                continue;
            }
            match start_app(&ns, "schedule").await {
                Ok(id) => {
                    tracing::info!("backup: queued {ns} ({id})");
                    queued += 1;
                }
                Err(e) => tracing::warn!("backup: could not queue {ns}: {e}"),
            }
        }

        if queued_for(&sets, "").is_none() && !running_for(&sets, "") {
            if let Ok(schedule) = crate::cron::Cron::parse(DR_SCHEDULE) {
                if schedule.due(last_ok_for(&sets, ""), now) {
                    match start("schedule-dr").await {
                        Ok(id) => {
                            tracing::info!("backup: queued DR snapshot ({id})");
                            queued += 1;
                        }
                        Err(e) => tracing::warn!("backup: could not queue DR snapshot: {e}"),
                    }
                }
            }
        }

        let promoted = promote_next().await?;
        sweep_staging(&promoted.iter().map(|p| p.id.clone()).collect::<Vec<_>>()).await;
        let started = promoted.is_some();
        if let Some(promoted) = promoted {
            tracing::info!("backup: starting {}", promoted.id);
            spawn_run(cfg, promoted);
        }

        if queued == 0 && !started {
            Ok(Tick::Idle("no app is due for a backup".into()))
        } else {
            Ok(Tick::Done)
        }
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
            namespace: "yolab-a".into(),
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
            claim: Claim {
                owner: "other-node".into(),
                heartbeat: "2026-01-01T00:00:00Z".into(),
            },
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

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn last_ok_is_scoped_to_the_namespace() {
        let mut a = set("a", "succeeded");
        a.namespace = "yolab-a".into();
        a.finished_at = Some("2026-01-01T00:00:00Z".into());
        let mut b = set("b", "succeeded");
        b.namespace = "yolab-b".into();
        b.finished_at = Some("2025-12-01T00:00:00Z".into());
        let sets = vec![a, b];
        assert_eq!(
            last_ok_for(&sets, "yolab-a"),
            Some(at("2026-01-01T00:00:00Z"))
        );
        assert_eq!(
            last_ok_for(&sets, "yolab-b"),
            Some(at("2025-12-01T00:00:00Z"))
        );
        assert_eq!(last_ok_for(&[], "yolab-a"), None);
    }

    #[test]
    fn a_cluster_set_is_not_an_apps_backup() {
        let mut cluster = set("c", "succeeded");
        cluster.namespace = String::new();
        assert_eq!(last_ok_for(&[cluster], "yolab-a"), None);
    }

    #[test]
    fn running_is_scoped_to_the_namespace() {
        assert!(running_for(&[set("a", "running")], "yolab-a"));
        assert!(!running_for(&[set("a", "running")], "yolab-b"));
        assert!(!running_for(&[set("a", "succeeded")], "yolab-a"));
    }

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

    fn queued_at(id: &str, namespace: &str, started_at: &str) -> BackupSet {
        let mut s = set(id, QUEUED);
        s.namespace = namespace.into();
        s.started_at = started_at.into();
        s.finished_at = None;
        s.claim = Claim::default();
        s
    }

    #[test]
    fn a_queued_set_reads_as_waiting_not_as_a_failure() {
        assert_eq!(
            classify(
                &queued_at("a", "yolab-a", "2026-01-01T00:00:00Z"),
                Liveness::Abandoned
            ),
            SetState::Queued,
            "nobody drives a queued set yet — that is not a crash"
        );
        assert_eq!(state_str(SetState::Queued), "queued");
    }

    #[test]
    fn the_oldest_queued_set_goes_first() {
        let sets = vec![
            queued_at("newer", "yolab-b", "2026-01-01T03:00:00Z"),
            queued_at("older", "yolab-a", "2026-01-01T01:00:00Z"),
            queued_at("middle", "yolab-c", "2026-01-01T02:00:00Z"),
        ];
        assert_eq!(next_in_line(&sets).map(|s| s.id.as_str()), Some("older"));
    }

    #[test]
    fn nothing_is_in_line_when_nothing_is_queued() {
        assert!(next_in_line(&[]).is_none());
        assert!(next_in_line(&[set("a", SUCCEEDED), set("b", RUNNING)]).is_none());
    }

    #[test]
    fn a_queued_set_is_found_by_its_namespace_so_it_is_not_queued_twice() {
        let sets = vec![queued_at("a", "yolab-a", "2026-01-01T00:00:00Z")];
        assert_eq!(
            queued_for(&sets, "yolab-a").map(|s| s.id.as_str()),
            Some("a")
        );
        assert!(queued_for(&sets, "yolab-b").is_none());
    }

    #[test]
    fn a_cluster_run_is_queued_under_the_empty_namespace() {
        let sets = vec![queued_at("dr", "", "2026-01-01T00:00:00Z")];
        assert!(queued_for(&sets, "").is_some());
        assert!(queued_for(&sets, "yolab-a").is_none());
    }

    #[test]
    fn a_set_carries_enough_to_know_what_to_back_up() {
        assert_eq!(target_of(&queued_at("dr", "", "t")), BackupTarget::Cluster);
        assert_eq!(
            target_of(&queued_at("a", "yolab-a", "t")),
            BackupTarget::App("yolab-a".into())
        );
    }

    #[test]
    fn every_run_stages_into_its_own_directory() {
        assert_eq!(
            staging_dir("/var/lib/yolab/backup-staging", "bk-1"),
            "/var/lib/yolab/backup-staging/bk-1"
        );
        assert_ne!(
            staging_dir(STAGING_ROOT, "bk-1"),
            staging_dir(STAGING_ROOT, "bk-2"),
            "two runs must never share a staging directory"
        );
    }

    #[test]
    fn leftovers_from_dead_runs_are_swept_and_the_live_one_is_kept() {
        let present = vec!["bk-dead".to_string(), "bk-live".to_string()];
        assert_eq!(
            stale_staging_dirs(&present, &["bk-live".to_string()]),
            vec!["bk-dead".to_string()]
        );
    }

    #[test]
    fn nothing_is_swept_out_from_under_a_running_backup() {
        let present = vec!["bk-live".to_string()];
        assert!(stale_staging_dirs(&present, &["bk-live".to_string()]).is_empty());
    }

    #[test]
    fn with_no_run_in_flight_every_leftover_goes() {
        let present = vec!["bk-1".to_string(), "bk-2".to_string()];
        assert_eq!(stale_staging_dirs(&present, &[]).len(), 2);
    }
}
