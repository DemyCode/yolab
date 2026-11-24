use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backup::common::*;
use crate::error::Outcome;
use crate::host::Host;
use crate::ops::{self, Claim, Claimed, FAILED, QUEUED, RUNNING, SUCCEEDED};
use crate::records::Store;
use crate::routers::apps::{ANN_APP_ID, ANN_CHART_REPO, ANN_CHART_VERSION};
use crate::runtime::{Activity, Ctx, Resource, Scope, Tick};
use chrono::{DateTime, Utc};
use kube::Client;

pub(crate) const SETS: Store = Store {
    name: "yolab-backups",
    namespace: "kube-system",
    key: "sets",
};
const MAX_SETS: usize = 50;

const SCHEDULE_TICK_SECS: u64 = 300;

const CLUSTER_BACKUP_TIMEOUT_SECS: u64 = 3600;
const PRUNE_TIMEOUT_SECS: u64 = 600;

const MAX_ATTEMPTS: u32 = 3;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSummary>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(flatten)]
    pub claim: Claim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackupTarget {
    Cluster,
    App(String),
}

impl BackupTarget {
    fn key(&self) -> String {
        match self {
            BackupTarget::Cluster => String::new(),
            BackupTarget::App(ns) => ns.clone(),
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

async fn read_sets(client: &Client) -> anyhow::Result<Vec<BackupSet>> {
    Ok(SETS.read(client).await?)
}

async fn record_done(client: &Client, id: &str, result: &anyhow::Result<RunDone>) {
    let finished_at = Utc::now().to_rfc3339();
    let written = SETS
        .update(client, |sets: &mut Vec<BackupSet>| {
            let Some(s) = sets.iter_mut().find(|s| s.id == id) else {
                return;
            };
            s.finished_at = Some(finished_at.clone());
            match result {
                Ok(done) => {
                    s.state = SUCCEEDED.to_string();
                    s.snapshot_id = Some(done.snapshot_id.clone());
                    s.error = None;
                    s.warning = done.warning.clone();
                    s.services = done.services.clone();
                }
                Err(e) => {
                    s.state = FAILED.to_string();
                    s.snapshot_id = None;
                    s.error = Some(e.to_string());
                    s.warning = None;
                    s.services = vec![];
                }
            }
        })
        .await;
    written.warn_on_err(format!(
        "backup {id}: could not record the outcome — it will read as crashed"
    ));
}

fn classify(set: &BackupSet) -> SetState {
    match set.state.as_str() {
        SUCCEEDED => SetState::Restorable,
        QUEUED => SetState::Queued,
        RUNNING => SetState::Running,
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
        .any(|s| s.namespace == namespace && s.is_running())
}

pub(crate) fn queued_for<'a>(sets: &'a [BackupSet], namespace: &str) -> Option<&'a BackupSet> {
    sets.iter()
        .find(|s| s.namespace == namespace && s.state == QUEUED)
}

pub(crate) fn next_in_line(sets: &[BackupSet]) -> Option<&BackupSet> {
    sets.iter()
        .filter(|s| s.state == QUEUED && target_of(s).is_some())
        .min_by(|a, b| a.started_at.cmp(&b.started_at))
}

const DR_SCHEDULE: &str = "0 4 * * *";

pub(crate) async fn start<H: Host>(b: &Backend<H>, triggered_by: &str) -> anyhow::Result<String> {
    enqueue(b, BackupTarget::Cluster, triggered_by).await
}

pub(crate) async fn start_app<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    triggered_by: &str,
) -> anyhow::Result<String> {
    enqueue(b, BackupTarget::App(namespace.to_string()), triggered_by).await
}

async fn enqueue<H: Host>(
    b: &Backend<H>,
    target: BackupTarget,
    triggered_by: &str,
) -> anyhow::Result<String> {
    if master_config(&b.kube).await?.is_none() {
        anyhow::bail!("backup not configured");
    }
    if let Some(why) = crate::heal::backups_blocked(&b.host).await {
        anyhow::bail!("not backing up: {why}");
    }

    let namespace = target.key();
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
        warning: None,
        services: vec![],
        attempts: 0,
        claim: Claim::default(),
    };

    let mut existing = None;
    SETS.update(&b.kube, |sets: &mut Vec<BackupSet>| {
        existing = queued_for(sets, &namespace)
            .map(|s| s.id.clone())
            .or_else(|| {
                sets.iter()
                    .find(|s| s.namespace == namespace && s.is_running())
                    .map(|s| s.id.clone())
            });
        if existing.is_none() {
            ops::upsert(sets, queued.clone(), MAX_SETS);
        }
    })
    .await?;

    crate::runtime::wake("backup-scheduler");
    Ok(existing.unwrap_or(id))
}

fn pick(sets: &[BackupSet], silent: impl Fn(&BackupSet) -> Duration) -> ops::Next {
    match ops::next(sets, |_| false, silent) {
        ops::Next::Nothing => match next_in_line(sets) {
            Some(s) => ops::Next::Run(s.id.clone()),
            None => ops::Next::Nothing,
        },
        found => found,
    }
}

fn takeable(s: &BackupSet) -> bool {
    let waiting = s.state == QUEUED && target_of(s).is_some();
    waiting || (s.is_running() && !ops::is_driven(s, ops::silent_for(s)))
}

fn begin_attempt(s: &mut BackupSet) {
    s.state = RUNNING.to_string();
    s.started_at = Utc::now().to_rfc3339();
    s.attempts += 1;
}

async fn run_taken<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    set: &BackupSet,
) -> anyhow::Result<RunDone> {
    if set.attempts > MAX_ATTEMPTS {
        anyhow::bail!("interrupted {} times — stopped", set.attempts - 1);
    }
    let Some(target) = target_of(set) else {
        anyhow::bail!("{} is not a service namespace", set.namespace);
    };
    run_set(b, cfg, &target, &set.id).await
}

async fn drive_next<H: Host>(b: &Backend<H>, cfg: &BackupConfig) -> anyhow::Result<bool> {
    let sets = read_sets(&b.kube).await?;
    let id = match pick(&sets, ops::silent_for) {
        ops::Next::Run(id) => id,
        ops::Next::Busy(_) | ops::Next::Nothing => return Ok(false),
    };
    sweep_staging(std::slice::from_ref(&id)).await;
    let Some(set) = ops::take(SETS, &b.kube, &id, takeable, begin_attempt).await? else {
        return Ok(false);
    };
    tracing::info!("backup: starting {} (attempt {})", set.id, set.attempts);
    let run = run_taken(b, cfg, &set);
    let result = ops::drive::<BackupSet, _>(SETS, &b.kube, &set.id, run).await;
    record_done(&b.kube, &set.id, &result).await;
    crate::runtime::wake("backup-scheduler");
    Ok(true)
}

fn target_of(set: &BackupSet) -> Option<BackupTarget> {
    if set.namespace.is_empty() {
        Some(BackupTarget::Cluster)
    } else if crate::routers::install::is_app_namespace(&set.namespace) {
        Some(BackupTarget::App(set.namespace.clone()))
    } else {
        None
    }
}

pub(crate) struct RunDone {
    snapshot_id: String,
    services: Vec<ServiceSummary>,
    warning: Option<String>,
}

fn incomplete(failures: &[String]) -> anyhow::Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "the service settings were saved, but {} part(s) did not back up, so no older copy was deleted: {}",
        failures.len(),
        failures.join("; ")
    )
}

fn outside_tree_failures(pvcs: &[PvcInfo], in_tree: &[crate::tree::Claimed]) -> Vec<String> {
    pvcs.iter()
        .filter(|p| {
            !in_tree
                .iter()
                .any(|c| c.namespace == p.namespace && c.claim == p.name)
        })
        .map(|p| {
            format!(
                "{} keeps its data the old way, outside the file tree, and is not backed up — uninstall this service and install it again to move it into the tree",
                p.name
            )
        })
        .collect()
}

pub(crate) const MANUAL_TRIGGER: &str = "manual";

pub(crate) fn budget_warning(
    name: &str,
    size: &crate::backup::points::ServiceSize,
) -> Option<String> {
    size.over_budget.then(|| {
        format!(
            "{name}'s backups take {} but its budget is {}: its newest copy alone does not fit, so no copy was deleted. Raise the budget.",
            crate::quantity::human_bytes(size.size_bytes),
            crate::quantity::human_bytes(size.budget_bytes),
        )
    })
}

async fn run_set<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    target: &BackupTarget,
    run_id: &str,
) -> anyhow::Result<RunDone> {
    let BackupTarget::App(namespace) = target else {
        let (snapshot_id, services) =
            snapshot_cluster(b, cfg, &HashMap::new(), target, run_id).await?;
        forget_old_cluster_snapshots(b, cfg).await;
        return Ok(RunDone {
            snapshot_id,
            services,
            warning: None,
        });
    };
    let volumes = crate::tree::volumes(&b.kube).await?;
    let claimed = crate::tree::claimed_in(&volumes, namespace);
    let pvcs: Vec<PvcInfo> = user_pvcs(&b.kube)
        .await?
        .into_iter()
        .filter(|p| &p.namespace == namespace)
        .collect();
    let mut failures = outside_tree_failures(&pvcs, &claimed);
    let mut pinned = HashMap::new();

    let folders = crate::tree::service_folders(&volumes, namespace);
    if !folders.is_empty() {
        match crate::routers::folder_backup::back_up(&b.kube, cfg, namespace, run_id, &folders)
            .await
        {
            Ok(short) => match pin_snapshot(
                &b.host,
                cfg,
                &crate::routers::folder_backup::store(namespace),
                &short,
            )
            .await
            {
                Some(snap) => {
                    for c in &claimed {
                        pinned.insert((namespace.clone(), c.claim.clone()), snap.clone());
                    }
                }
                None => failures.push(format!(
                    "its folders were saved as {short}, but that snapshot could not be read back"
                )),
            },
            Err(e) => failures.push(format!("its folders: {e:#}")),
        }
    }

    let (snapshot_id, services) = snapshot_cluster(b, cfg, &pinned, target, run_id).await?;

    let name = namespace.strip_prefix("yolab-").unwrap_or(namespace);
    incomplete(&failures)?;
    let warning = match crate::backup::points::after_run(b, cfg, namespace, &snapshot_id).await {
        Ok(done) => {
            if done.dropped > 0 {
                tracing::info!(
                    "backup: {namespace} was over its budget; deleted {} copies to fit it",
                    done.dropped
                );
            }
            budget_warning(name, &done.size)
        }
        Err(e) => Some(format!(
            "{name} was backed up, but keeping its backups within budget failed: {e:#}"
        )),
    };

    Ok(RunDone {
        snapshot_id,
        services,
        warning,
    })
}

async fn forget_old_cluster_snapshots<H: Host>(b: &Backend<H>, cfg: &BackupConfig) {
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let out = restic_with(
        &b.host,
        &repo,
        cfg,
        &[
            "forget",
            "--tag",
            "scope:cluster",
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
    match out {
        Ok(o) if o.success => {}
        Ok(o) => tracing::warn!(
            "backup: thinning the nightly cluster snapshots failed, the next night retries: {}",
            o.stderr.trim()
        ),
        Err(e) => tracing::warn!(
            "backup: thinning the nightly cluster snapshots could not run, the next night retries: {e}"
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeSnapshot {
    pub id: String,
    pub time: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CatalogVolume {
    pub name: String,
    #[serde(default = "capacity_not_recorded")]
    pub capacity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

fn capacity_not_recorded() -> String {
    "10Gi".to_string()
}

impl CatalogVolume {
    pub(crate) fn snapshot(&self) -> Option<VolumeSnapshot> {
        Some(VolumeSnapshot {
            id: self.snapshot_id.clone()?,
            time: self.snapshot_time.clone()?,
        })
    }
}

fn parse_pinned(v: &Value) -> Option<VolumeSnapshot> {
    let s = v.as_array()?.first()?;
    Some(VolumeSnapshot {
        id: s["id"].as_str()?.to_string(),
        time: s["time"].as_str()?.to_string(),
    })
}

async fn pin_snapshot<H: Host>(
    host: &H,
    cfg: &BackupConfig,
    repo_path: &str,
    short: &str,
) -> Option<VolumeSnapshot> {
    let repo = cfg.restic_repo(repo_path);
    let out = restic_with(
        host,
        &repo,
        cfg,
        &["snapshots", "--no-lock", "--json", short],
        RESTIC_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.success {
        return None;
    }
    parse_pinned(&serde_json::from_str(&out.stdout).ok()?)
}

type PinnedVolumes = HashMap<(String, String), VolumeSnapshot>;

fn catalog_pvc(
    name: &str,
    capacity: &str,
    pinned: Option<&VolumeSnapshot>,
    in_tree: Option<(&str, &str)>,
) -> CatalogVolume {
    CatalogVolume {
        name: name.to_string(),
        capacity: capacity.to_string(),
        snapshot_id: pinned.map(|s| s.id.clone()),
        snapshot_time: pinned.map(|s| s.time.clone()),
        store: in_tree.map(|(store, _)| store.to_string()),
        folder: in_tree.map(|(_, folder)| folder.to_string()),
    }
}

#[cfg(not(test))]
fn staging_root() -> String {
    "/var/lib/yolab/backup-staging".into()
}

#[cfg(test)]
fn staging_root() -> String {
    crate::testkit::thread_dir("backup-staging")
}

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
    let root = staging_root();
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return;
    };
    let mut present = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            present.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    for name in stale_staging_dirs(&present, keep) {
        tokio::fs::remove_dir_all(staging_dir(&root, &name))
            .await
            .debug_on_err(format!(
                "backup: clear the leftover staging directory {name}"
            ));
    }
}

async fn snapshot_cluster<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    pinned: &PinnedVolumes,
    target: &BackupTarget,
    run_id: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let tmp_dir = staging_dir(&staging_root(), run_id);

    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }

    let result = snapshot_cluster_inner(b, cfg, &tmp_dir, pinned, target, run_id).await;
    tokio::fs::remove_dir_all(&tmp_dir)
        .await
        .debug_on_err("backup: clear the staging directory");
    result
}

const CLUSTER_SCOPED_EXPORT: &[(&str, &str)] = &[
    ("v1", "Namespace"),
    ("apiextensions.k8s.io/v1", "CustomResourceDefinition"),
    ("storage.k8s.io/v1", "StorageClass"),
    ("v1", "PersistentVolume"),
    ("scheduling.k8s.io/v1", "PriorityClass"),
    ("rbac.authorization.k8s.io/v1", "ClusterRole"),
    ("rbac.authorization.k8s.io/v1", "ClusterRoleBinding"),
    ("storage.k8s.io/v1", "VolumeAttachment"),
    ("storage.k8s.io/v1", "CSIDriver"),
    ("storage.k8s.io/v1", "CSINode"),
    (
        "admissionregistration.k8s.io/v1",
        "MutatingWebhookConfiguration",
    ),
    (
        "admissionregistration.k8s.io/v1",
        "ValidatingWebhookConfiguration",
    ),
    ("networking.k8s.io/v1", "IngressClass"),
    ("node.k8s.io/v1", "RuntimeClass"),
];

const NAMESPACED_EXPORT: &[(&str, &str)] = &[
    ("apps/v1", "Deployment"),
    ("apps/v1", "DaemonSet"),
    ("v1", "Service"),
    ("v1", "Secret"),
    ("v1", "ConfigMap"),
];

async fn cluster_scoped_objects(client: &Client) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for (api_version, kind) in CLUSTER_SCOPED_EXPORT {
        match crate::k8s::list(client, api_version, kind, None, &Default::default()).await {
            Ok(found) => items.extend(found),
            Err(e) => tracing::warn!("cluster-backup: export {kind}: {e}"),
        }
    }
    items
}

async fn namespace_objects(client: &Client, ns: &str) -> anyhow::Result<Vec<Value>> {
    let mut items = Vec::new();
    for (api_version, kind) in NAMESPACED_EXPORT {
        items.extend(
            crate::k8s::list(client, api_version, kind, Some(ns), &Default::default()).await?,
        );
    }
    Ok(items)
}

async fn export_cluster_scoped(client: &Client, tmp_dir: &str) -> anyhow::Result<()> {
    let items = cluster_scoped_objects(client).await;
    let sanitized = sanitize_k8s_items_for_backup(&items);
    let list = json!({ "apiVersion": "v1", "kind": "List", "items": sanitized });
    tokio::fs::write(
        format!("{tmp_dir}/cluster-resources.yaml"),
        serde_json::to_string_pretty(&list)?,
    )
    .await?;
    Ok(())
}

#[cfg(not(test))]
fn etcd_snapshot_dir() -> String {
    "/var/lib/rancher/k3s/server/db/snapshots".into()
}

#[cfg(test)]
fn etcd_snapshot_dir() -> String {
    crate::testkit::thread_dir("etcd-snapshots")
}

async fn save_etcd_snapshot<H: Host>(b: &Backend<H>, tmp_dir: &str, snap_name: &str) {
    let saved = b
        .host
        .run_cmd(
            "k3s",
            &["etcd-snapshot", "save", &format!("--name={snap_name}")],
        )
        .await;
    match saved {
        Ok(o) if o.success => {}
        Ok(o) => {
            tracing::warn!("cluster-backup: etcd-snapshot: {}", o.stderr.trim());
            return;
        }
        Err(e) => {
            tracing::warn!("cluster-backup: etcd-snapshot: {e}");
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(etcd_snapshot_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        if !fname_str.starts_with(snap_name) {
            continue;
        }
        let dst = format!("{tmp_dir}/etcd.db");
        if let Err(e) = std::fs::copy(entry.path(), &dst) {
            tracing::warn!("cluster-backup: copy etcd snapshot: {e}");
        } else {
            std::fs::remove_file(entry.path())
                .warn_on_err("cluster-backup: remove the local etcd snapshot copy");
        }
        let record = json!({
            "apiVersion": "k3s.cattle.io/v1",
            "kind": "ETCDSnapshotFile",
            "metadata": { "name": fname_str.as_ref() }
        });
        crate::k8s::delete_if_present(&b.kube, &record)
            .await
            .warn_on_err("cluster-backup: delete the etcdsnapshotfile record");
        break;
    }
}

async fn snapshot_cluster_inner<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    tmp_dir: &str,
    pinned: &PinnedVolumes,
    target: &BackupTarget,
    run_id: &str,
) -> anyhow::Result<(String, Vec<ServiceSummary>)> {
    let in_tree = crate::tree::claimed(&crate::tree::volumes(&b.kube).await?);
    let date = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
    let repo = cfg.restic_repo("cluster-backup");

    let namespaces: Vec<String> = match target {
        BackupTarget::Cluster => managed_namespaces(&b.kube).await?,
        BackupTarget::App(ns) => vec![ns.clone()],
    };
    let include_etcd = target.is_cluster();

    let mut pvcs_by_ns: HashMap<String, Vec<PvcInfo>> = HashMap::new();
    for pvc in user_pvcs(&b.kube).await? {
        pvcs_by_ns
            .entry(pvc.namespace.clone())
            .or_default()
            .push(pvc);
    }

    if include_etcd {
        save_etcd_snapshot(b, tmp_dir, &format!("yolab-cluster-{date}")).await;
        if let Err(e) = export_cluster_scoped(&b.kube, tmp_dir).await {
            tracing::warn!("cluster-backup: cluster-scoped export: {e}");
        }
    }

    let mut services: Vec<Value> = Vec::new();

    for ns in &namespaces {
        let mut items: Vec<Value> = Vec::new();

        let Some(ns_obj) = crate::k8s::get(
            &b.kube,
            &json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
        )
        .await?
        else {
            continue;
        };
        items.push(ns_obj.clone());
        let ns_obj = Some(ns_obj);

        let workloads = namespace_objects(&b.kube, ns).await?;
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

        let pvcs: Vec<CatalogVolume> = pvcs_by_ns
            .get(ns)
            .map(|list| {
                list.iter()
                    .map(|p| {
                        let snap = pinned.get(&(ns.clone(), p.name.clone()));
                        let store = crate::routers::folder_backup::store(ns);
                        let folder = in_tree
                            .iter()
                            .find(|c| &c.namespace == ns && c.claim == p.name)
                            .map(|c| (store.as_str(), c.folder.as_str()));
                        catalog_pvc(&p.name, &p.capacity, snap, folder)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let images = collect_images(&workloads);

        let definition = crate::routers::apps::read_definition_opt(&b.kube, ns)
            .await
            .with_context(|| format!("{ns}: read its settings to back them up"))?;
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
        .map(|p| crate::quantity::bytes(p["capacity"].as_str().unwrap_or("0")))
        .sum();
    let catalog = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "namespaces": namespaces,
        "services": services,
        "total_pvc_bytes": total_pvc_bytes,
        "catalog_version": built_hash(),
    });
    tokio::fs::write(format!("{tmp_dir}/catalog.json"), catalog.to_string()).await?;

    let manifest = rebuild_manifest(&b.kube, &services).await;
    tokio::fs::write(
        format!("{tmp_dir}/manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )
    .await?;

    cfg.unlock(&b.host, "cluster-backup").await;
    let check = restic_with(
        &b.host,
        &repo,
        cfg,
        &["snapshots", "--no-lock"],
        RESTIC_TIMEOUT,
    )
    .await;
    if check.map(|o| !o.success).unwrap_or(true) {
        let init = restic_with(&b.host, &repo, cfg, &["init"], RESTIC_TIMEOUT).await?;
        if !init.success {
            anyhow::bail!("restic init failed: {}", init.stderr.trim());
        }
    }

    let scope_tag = match target {
        BackupTarget::Cluster => "scope:cluster".to_string(),
        BackupTarget::App(ns) => crate::backup::points::namespace_tag(ns),
    };
    let run_tag = crate::routers::folder_backup::run_tag(run_id);
    let backup = restic_with(
        &b.host,
        &repo,
        cfg,
        &[
            "backup",
            tmp_dir,
            "--tag",
            "cluster-backup",
            "--tag",
            &scope_tag,
            "--tag",
            &run_tag,
        ],
        Duration::from_secs(CLUSTER_BACKUP_TIMEOUT_SECS),
    )
    .await?;
    if !backup.success {
        anyhow::bail!("restic backup failed: {}", backup.stderr.trim());
    }

    newest_snapshot_id(&b.host, &repo, cfg, &run_tag)
        .await
        .ok_or_else(|| anyhow::anyhow!("backup completed but no snapshot id could be read"))
        .map(|snapshot_id| (snapshot_id, summarize_services(&services)))
}

async fn rebuild_manifest(client: &Client, services: &[Value]) -> Value {
    use k8s_openapi::api::core::v1::Node;
    let (nodes, node_cpu_millicores, node_memory_bytes) =
        match kube::Api::<Node>::all(client.clone())
            .list(&Default::default())
            .await
        {
            Ok(listed) => {
                let mut cpu = 0u64;
                let mut mem = 0u64;
                for n in &listed.items {
                    let allocatable = n.status.as_ref().and_then(|s| s.allocatable.as_ref());
                    if let Some(c) = allocatable.and_then(|a| a.get("cpu")) {
                        cpu += crate::routers::apps::parse_cpu_millicores(&c.0);
                    }
                    if let Some(m) = allocatable.and_then(|a| a.get("memory")) {
                        mem += crate::quantity::bytes(&m.0);
                    }
                }
                (listed.items.len(), cpu, mem)
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

async fn newest_snapshot_id<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    tag: &str,
) -> Option<String> {
    let out = restic_with(
        host,
        repo,
        cfg,
        &["snapshots", "--no-lock", "--json", "--tag", tag],
        RESTIC_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.success {
        return None;
    }
    let v: Value = serde_json::from_str(&out.stdout).ok()?;
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

#[cfg(not(test))]
fn built_hash_file() -> String {
    "/var/lib/yolab/built-hash".into()
}

#[cfg(test)]
fn built_hash_file() -> String {
    format!("{}/built-hash", crate::testkit::thread_dir("built"))
}

fn built_hash() -> Option<String> {
    std::fs::read_to_string(built_hash_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) async fn list(client: &Client) -> anyhow::Result<Vec<Value>> {
    let sets = read_sets(client).await?;
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
                "warning": s.warning,
                "services": s.services,
                "state": state_str(classify(s)),
                "node": s.claim.owner,
            })
        })
        .collect())
}

pub(crate) async fn app_backup_status(client: &Client) -> HashMap<String, (Option<String>, bool)> {
    let Ok(sets) = read_sets(client).await else {
        return HashMap::new();
    };
    let mut map: HashMap<String, (Option<String>, bool)> = HashMap::new();
    for s in &sets {
        if s.namespace.is_empty() {
            continue;
        }
        let e = map.entry(s.namespace.clone()).or_insert((None, false));
        if s.is_running() {
            e.1 = true;
        }
        if e.0.is_none() && s.state == SUCCEEDED {
            e.0 = s.finished_at.clone();
        }
    }
    map
}

pub struct BackupSchedulerController;

impl Resource for BackupSchedulerController {
    fn name(&self) -> &'static str {
        "backup-scheduler"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(SCHEDULE_TICK_SECS)
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[Activity::Restore]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(60)
    }
    async fn converge(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        schedule(&ctx.tools.backend().await?).await
    }
}

async fn schedule<H: Host + Clone + 'static>(b: &Backend<H>) -> anyhow::Result<Tick> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Tick::Idle("backups are not enabled".into()));
    };
    let sets = read_sets(&b.kube).await?;
    let now = Utc::now();

    let mut queued = 0usize;
    for ns in managed_namespaces(&b.kube).await? {
        if running_for(&sets, &ns) || queued_for(&sets, &ns).is_some() {
            continue;
        }
        let def = match crate::routers::apps::read_definition_opt(&b.kube, &ns).await {
            Ok(Some(def)) => def,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    "{ns}: its settings cannot be read, so its scheduled backup is skipped: {e:#}"
                );
                continue;
            }
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
        match start_app(b, &ns, "schedule").await {
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
                match start(b, "schedule-dr").await {
                    Ok(id) => {
                        tracing::info!("backup: queued DR snapshot ({id})");
                        queued += 1;
                    }
                    Err(e) => tracing::warn!("backup: could not queue DR snapshot: {e}"),
                }
            }
        }
    }

    let started = drive_next(b, &cfg).await?;

    if queued == 0 && !started {
        Ok(Tick::Idle("no app is due for a backup".into()))
    } else {
        Ok(Tick::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_missing_a_part_deletes_no_older_copy() {
        let err = incomplete(&["its folders: timed out".into()]).unwrap_err();
        assert!(
            err.to_string().contains("no older copy was deleted"),
            "{err}"
        );
        assert!(err.to_string().contains("its folders: timed out"), "{err}");
    }

    #[test]
    fn a_complete_backup_goes_on_to_thin_older_copies() {
        assert!(incomplete(&[]).is_ok());
    }

    #[test]
    fn a_volume_outside_the_tree_fails_the_backup_and_says_to_reinstall() {
        let pvcs = vec![
            PvcInfo {
                namespace: "yolab-notes".into(),
                name: "home".into(),
                capacity: "100Ti".into(),
            },
            PvcInfo {
                namespace: "yolab-notes".into(),
                name: "old-data".into(),
                capacity: "5Gi".into(),
            },
        ];
        let in_tree = vec![crate::tree::Claimed {
            namespace: "yolab-notes".into(),
            claim: "home".into(),
            folder: "notes".into(),
        }];
        let failures = outside_tree_failures(&pvcs, &in_tree);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("old-data "), "{}", failures[0]);
        assert!(failures[0].contains("install it again"), "{}", failures[0]);
    }

    #[test]
    fn a_service_whose_volumes_are_all_in_the_tree_has_nothing_outside_it() {
        let pvcs = vec![PvcInfo {
            namespace: "yolab-notes".into(),
            name: "home".into(),
            capacity: "100Ti".into(),
        }];
        let in_tree = vec![crate::tree::Claimed {
            namespace: "yolab-notes".into(),
            claim: "home".into(),
            folder: "notes".into(),
        }];
        assert!(outside_tree_failures(&pvcs, &in_tree).is_empty());
    }

    #[test]
    fn the_cluster_export_no_longer_asks_for_volume_snapshot_classes() {
        assert!(!CLUSTER_SCOPED_EXPORT
            .iter()
            .any(|(api, _)| api.starts_with("snapshot.storage.k8s.io")));
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
            serde_json::to_value(catalog_pvc("data", "5Gi", Some(&snap), None)).unwrap(),
            json!({"name": "data", "capacity": "5Gi", "snapshot_id": "abc",
                   "snapshot_time": "2026-09-14T12:10:05Z"})
        );
        assert_eq!(
            serde_json::to_value(catalog_pvc("data", "5Gi", None, None)).unwrap(),
            json!({"name": "data", "capacity": "5Gi"})
        );
    }

    fn read_back(written: CatalogVolume) -> CatalogVolume {
        serde_json::from_str(&serde_json::to_string(&written).unwrap()).unwrap()
    }

    #[test]
    fn a_restore_reads_back_exactly_the_snapshot_a_backup_recorded() {
        let snap = VolumeSnapshot {
            id: "abc".into(),
            time: "2026-10-12T03:00:00Z".into(),
        };
        assert_eq!(
            read_back(catalog_pvc("data", "5Gi", Some(&snap), None)).snapshot(),
            Some(snap)
        );
        assert_eq!(
            read_back(catalog_pvc("data", "5Gi", None, None)).snapshot(),
            None
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
            warning: None,
            services: vec![],
            attempts: 0,
            claim: Claim {
                owner: "other-node".into(),
                heartbeat: "2026-01-01T00:00:00Z".into(),
            },
        }
    }

    #[test]
    fn a_set_reads_as_what_its_record_says() {
        assert_eq!(classify(&set("a", SUCCEEDED)), SetState::Restorable);
        assert_eq!(classify(&set("a", FAILED)), SetState::Crashed);
        assert_eq!(classify(&set("a", RUNNING)), SetState::Running);
        assert_eq!(classify(&set("a", "weird")), SetState::Crashed);
    }

    const QUIET: Duration = Duration::ZERO;
    const SILENT: Duration = Duration::from_secs(600);

    #[test]
    fn a_backup_that_still_heartbeats_keeps_the_queue_waiting() {
        let sets = vec![
            queued_at("q", "yolab-b", "2026-01-01T01:00:00Z"),
            set("live", RUNNING),
        ];
        assert_eq!(pick(&sets, |_| QUIET), ops::Next::Busy("live".into()));
    }

    #[test]
    fn a_backup_whose_machine_went_quiet_is_run_again_first() {
        let sets = vec![
            queued_at("q", "yolab-b", "2026-01-01T01:00:00Z"),
            set("stopped", RUNNING),
        ];
        assert_eq!(pick(&sets, |_| SILENT), ops::Next::Run("stopped".into()));
    }

    #[test]
    fn with_nothing_running_the_oldest_queued_backup_is_next() {
        let sets = vec![
            queued_at("newer", "yolab-b", "2026-01-01T03:00:00Z"),
            queued_at("older", "yolab-a", "2026-01-01T01:00:00Z"),
            set("done", SUCCEEDED),
        ];
        assert_eq!(pick(&sets, |_| SILENT), ops::Next::Run("older".into()));
        assert_eq!(
            pick(&[set("done", SUCCEEDED)], |_| QUIET),
            ops::Next::Nothing
        );
    }

    #[test]
    fn a_queued_backup_of_a_service_is_takeable_and_a_finished_one_never() {
        assert!(takeable(&queued_at("q", "yolab-a", "t")));
        assert!(!takeable(&queued_at("q", "kube-system", "t")));
        assert!(!takeable(&set("a", SUCCEEDED)));
        assert!(!takeable(&set("a", FAILED)));
    }

    #[test]
    fn each_attempt_counts_and_restarts_the_clock() {
        let mut s = queued_at("q", "yolab-a", "2026-01-01T00:00:00Z");
        begin_attempt(&mut s);
        assert_eq!(s.state, RUNNING);
        assert_eq!(s.attempts, 1);
        assert_ne!(s.started_at, "2026-01-01T00:00:00Z");
    }

    #[tokio::test]
    async fn a_backup_interrupted_too_often_fails_without_running() {
        let mut s = set("a", RUNNING);
        s.attempts = MAX_ATTEMPTS + 1;
        let b = Backend {
            kube: crate::k8s::testing::unreachable(),
            host: crate::host::fake::FakeHost::new(),
        };
        let cfg = BackupConfig {
            access_key_id: "AKID".into(),
            secret_access_key: "SECRET".into(),
            bucket: "bucket-1".into(),
            endpoint: "https://s3.example".into(),
            restic_password: "pw".into(),
        };
        let err = run_taken(&b, &cfg, &s).await.err().unwrap();
        assert!(err.to_string().contains("interrupted"), "{err}");
        assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
    }

    #[test]
    fn a_record_written_before_attempts_were_counted_still_reads() {
        let old: BackupSet = serde_json::from_value(json!({
            "id": "bk-old", "started_at": "t", "state": RUNNING,
            "owner": "node1", "heartbeat": "h"
        }))
        .unwrap();
        assert_eq!(old.attempts, 0);
    }

    #[test]
    fn upsert_replaces_by_id_and_keeps_newest_first() {
        let mut sets = vec![set("a", "running"), set("b", "succeeded")];
        ops::upsert(&mut sets, set("a", "succeeded"), MAX_SETS);
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
        ops::upsert(&mut sets, set("bk-new", "running"), MAX_SETS);
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
            classify(&queued_at("a", "yolab-a", "2026-01-01T00:00:00Z")),
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
        assert_eq!(
            target_of(&queued_at("dr", "", "t")),
            Some(BackupTarget::Cluster)
        );
        assert_eq!(
            target_of(&queued_at("a", "yolab-a", "t")),
            Some(BackupTarget::App("yolab-a".into()))
        );
    }

    #[test]
    fn a_set_for_something_that_is_not_an_app_never_runs_or_blocks_the_queue() {
        let stale = queued_at("f", "kube-system", "2026-10-01T00:00:00Z");
        assert_eq!(target_of(&stale), None);
        let sets = vec![stale, queued_at("a", "yolab-a", "2026-10-02T00:00:00Z")];
        assert_eq!(next_in_line(&sets).map(|s| s.id.as_str()), Some("a"));
    }

    #[test]
    fn every_run_stages_into_its_own_directory() {
        assert_eq!(
            staging_dir("/var/lib/yolab/backup-staging", "bk-1"),
            "/var/lib/yolab/backup-staging/bk-1"
        );
        assert_ne!(
            staging_dir(&staging_root(), "bk-1"),
            staging_dir(&staging_root(), "bk-2"),
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

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list, status};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const CONFIG_PATH: &str = "/api/v1/namespaces/kube-system/secrets/yolab-backup-config";

        fn enabled() -> Value {
            use base64::Engine as _;
            let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
            json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" },
                "data": {
                    "access_key_id": b("AKID"), "secret_access_key": b("SECRET"),
                    "bucket": b("bucket-1"), "endpoint": b("https://s3.example"),
                    "restic_password": b("pw")
                }
            })
        }

        async fn config(server: &MockServer, code: u16, body: Value) {
            Mock::given(method("GET"))
                .and(path(CONFIG_PATH))
                .respond_with(ResponseTemplate::new(code).set_body_json(body))
                .mount(server)
                .await;
        }

        async fn wrote_records(server: &MockServer) -> bool {
            !crate::records::testing::written(server, "yolab-backups")
                .await
                .is_empty()
        }

        #[tokio::test]
        async fn nothing_is_queued_while_backups_are_not_enabled() {
            let (server, kube) = api_server().await;
            config(&server, 404, status(404, "NotFound")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            let err = start_app(&b, "yolab-notes", "user").await.unwrap_err();
            assert!(err.to_string().contains("not configured"), "{err}");
            assert!(!wrote_records(&server).await);
        }

        #[tokio::test]
        async fn an_unreachable_cluster_is_not_reported_as_backups_being_off() {
            let (server, kube) = api_server().await;
            config(&server, 503, status(503, "ServiceUnavailable")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            let err = start(&b, "schedule").await.unwrap_err();
            assert!(!err.to_string().contains("not configured"), "{err}");
            assert!(!wrote_records(&server).await);
        }

        #[tokio::test]
        async fn asking_twice_for_the_same_app_joins_the_queued_backup() {
            let (server, kube) = api_server().await;
            config(&server, 200, enabled()).await;
            crate::records::testing::records(
                &server,
                "yolab-backups",
                json!([{
                    "id": "bk-waiting", "namespace": "yolab-notes",
                    "started_at": "2026-09-28T00:00:00Z", "state": QUEUED,
                    "owner": "", "heartbeat": ""
                }]),
            )
            .await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            assert_eq!(
                start_app(&b, "yolab-notes", "user").await.unwrap(),
                "bk-waiting"
            );
            assert!(!wrote_records(&server).await);
        }

        #[tokio::test]
        async fn a_new_request_is_recorded_as_queued() {
            let (server, kube) = api_server().await;
            config(&server, 200, enabled()).await;
            crate::records::testing::no_records(&server, "yolab-backups").await;
            crate::records::testing::accept_writes(&server).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            let id = start_app(&b, "yolab-notes", "user").await.unwrap();
            let written = crate::records::testing::written(&server, "yolab-backups").await;
            assert_eq!(written.len(), 1);
            assert_eq!(written[0][0]["id"], id.as_str());
            assert_eq!(written[0][0]["namespace"], "yolab-notes");
            assert_eq!(written[0][0]["state"], QUEUED);
        }

        #[tokio::test]
        async fn exported_cluster_objects_carry_their_kind_so_a_restore_can_apply_them() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Namespace",
                    vec![json!({ "metadata": { "name": "yolab-notes" } })],
                )))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/apis/storage.k8s.io/v1/storageclasses"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;

            let items = cluster_scoped_objects(&kube).await;
            let ns = items
                .iter()
                .find(|i| i["metadata"]["name"] == "yolab-notes")
                .expect("the namespace is exported even though another kind failed");
            assert_eq!(ns["apiVersion"], "v1");
            assert_eq!(ns["kind"], "Namespace");
        }

        #[tokio::test]
        async fn an_app_export_keeps_its_daemonsets() {
            let (server, kube) = api_server().await;
            for (p, kind, items) in [
                (
                    "/apis/apps/v1/namespaces/yolab-ai/deployments",
                    "Deployment",
                    vec![],
                ),
                (
                    "/apis/apps/v1/namespaces/yolab-ai/daemonsets",
                    "DaemonSet",
                    vec![
                        json!({ "metadata": { "name": "ollama-nvidia", "namespace": "yolab-ai" } }),
                    ],
                ),
                ("/api/v1/namespaces/yolab-ai/services", "Service", vec![]),
                ("/api/v1/namespaces/yolab-ai/secrets", "Secret", vec![]),
                (
                    "/api/v1/namespaces/yolab-ai/configmaps",
                    "ConfigMap",
                    vec![],
                ),
            ] {
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(ResponseTemplate::new(200).set_body_json(list(kind, items)))
                    .mount(&server)
                    .await;
            }

            let items = namespace_objects(&kube, "yolab-ai").await.unwrap();
            assert!(items
                .iter()
                .any(|i| i["kind"] == "DaemonSet" && i["metadata"]["name"] == "ollama-nvidia"));
        }

        #[tokio::test]
        async fn an_app_export_fails_whole_rather_than_missing_a_kind() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list("Deployment", vec![])))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces/yolab-notes/services"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;

            assert!(namespace_objects(&kube, "yolab-notes").await.is_err());
        }

        #[tokio::test]
        async fn a_failed_etcd_snapshot_touches_nothing_in_the_cluster() {
            let (server, kube) = api_server().await;
            let b = Backend {
                kube,
                host: FakeHost::new().fail("k3s etcd-snapshot save", "etcd is not running"),
            };
            let dir = tempfile::tempdir().unwrap();

            save_etcd_snapshot(&b, &dir.path().to_string_lossy(), "yolab-bk-1").await;
            assert!(b.host.ran("k3s etcd-snapshot save --name=yolab-bk-1"));
            assert!(server.received_requests().await.unwrap().is_empty());
            assert!(!dir.path().join("etcd.db").exists());
        }

        fn cfg() -> BackupConfig {
            BackupConfig {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
                bucket: "bucket-1".into(),
                endpoint: "https://s3.example".into(),
                restic_password: "pw".into(),
            }
        }

        async fn serve(server: &MockServer, p: &str, body: Value) {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(server)
                .await;
        }

        async fn a_service(
            pvcs: Vec<Value>,
        ) -> (MockServer, Client, crate::records::testing::Kept) {
            a_service_whose_settings_answer(pvcs, 404, status(404, "NotFound")).await
        }

        async fn a_service_whose_settings_answer(
            pvcs: Vec<Value>,
            code: u16,
            settings: Value,
        ) -> (MockServer, Client, crate::records::testing::Kept) {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/namespaces/yolab-notes/secrets/yolab-config"))
                .respond_with(ResponseTemplate::new(code).set_body_json(settings))
                .mount(&server)
                .await;
            config(&server, 200, enabled()).await;
            let kept = crate::records::testing::kept(&server, "yolab-backups").await;
            serve(
                &server,
                "/api/v1/namespaces",
                list(
                    "Namespace",
                    vec![json!({ "metadata": { "name": "yolab-notes" } })],
                ),
            )
            .await;
            serve(
                &server,
                "/api/v1/namespaces/yolab-notes",
                json!({
                    "apiVersion": "v1", "kind": "Namespace",
                    "metadata": {
                        "name": "yolab-notes",
                        "annotations": {
                            ANN_APP_ID: "notes",
                            ANN_CHART_REPO: "https://charts.example",
                            ANN_CHART_VERSION: "1.2.3"
                        }
                    }
                }),
            )
            .await;
            serve(
                &server,
                "/api/v1/persistentvolumes",
                list("PersistentVolume", vec![]),
            )
            .await;
            serve(
                &server,
                "/api/v1/persistentvolumeclaims",
                list("PersistentVolumeClaim", pvcs),
            )
            .await;
            serve(
                &server,
                "/apis/apps/v1/namespaces/yolab-notes/deployments",
                list(
                    "Deployment",
                    vec![json!({
                        "metadata": { "name": "notes", "namespace": "yolab-notes" },
                        "spec": { "template": { "spec": {
                            "initContainers": [{ "name": "init", "image": "busybox:1" }],
                            "containers": [
                                { "name": "app", "image": "notes:2" },
                                { "name": "side", "image": "busybox:1" }
                            ]
                        } } }
                    })],
                ),
            )
            .await;
            for (p, kind) in [
                (
                    "/apis/apps/v1/namespaces/yolab-notes/daemonsets",
                    "DaemonSet",
                ),
                ("/api/v1/namespaces/yolab-notes/services", "Service"),
                ("/api/v1/namespaces/yolab-notes/secrets", "Secret"),
                ("/api/v1/namespaces/yolab-notes/configmaps", "ConfigMap"),
            ] {
                serve(&server, p, list(kind, vec![])).await;
            }
            serve(
                &server,
                "/api/v1/nodes",
                list(
                    "Node",
                    vec![json!({
                        "metadata": { "name": "node1" },
                        "status": { "allocatable": { "cpu": "4", "memory": "8Gi" } }
                    })],
                ),
            )
            .await;
            (server, kube, kept)
        }

        fn restic_but(changed: &[(&str, Result<&str, &str>)]) -> FakeHost {
            let defaults = [
                ("restic unlock", ""),
                ("restic snapshots --no-lock", "[]"),
                (
                    "restic snapshots --no-lock --json --tag",
                    r#"[{"id":"older","time":"2026-11-23T00:00:00Z"},{"id":"newest","time":"2026-11-24T00:00:00Z"}]"#,
                ),
                ("restic backup", ""),
                ("restic forget", ""),
                ("k3s etcd-snapshot save", ""),
            ];
            let mut host = FakeHost::new();
            for (prefix, answer) in changed {
                host = match answer {
                    Ok(out) => host.ok(prefix, out),
                    Err(err) => host.fail(prefix, err),
                };
            }
            for (prefix, out) in defaults {
                if !changed.iter().any(|(c, _)| *c == prefix) {
                    host = host.ok(prefix, out);
                }
            }
            host
        }

        fn restic() -> FakeHost {
            restic_but(&[])
        }

        fn only_set(kept: &crate::records::testing::Kept) -> Value {
            let sets = kept.data("sets");
            assert_eq!(sets.as_array().map(Vec::len), Some(1), "{sets}");
            sets[0].clone()
        }

        #[tokio::test]
        async fn the_scheduler_backs_up_the_whole_cluster_every_night() {
            let (server, kube, kept) = a_service(vec![]).await;
            let b = Backend {
                kube: kube.clone(),
                host: restic(),
            };
            assert!(matches!(schedule(&b).await.unwrap(), Tick::Done));
            let set = only_set(&kept);
            assert_eq!(set["namespace"], "");
            assert_eq!(set["triggered_by"], "schedule-dr");
            assert_eq!(set["state"], SUCCEEDED, "{set}");
            assert_eq!(set["snapshot_id"], "newest");
            assert_eq!(set["services"][0]["instance_name"], "notes");
            assert!(b.host.ran("k3s etcd-snapshot save --name=yolab-cluster-"));
            assert!(b.host.ran("restic backup"));
            assert!(b.host.ran("restic forget --tag scope:cluster"));
            assert!(crate::k8s::testing::asked(&server, "/api/v1/nodes").await);

            let shown = list_sets(&kube).await;
            assert_eq!(shown[0]["state"], "succeeded");
            assert!(app_backup_status(&kube).await.is_empty());

            assert!(matches!(schedule(&b).await.unwrap(), Tick::Idle(_)));
        }

        async fn list_sets(kube: &Client) -> Vec<Value> {
            crate::backup::run::list(kube).await.unwrap()
        }

        #[tokio::test]
        async fn a_service_backup_records_its_snapshot_and_shows_as_done() {
            let (_server, kube, kept) = a_service(vec![]).await;
            let b = Backend {
                kube: kube.clone(),
                host: restic(),
            };
            let id = start_app(&b, "yolab-notes", MANUAL_TRIGGER).await.unwrap();
            assert!(drive_next(&b, &cfg()).await.unwrap());
            let set = only_set(&kept);
            assert_eq!(set["id"], id.as_str());
            assert_eq!(set["state"], SUCCEEDED, "{set}");
            assert_eq!(set["snapshot_id"], "newest");
            assert!(!b.host.ran("k3s etcd-snapshot"));
            assert!(!b.host.ran("restic forget --tag scope:cluster"));
            let status = app_backup_status(&kube).await;
            assert!(status["yolab-notes"].0.is_some());
            assert!(!status["yolab-notes"].1);
            assert!(!drive_next(&b, &cfg()).await.unwrap());
        }

        #[tokio::test]
        async fn a_volume_outside_the_tree_fails_the_run_after_the_settings_are_saved() {
            let pvc = json!({
                "metadata": { "name": "data", "namespace": "yolab-notes" },
                "spec": { "resources": { "requests": { "storage": "1Gi" } } }
            });
            let (_server, kube, kept) = a_service(vec![pvc]).await;
            let b = Backend {
                kube,
                host: restic(),
            };
            start_app(&b, "yolab-notes", MANUAL_TRIGGER).await.unwrap();
            assert!(drive_next(&b, &cfg()).await.unwrap());
            let set = only_set(&kept);
            assert_eq!(set["state"], FAILED);
            let error = set["error"].as_str().unwrap();
            assert!(error.contains("data keeps its data the old way"), "{error}");
            assert!(b.host.ran("restic backup"));
        }

        #[tokio::test]
        async fn a_store_restic_cannot_open_is_created_before_the_first_backup() {
            let (_server, kube, kept) = a_service(vec![]).await;
            let host = restic_but(&[(
                "restic snapshots --no-lock",
                Err("Is there a repository at the following location?"),
            )])
            .ok("restic init", "created restic repository");
            let b = Backend { kube, host };
            start_app(&b, "yolab-notes", MANUAL_TRIGGER).await.unwrap();
            assert!(drive_next(&b, &cfg()).await.unwrap());
            assert!(b.host.ran("restic init"));
            assert_eq!(only_set(&kept)["state"], SUCCEEDED);
        }

        async fn failed_with(host: FakeHost) -> String {
            let (_server, kube, kept) = a_service(vec![]).await;
            let b = Backend { kube, host };
            start_app(&b, "yolab-notes", MANUAL_TRIGGER).await.unwrap();
            assert!(drive_next(&b, &cfg()).await.unwrap());
            let set = only_set(&kept);
            assert_eq!(set["state"], FAILED, "{set}");
            set["error"].as_str().unwrap().to_string()
        }

        #[tokio::test]
        async fn every_restic_failure_fails_the_run_with_restics_reason() {
            let init = failed_with(
                restic_but(&[("restic snapshots --no-lock", Err("no repository"))])
                    .fail("restic init", "permission denied"),
            )
            .await;
            assert!(init.starts_with("restic init failed:"), "{init}");
            assert!(init.ends_with("permission denied"), "{init}");
            let backup = failed_with(restic_but(&[("restic backup", Err("bucket is full"))])).await;
            assert!(backup.starts_with("restic backup failed:"), "{backup}");
            let unread = failed_with(restic_but(&[(
                "restic snapshots --no-lock --json --tag",
                Err("timed out"),
            )]))
            .await;
            assert_eq!(unread, "backup completed but no snapshot id could be read");
            let empty = failed_with(restic_but(&[(
                "restic snapshots --no-lock --json --tag",
                Ok("[]"),
            )]))
            .await;
            assert_eq!(empty, "backup completed but no snapshot id could be read");
            let garbled = failed_with(restic_but(&[(
                "restic snapshots --no-lock --json --tag",
                Ok("{"),
            )]))
            .await;
            assert_eq!(garbled, "backup completed but no snapshot id could be read");
        }

        #[tokio::test]
        async fn thinning_the_nightly_snapshots_failing_never_fails_the_backup() {
            for host in [
                restic_but(&[("restic forget", Err("repository is locked"))]),
                restic().unstartable("restic forget"),
            ] {
                let (_server, kube, kept) = a_service(vec![]).await;
                let b = Backend { kube, host };
                start(&b, "schedule-dr").await.unwrap();
                assert!(drive_next(&b, &cfg()).await.unwrap());
                assert_eq!(only_set(&kept)["state"], SUCCEEDED);
            }
        }

        #[tokio::test]
        async fn a_service_the_scheduler_cannot_read_or_that_is_not_due_is_skipped() {
            let (_server, kube, kept) =
                a_service_whose_settings_answer(vec![], 503, status(503, "ServiceUnavailable"))
                    .await;
            let b = Backend {
                kube,
                host: restic(),
            };
            schedule(&b).await.unwrap();
            let set = only_set(&kept);
            assert_eq!(
                set["namespace"], "",
                "only the nightly snapshot was queued: {set}"
            );
        }

        #[tokio::test]
        async fn the_scheduler_is_idle_while_backups_are_off_and_fails_without_a_cluster() {
            let (server, kube) = api_server().await;
            config(&server, 404, status(404, "NotFound")).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };
            assert!(matches!(schedule(&b).await.unwrap(), Tick::Idle(_)));
            let b = Backend {
                kube: crate::k8s::testing::unreachable(),
                host: FakeHost::new(),
            };
            assert!(schedule(&b).await.is_err());
            assert!(app_backup_status(&b.kube).await.is_empty());

            let c = BackupSchedulerController;
            assert_eq!(c.name(), "backup-scheduler");
            assert_eq!(c.scope(), Scope::Cluster);
            assert_eq!(c.interval(), Duration::from_secs(SCHEDULE_TICK_SECS));
            assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
            assert_eq!(c.pauses_during(), &[Activity::Restore]);
            assert_eq!(c.not_before_uptime(), Duration::from_secs(60));
            let ctx = Ctx {
                node: "node1".into(),
                tools: crate::tools::Tools::unreachable(),
            };
            assert!(c.converge(&ctx).await.is_err());
        }

        #[tokio::test]
        async fn leftover_staging_folders_of_dead_runs_are_cleared_before_a_run() {
            let root = staging_root();
            std::fs::create_dir_all(format!("{root}/bk-dead")).unwrap();
            std::fs::write(format!("{root}/stray-file"), "x").unwrap();
            let (_server, kube, _kept) = a_service(vec![]).await;
            let b = Backend {
                kube,
                host: restic(),
            };
            start_app(&b, "yolab-notes", MANUAL_TRIGGER).await.unwrap();
            assert!(drive_next(&b, &cfg()).await.unwrap());
            assert!(!std::path::Path::new(&format!("{root}/bk-dead")).exists());
            assert!(std::path::Path::new(&format!("{root}/stray-file")).exists());
        }

        #[tokio::test]
        async fn a_saved_etcd_snapshot_is_copied_into_the_backup_and_its_record_deleted() {
            let (server, kube) = api_server().await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(&server)
                .await;
            let dir = etcd_snapshot_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(format!("{dir}/other-1"), "x").unwrap();
            std::fs::write(format!("{dir}/yolab-bk-2-node1-123"), "etcd").unwrap();
            let staging = tempfile::tempdir().unwrap();
            let b = Backend {
                kube,
                host: FakeHost::new().ok("k3s etcd-snapshot save", ""),
            };
            save_etcd_snapshot(&b, &staging.path().to_string_lossy(), "yolab-bk-2").await;
            assert_eq!(
                std::fs::read_to_string(staging.path().join("etcd.db")).unwrap(),
                "etcd"
            );
            assert!(!std::path::Path::new(&format!("{dir}/yolab-bk-2-node1-123")).exists());
            assert!(std::path::Path::new(&format!("{dir}/other-1")).exists());
            assert!(crate::k8s::testing::asked(&server, "yolab-bk-2-node1-123").await);

            let b = Backend {
                kube: crate::k8s::testing::unreachable(),
                host: FakeHost::new()
                    .ok("k3s etcd-snapshot save", "")
                    .fail("k3s etcd-snapshot save", "x"),
            };
            std::fs::write(format!("{dir}/yolab-bk-3-node1"), "etcd").unwrap();
            save_etcd_snapshot(&b, "/proc/no-such-dir", "yolab-bk-3").await;
            assert!(std::path::Path::new(&format!("{dir}/yolab-bk-3-node1")).exists());
            save_etcd_snapshot(&b, "/proc/no-such-dir", "yolab-bk-3").await;
        }

        #[test]
        fn the_build_a_backup_was_made_by_is_recorded_when_known() {
            let file = built_hash_file();
            std::fs::create_dir_all(std::path::Path::new(&file).parent().unwrap()).unwrap();
            std::fs::write(&file, "  \n").unwrap();
            assert_eq!(built_hash(), None);
            std::fs::write(&file, "abc123\n").unwrap();
            assert_eq!(built_hash().as_deref(), Some("abc123"));
            std::fs::remove_file(&file).unwrap();
            assert_eq!(built_hash(), None);
        }
    }
}
