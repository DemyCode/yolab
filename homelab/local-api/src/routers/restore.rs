use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::host::RealHost;
use crate::ops::{self, Claim, Claimed, InFlight, Liveness};
use crate::records::Store;
use crate::routers::backup_common::*;
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};
use tokio::process::Command;

pub(crate) const RESTORES: Store = Store {
    name: "yolab-restores",
    namespace: "kube-system",
    key: "sets",
};
const MAX_RESTORES: usize = 50;

const PVC_DELETE_TIMEOUT_SECS: u64 = 180;
const RD_TIMEOUT_SECS: u64 = 3600;
const WATCHDOG_TICK_SECS: u64 = 30;

pub(crate) static RESTORE_IN_FLIGHT: InFlight = InFlight::new();

#[derive(Clone, Serialize, Deserialize)]
struct DeploymentScale {
    name: String,
    replicas: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RestoreSet {
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
    #[serde(flatten)]
    claim: Claim,
}

impl Claimed for RestoreSet {
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

fn upsert(sets: &mut Vec<RestoreSet>, set: RestoreSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_RESTORES);
}

async fn read_sets() -> anyhow::Result<Vec<RestoreSet>> {
    Ok(RESTORES.read(&RealHost).await?)
}

async fn patch_set(id: &str, mut update: impl FnMut(&mut RestoreSet)) -> anyhow::Result<()> {
    RESTORES
        .update(&RealHost, |sets: &mut Vec<RestoreSet>| {
            if let Some(s) = sets.iter_mut().find(|s| s.id == id) {
                update(s);
            }
        })
        .await?;
    Ok(())
}

pub(crate) async fn start(namespace: &str, snapshot_id: Option<String>) -> anyhow::Result<String> {
    let Some(cfg) = read_master_config().await else {
        anyhow::bail!("backup not configured");
    };
    let resolved = resolve_snapshot(&cfg, namespace, snapshot_id).await?;
    let Some(snapshot_id) = resolved else {
        anyhow::bail!("no cluster-backup snapshot to restore from");
    };

    let (id, original, guard) = begin(namespace, &snapshot_id).await?;
    let task_id = id.clone();
    let ns = namespace.to_string();
    tokio::spawn(async move {
        let _guard = guard;
        let result = run_restore(&ns, &snapshot_id, &cfg, &original).await;
        record_done(&task_id, &result).await;
        crate::runtime::wake("restore-watchdog");
    });

    Ok(id)
}

async fn begin(
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<(String, Vec<DeploymentScale>, ops::InFlightGuard)> {
    let scaled_deployments = read_deployment_scales(namespace).await?;

    let id = format!("rs-{}", random_hex(8));
    let guard = RESTORE_IN_FLIGHT.claim(&id);
    let set = RestoreSet {
        id: id.clone(),
        namespace: namespace.to_string(),
        snapshot_id: Some(snapshot_id.to_string()),
        started_at: Utc::now().to_rfc3339(),
        state: "running".to_string(),
        finished_at: None,
        error: None,
        scaled_deployments: scaled_deployments.clone(),
        claim: Claim::mine(Utc::now()),
    };
    RESTORES
        .update(&RealHost, |sets: &mut Vec<RestoreSet>| {
            upsert(sets, set.clone())
        })
        .await?;
    Ok((id, scaled_deployments, guard))
}

async fn record_done(id: &str, result: &anyhow::Result<bool>) {
    let finished_at = Utc::now().to_rfc3339();
    let error = result.as_ref().err().map(|e| e.to_string());
    patch_set(id, |s| {
        s.state = if error.is_none() {
            "succeeded"
        } else {
            "failed"
        }
        .to_string();
        s.finished_at = Some(finished_at.clone());
        s.error = error.clone();
    })
    .await
    .warn_on_err(format!(
        "restore {id}: could not record the outcome — the watchdog will treat it as abandoned"
    ));
}

async fn run_restore(
    namespace: &str,
    snapshot_id: &str,
    cfg: &BackupConfig,
    original: &[DeploymentScale],
) -> anyhow::Result<bool> {
    let result = restore_inner(namespace, snapshot_id, cfg).await;
    if result.is_err() {
        for d in original {
            scale_deployment(namespace, &d.name, d.replicas)
                .await
                .warn_on_err(format!(
                    "restore of {namespace} failed; scale {} back up",
                    d.name
                ));
        }
    }
    result
}

async fn restore_inner(
    namespace: &str,
    snapshot_id: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<bool> {
    crate::kubectl::run(&[
        "scale",
        "deployment",
        "--all",
        "-n",
        namespace,
        "--replicas=0",
    ])
    .await?;

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let catalog = extract_json_file(&repo, cfg, snapshot_id, "catalog.json").await?;
    let ns_yaml = extract_file(&repo, cfg, snapshot_id, &format!("**/{namespace}.yaml")).await?;
    let restore_as_of = snapshot_time(&repo, cfg, snapshot_id).await;

    let pvcs = catalog_pvcs(&catalog, namespace);
    let mut backed_up = Vec::new();
    for pvc in &pvcs {
        let snaps = volume_snapshots(namespace, &pvc.name, cfg).await?;
        match plan_volume(
            &pvc.name,
            pvc.snapshot.as_ref(),
            &snaps,
            restore_as_of.as_deref(),
        ) {
            VolumePlan::NoBackup => tracing::warn!(
                "restore: {namespace}/{}: no backup snapshot — keeping as-is",
                pvc.name
            ),
            VolumePlan::Refuse(why) => anyhow::bail!("{why}"),
            VolumePlan::RestoreAsOf(as_of) => backed_up.push((pvc, as_of)),
        }
    }

    for (pvc, as_of) in &backed_up {
        restore_volume(namespace, &pvc.name, &pvc.capacity, cfg, as_of.as_deref()).await?;
    }

    if let Some(path) = ns_yaml {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            if let Err(e) = kubectl_apply(&String::from_utf8_lossy(&bytes)).await {
                anyhow::bail!("apply {namespace}.yaml: {e}");
            }
        }
    }

    tracing::info!("restore: {namespace} restored from {snapshot_id}");
    Ok(!backed_up.is_empty())
}

#[derive(Debug, PartialEq)]
enum VolumePlan {
    NoBackup,
    Refuse(String),
    RestoreAsOf(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
struct SnapshotEntry {
    id: String,
    time: chrono::DateTime<Utc>,
}

fn plan_volume(
    pvc: &str,
    pinned: Option<&VolumeSnapshotRef>,
    snaps: &[SnapshotEntry],
    cluster_time: Option<&str>,
) -> VolumePlan {
    if snaps.is_empty() {
        return VolumePlan::NoBackup;
    }
    if let Some(p) = pinned {
        return if snaps.iter().any(|s| s.id == p.id) {
            VolumePlan::RestoreAsOf(Some(p.time.clone()))
        } else {
            VolumePlan::Refuse(format!(
                "the backup of {pvc} this restore point took has since been pruned — pick a newer backup"
            ))
        };
    }
    let Some(as_of) = cluster_time
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return VolumePlan::RestoreAsOf(None);
    };
    if snaps.iter().any(|s| s.time <= as_of) {
        VolumePlan::RestoreAsOf(cluster_time.map(str::to_string))
    } else {
        VolumePlan::Refuse(format!(
            "{pvc} has no backup taken before this restore point — pick a later backup"
        ))
    }
}

async fn volume_snapshots(
    namespace: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<Vec<SnapshotEntry>> {
    let repo = cfg.restic_repo(&format!("volsync/{namespace}/{}", canonical_pvc_id(pvc)));
    let out = restic(&repo, cfg, &["snapshots", "--no-lock", "--json"]).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(Vec::new());
        }
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(parse_snapshots(&serde_json::from_slice(&out.stdout)?))
}

fn parse_snapshots(v: &Value) -> Vec<SnapshotEntry> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(SnapshotEntry {
                id: s["id"].as_str()?.to_string(),
                time: chrono::DateTime::parse_from_rfc3339(s["time"].as_str()?)
                    .ok()?
                    .with_timezone(&Utc),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AppVersion {
    pub snapshot_id: String,
    pub time: String,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct BackupVersions {
    pub configured: bool,
    pub apps: BTreeMap<String, Vec<AppVersion>>,
}

pub(crate) async fn backup_versions() -> anyhow::Result<BackupVersions> {
    let Some(cfg) = load_master_config().await? else {
        return Ok(BackupVersions::default());
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let Some(snapshots) = restic_json(
        &repo,
        &cfg,
        &[
            "snapshots",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
        ],
    )
    .await?
    else {
        return Ok(BackupVersions {
            configured: true,
            apps: BTreeMap::new(),
        });
    };
    let found = restic_json(
        &repo,
        &cfg,
        &[
            "find",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
            "*.yaml",
        ],
    )
    .await?
    .unwrap_or(Value::Null);
    Ok(BackupVersions {
        configured: true,
        apps: versions_by_app(&snapshots, &found),
    })
}

async fn restic_json(
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
) -> anyhow::Result<Option<Value>> {
    let out = restic(repo, cfg, args).await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(None);
        }
        anyhow::bail!("restic {}: {}", args.join(" "), stderr.trim());
    }
    Ok(Some(serde_json::from_slice(&out.stdout).map_err(|e| {
        anyhow::anyhow!("restic {}: unreadable output: {e}", args.join(" "))
    })?))
}

fn versions_by_app(snapshots: &Value, found: &Value) -> BTreeMap<String, Vec<AppVersion>> {
    let listed: Vec<(String, chrono::DateTime<Utc>, String)> = snapshots
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let id = s["id"].as_str()?.to_string();
            let raw = s["time"].as_str()?;
            let time = chrono::DateTime::parse_from_rfc3339(raw)
                .ok()?
                .with_timezone(&Utc);
            Some((id, time, raw.to_string()))
        })
        .collect();
    let mut apps: BTreeMap<String, Vec<(chrono::DateTime<Utc>, AppVersion)>> = BTreeMap::new();
    for entry in found.as_array().into_iter().flatten() {
        let Some(named) = entry["snapshot"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some((id, time, raw)) = listed
            .iter()
            .find(|(id, _, _)| id.starts_with(named) || named.starts_with(id.as_str()))
        else {
            continue;
        };
        for m in entry["matches"].as_array().into_iter().flatten() {
            let Some(namespace) = m["path"]
                .as_str()
                .and_then(|p| p.rsplit('/').next())
                .and_then(|file| file.strip_suffix(".yaml"))
                .filter(|ns| !ns.is_empty())
            else {
                continue;
            };
            let versions = apps.entry(namespace.to_string()).or_default();
            if versions.iter().all(|(_, v)| &v.snapshot_id != id) {
                versions.push((
                    *time,
                    AppVersion {
                        snapshot_id: id.clone(),
                        time: raw.clone(),
                    },
                ));
            }
        }
    }
    apps.into_iter()
        .map(|(ns, mut versions)| {
            versions.sort_by_key(|v| std::cmp::Reverse(v.0));
            (ns, versions.into_iter().map(|(_, v)| v).collect())
        })
        .collect()
}

pub(crate) async fn install_from_backup(
    source_namespace: &str,
    snapshot_id: &str,
    target: Option<(&str, serde_json::Map<String, Value>)>,
) -> anyhow::Result<()> {
    let Some(cfg) = load_master_config().await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let catalog = extract_json_file(&repo, &cfg, snapshot_id, "catalog.json").await?;
    let Some(app) = catalog_apps(&catalog)
        .into_iter()
        .find(|a| a.namespace == source_namespace)
    else {
        anyhow::bail!("{source_namespace} is not in backup {snapshot_id}");
    };
    let Some(path) = extract_file(
        &repo,
        &cfg,
        snapshot_id,
        &format!("**/{source_namespace}.yaml"),
    )
    .await?
    else {
        anyhow::bail!("backup {snapshot_id} has no saved settings for {source_namespace}");
    };
    let objects_raw = tokio::fs::read(&path).await?;
    let objects: Value = serde_json::from_slice(&objects_raw)?;

    let (instance_name, config) = match target {
        Some((name, config)) => (name.to_string(), config),
        None => (
            app.instance_name.clone(),
            saved_config(&objects).unwrap_or_default(),
        ),
    };
    let dest_namespace = format!("yolab-{instance_name}");
    let restore_as_of = snapshot_time(&repo, &cfg, snapshot_id).await;

    let mut volumes = Vec::new();
    for pvc in catalog_pvcs(&catalog, source_namespace) {
        let snaps = volume_snapshots(source_namespace, &pvc.name, &cfg).await?;
        match plan_volume(
            &pvc.name,
            pvc.snapshot.as_ref(),
            &snaps,
            restore_as_of.as_deref(),
        ) {
            VolumePlan::NoBackup => tracing::warn!(
                "install {dest_namespace} from backup: {} was never backed up — the chart creates it empty",
                pvc.name
            ),
            VolumePlan::Refuse(why) => anyhow::bail!("{why}"),
            VolumePlan::RestoreAsOf(as_of) => volumes.push((pvc, as_of)),
        }
    }

    let install =
        crate::routers::apps::prepare_install(&app.app_id, &instance_name, &config).await?;
    let (id, _, _guard) = begin(&dest_namespace, snapshot_id).await?;

    let mut filled = Ok(());
    for (pvc, as_of) in &volumes {
        filled = fill_volume(
            &dest_namespace,
            source_namespace,
            &instance_name,
            pvc,
            &cfg,
            as_of.as_deref(),
        )
        .await;
        if filled.is_err() {
            break;
        }
    }
    if let Err(e) = filled {
        crate::kubectl::run(&["delete", "namespace", &dest_namespace, "--wait=false"])
            .await
            .warn_on_err(format!(
                "install {dest_namespace} from backup failed; remove its namespace"
            ));
        let failed = Err(e);
        record_done(&id, &failed).await;
        return failed.map(|_: bool| ());
    }

    let result = async {
        install.run().await?;
        if let Some(reapply) = objects_to_reapply(&objects) {
            kubectl_apply(&reapply.to_string())
                .await
                .map_err(|e| anyhow::anyhow!("apply {source_namespace}.yaml: {e}"))?;
        }
        anyhow::Ok(!volumes.is_empty())
    }
    .await;
    record_done(&id, &result).await;
    result?;
    tracing::info!("install {dest_namespace} from backup {snapshot_id}: done");

    crate::routers::backups::setup_namespace_backup(&dest_namespace).await?;
    Ok(())
}

#[derive(Debug, PartialEq)]
struct CatalogApp {
    namespace: String,
    app_id: String,
    instance_name: String,
}

fn catalog_apps(catalog: &Value) -> Vec<CatalogApp> {
    catalog["services"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let app_id = s["app_id"].as_str().filter(|a| !a.is_empty())?;
            let namespace = s["namespace"].as_str()?;
            Some(CatalogApp {
                namespace: namespace.to_string(),
                app_id: app_id.to_string(),
                instance_name: s["instance_name"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| namespace.trim_start_matches("yolab-").to_string()),
            })
        })
        .collect()
}

fn saved_config(objects: &Value) -> Option<serde_json::Map<String, Value>> {
    use base64::Engine as _;
    let secret = objects["items"].as_array()?.iter().find(|i| {
        i["kind"].as_str() == Some("Secret")
            && i["metadata"]["name"].as_str() == Some("yolab-config")
    })?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(secret["data"]["config.json"].as_str()?)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

pub(crate) async fn definition_from_backup(
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<crate::routers::apps::AppDefinition> {
    let Some(cfg) = load_master_config().await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let catalog = extract_json_file(&repo, &cfg, snapshot_id, "catalog.json").await?;
    let Some(service) = catalog["services"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["namespace"] == namespace))
    else {
        anyhow::bail!("{namespace} is not in backup {snapshot_id}");
    };

    if let Some(def) = service.get("definition").filter(|d| !d.is_null()) {
        if let Ok(parsed) =
            serde_json::from_value::<crate::routers::apps::AppDefinition>(def.clone())
        {
            return Ok(parsed);
        }
    }

    let config =
        match extract_file(&repo, &cfg, snapshot_id, &format!("**/{namespace}.yaml")).await? {
            Some(path) => {
                let raw = tokio::fs::read(&path).await?;
                let objects: Value = serde_json::from_slice(&raw)?;
                saved_config(&objects).unwrap_or_default()
            }
            None => serde_json::Map::new(),
        };
    Ok(crate::routers::apps::AppDefinition {
        schema: crate::routers::apps::DEFINITION_SCHEMA,
        app_id: service["app_id"].as_str().unwrap_or("").to_string(),
        chart_repo: service["chart_repo"].as_str().unwrap_or("").to_string(),
        chart_version: service["chart_version"].as_str().unwrap_or("").to_string(),
        instance_name: namespace.trim_start_matches("yolab-").to_string(),
        service_name: service["service_name"].as_str().unwrap_or("").to_string(),
        config,
        volumes: Vec::new(),
        resources: Default::default(),
        backup: Default::default(),
    })
}

fn keep_for_reinstall(item: &Value) -> bool {
    let kind = item["kind"].as_str().unwrap_or("");
    if matches!(kind, "Deployment" | "StatefulSet" | "DaemonSet" | "Service") {
        return false;
    }
    if kind == "Secret" && item["type"].as_str() == Some("helm.sh/release.v1") {
        return false;
    }
    if kind == "Secret" {
        let name = item["metadata"]["name"].as_str().unwrap_or("");
        if matches!(name, "yolab-config" | "yolab-tunnel-credentials") {
            return false;
        }
    }
    true
}

fn objects_to_reapply(objects: &Value) -> Option<Value> {
    let items: Vec<Value> = objects["items"]
        .as_array()?
        .iter()
        .filter(|i| keep_for_reinstall(i))
        .cloned()
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(json!({ "apiVersion": "v1", "kind": "List", "items": items }))
}

async fn fill_volume(
    namespace: &str,
    source_namespace: &str,
    release: &str,
    pvc: &CatalogPvc,
    cfg: &BackupConfig,
    restore_as_of: Option<&str>,
) -> anyhow::Result<()> {
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": pvc.name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "Helm" },
            "annotations": {
                "meta.helm.sh/release-name": release,
                "meta.helm.sh/release-namespace": namespace,
            }
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": "yolab-cephfs",
            "resources": { "requests": { "storage": pvc.capacity } }
        }
    });
    kubectl_apply(&manifest.to_string()).await?;
    restore_into(namespace, source_namespace, &pvc.name, cfg, restore_as_of).await
}

async fn restore_volume(
    namespace: &str,
    pvc: &str,
    capacity: &str,
    cfg: &BackupConfig,
    restore_as_of: Option<&str>,
) -> anyhow::Result<()> {
    crate::kubectl::run(&[
        "delete",
        "pvc",
        pvc,
        "-n",
        namespace,
        "--wait=false",
        "--ignore-not-found",
    ])
    .await?;
    wait_for_pvc_deleted(namespace, pvc).await?;

    ensure_destination_pvc(pvc, namespace, capacity, "yolab-cephfs", "ReadWriteMany").await?;
    restore_into(namespace, namespace, pvc, cfg, restore_as_of).await
}

async fn restore_into(
    namespace: &str,
    source_namespace: &str,
    pvc: &str,
    cfg: &BackupConfig,
    restore_as_of: Option<&str>,
) -> anyhow::Result<()> {
    let cid = canonical_pvc_id(pvc);
    let pvc_repo = cfg.restic_repo(&format!("volsync/{source_namespace}/{cid}"));
    restic_unlock(
        &pvc_repo,
        &cfg.restic_password,
        &cfg.access_key_id,
        &cfg.secret_access_key,
    )
    .await;
    annotate_ns_privileged_movers(namespace).await;

    ensure_restic_secret_for_repo(namespace, source_namespace, pvc, cfg).await?;
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
    anyhow::bail!(
        "PVC still present after {PVC_DELETE_TIMEOUT_SECS}s — a pod may still be mounting it"
    )
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

async fn read_deployment_scales(ns: &str) -> anyhow::Result<Vec<DeploymentScale>> {
    let v = crate::kubectl::get_json(&["get", "deployments", "-n", ns, "-o", "json"]).await?;
    parse_deployment_scales(&v)
}

fn parse_deployment_scales(v: &Value) -> anyhow::Result<Vec<DeploymentScale>> {
    let items = v["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("kubectl get deployments: no items list"))?;
    items
        .iter()
        .map(|d| -> anyhow::Result<DeploymentScale> {
            let name = d["metadata"]["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("a deployment without a name"))?;
            let replicas = d["spec"]["replicas"].as_u64().unwrap_or(1);
            Ok(DeploymentScale {
                name: name.to_string(),
                replicas: u32::try_from(replicas)?,
            })
        })
        .collect()
}

async fn resolve_snapshot(
    cfg: &BackupConfig,
    namespace: &str,
    requested: Option<String>,
) -> anyhow::Result<Option<String>> {
    if requested.is_some() {
        return Ok(requested);
    }
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let Some(snapshots) = restic_json(
        &repo,
        cfg,
        &[
            "snapshots",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
        ],
    )
    .await?
    else {
        return Ok(None);
    };
    let found = restic_json(
        &repo,
        cfg,
        &[
            "find",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
            "*.yaml",
        ],
    )
    .await?
    .unwrap_or(Value::Null);
    let apps = versions_by_app(&snapshots, &found);
    Ok(apps
        .get(namespace)
        .and_then(|v| v.first())
        .map(|v| v.snapshot_id.clone()))
}

async fn snapshot_time(repo: &str, cfg: &BackupConfig, id: &str) -> Option<String> {
    let out = restic(repo, cfg, &["snapshots", "--no-lock", id, "--json"])
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?.first()?["time"].as_str().map(String::from)
}

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
        &[
            "restore",
            snapshot_id,
            "--target",
            &target,
            "--include",
            pattern,
        ],
    )
    .await?;
    if !out.status.success() {
        tokio::fs::remove_dir_all(&target)
            .await
            .debug_on_err("clean up a failed restic restore");
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

#[derive(Debug, Clone, PartialEq)]
struct VolumeSnapshotRef {
    id: String,
    time: String,
}

#[derive(Debug, Clone, PartialEq)]
struct CatalogPvc {
    name: String,
    capacity: String,
    snapshot: Option<VolumeSnapshotRef>,
}

fn catalog_pvcs(catalog: &Value, namespace: &str) -> Vec<CatalogPvc> {
    catalog["services"]
        .as_array()
        .and_then(|svcs| {
            svcs.iter()
                .find(|s| s["namespace"].as_str() == Some(namespace))
        })
        .and_then(|s| s["pvcs"].as_array())
        .map(|pvcs| {
            pvcs.iter()
                .filter_map(|p| {
                    Some(CatalogPvc {
                        name: p["name"].as_str()?.to_string(),
                        capacity: p["capacity"].as_str().unwrap_or("10Gi").to_string(),
                        snapshot: match (p["snapshot_id"].as_str(), p["snapshot_time"].as_str()) {
                            (Some(id), Some(time)) => Some(VolumeSnapshotRef {
                                id: id.to_string(),
                                time: time.to_string(),
                            }),
                            _ => None,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn liveness_of(s: &RestoreSet) -> Liveness {
    s.liveness(&crate::system::hostname(), &RESTORE_IN_FLIGHT)
}

pub(crate) async fn list() -> anyhow::Result<Vec<Value>> {
    let sets = read_sets().await?;
    Ok(sets
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "namespace": s.namespace,
                "snapshot_id": s.snapshot_id,
                "started_at": s.started_at,
                "finished_at": s.finished_at,
                "error": s.error,
                "state": classify(s, liveness_of(s)),
                "node": s.claim.owner,
            })
        })
        .collect())
}

pub(crate) async fn running_anywhere() -> anyhow::Result<bool> {
    let sets = read_sets().await?;
    Ok(sets
        .iter()
        .any(|s| s.is_running() && liveness_of(s).is_live()))
}

fn classify(s: &RestoreSet, liveness: Liveness) -> &'static str {
    match s.state.as_str() {
        "succeeded" => "succeeded",
        "failed" => "failed",
        _ if liveness.is_live() => "running",
        _ => "failed",
    }
}

fn abandoned(sets: &[RestoreSet], me: &str) -> Vec<RestoreSet> {
    sets.iter()
        .filter(|s| s.is_running() && s.liveness(me, &RESTORE_IN_FLIGHT) == Liveness::Abandoned)
        .cloned()
        .collect()
}

pub struct RestoreWatchdogController;

impl Controller for RestoreWatchdogController {
    fn name(&self) -> &'static str {
        "restore-watchdog"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(WATCHDOG_TICK_SECS)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        let now = Utc::now();
        let crashed = abandoned(&read_sets().await?, &ctx.node);
        if crashed.is_empty() {
            return Ok(Tick::Idle("no abandoned restores".into()));
        }
        for set in crashed {
            let mut claimed_by_us = false;
            let me = ctx.node.clone();
            RESTORES
                .update(&RealHost, |sets: &mut Vec<RestoreSet>| {
                    claimed_by_us = false;
                    if let Some(s) = sets.iter_mut().find(|s| s.id == set.id) {
                        if s.is_running()
                            && s.liveness(&me, &RESTORE_IN_FLIGHT) == Liveness::Abandoned
                        {
                            s.state = "failed".to_string();
                            s.finished_at = Some(now.to_rfc3339());
                            s.error = Some("interrupted — scaled back up".to_string());
                            claimed_by_us = true;
                        }
                    }
                })
                .await?;
            if !claimed_by_us {
                continue;
            }
            tracing::warn!(
                "restore {} ({}) was abandoned by {} — scaling back up",
                set.id,
                set.namespace,
                set.claim.owner
            );
            for d in &set.scaled_deployments {
                scale_deployment(&set.namespace, &d.name, d.replicas)
                    .await
                    .warn_on_err(format!("restore {}: scale {} back up", set.id, d.name));
            }
        }
        Ok(Tick::Done)
    }
}

pub(crate) fn start_heartbeat() {
    ops::spawn_heartbeat::<RestoreSet>(RESTORES, &RESTORE_IN_FLIGHT);
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
            claim: Claim::default(),
        }
    }

    #[test]
    fn a_reinstall_never_overwrites_the_charts_own_objects_or_helms_bookkeeping() {
        let backed_up = json!({"items": [
            {"kind": "Deployment", "metadata": {"name": "gateway"}},
            {"kind": "StatefulSet", "metadata": {"name": "db"}},
            {"kind": "DaemonSet", "metadata": {"name": "d"}},
            {"kind": "Service", "metadata": {"name": "filebrowser"}},
            {"kind": "Secret", "type": "helm.sh/release.v1", "metadata": {"name": "sh.helm.release.v1.filebrowser-yrrx.v1"}},
            {"kind": "Secret", "type": "Opaque", "metadata": {"name": "filebrowser-yrrx-admin"}},
            {"kind": "ConfigMap", "metadata": {"name": "filebrowser-yrrx-caddy"}},
        ]});
        let kept = objects_to_reapply(&backed_up).unwrap();
        let names: Vec<&str> = kept["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["filebrowser-yrrx-admin", "filebrowser-yrrx-caddy"],
            "only what the app generated for itself survives"
        );
        assert_eq!(kept["kind"], "List");
    }

    #[test]
    fn nothing_worth_reapplying_applies_nothing() {
        let only_chart_owned = json!({"items": [
            {"kind": "Deployment", "metadata": {"name": "gateway"}},
            {"kind": "Secret", "type": "helm.sh/release.v1", "metadata": {"name": "sh.helm.release.v1.x.v1"}},
        ]});
        assert!(objects_to_reapply(&only_chart_owned).is_none());
        assert!(objects_to_reapply(&json!({"items": []})).is_none());
    }

    #[test]
    fn succeeded_is_succeeded() {
        assert_eq!(
            classify(&set("a", "succeeded"), Liveness::Abandoned),
            "succeeded"
        );
    }

    #[test]
    fn failed_is_failed() {
        assert_eq!(classify(&set("a", "failed"), Liveness::Driving), "failed");
    }

    #[test]
    fn running_is_running_only_while_someone_drives_it() {
        assert_eq!(classify(&set("a", "running"), Liveness::Driving), "running");
        assert_eq!(classify(&set("a", "running"), Liveness::Remote), "running");
        assert_eq!(
            classify(&set("a", "running"), Liveness::Abandoned),
            "failed"
        );
    }

    #[test]
    fn another_nodes_heartbeating_restore_is_never_abandoned() {
        let now = Utc::now();
        let mut live = set("rs-live", "running");
        live.claim = Claim {
            owner: "node1".into(),
            heartbeat: (now - chrono::Duration::seconds(10)).to_rfc3339(),
        };
        let mut looks_old = set("rs-looks-old", "running");
        looks_old.claim = Claim {
            owner: "node1".into(),
            heartbeat: (now - chrono::Duration::seconds(600)).to_rfc3339(),
        };
        let mut mine_restarted = set("rs-mine", "running");
        mine_restarted.claim = Claim {
            owner: "node2".into(),
            heartbeat: now.to_rfc3339(),
        };
        let done = set("rs-done", "succeeded");
        let found = abandoned(&[live, looks_old, mine_restarted, done], "node2");
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["rs-mine"]);
    }

    #[test]
    fn replica_counts_are_read_exactly_or_not_at_all() {
        let v = json!({"items": [
            {"metadata": {"name": "web"}, "spec": {"replicas": 2}},
            {"metadata": {"name": "worker"}, "spec": {}},
        ]});
        let scales = parse_deployment_scales(&v).unwrap();
        let got: Vec<(&str, u32)> = scales
            .iter()
            .map(|d| (d.name.as_str(), d.replicas))
            .collect();
        assert_eq!(got, vec![("web", 2), ("worker", 1)]);
        assert!(
            parse_deployment_scales(&json!({})).is_err(),
            "not a list is not empty"
        );
        let unnamed = json!({"items": [{"spec": {"replicas": 1}}]});
        assert!(parse_deployment_scales(&unnamed).is_err());
        let absurd =
            json!({"items": [{"metadata": {"name": "x"}, "spec": {"replicas": 5_000_000_000u64}}]});
        assert!(parse_deployment_scales(&absurd).is_err());
    }

    #[test]
    fn upsert_replaces_by_id_and_keeps_newest_first() {
        let mut sets = vec![set("a", "running"), set("b", "succeeded")];
        upsert(&mut sets, set("a", "succeeded"));
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].id, "a");
        assert_eq!(sets[0].state, "succeeded");
    }

    fn t(s: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn snap(id: &str, time: &str) -> SnapshotEntry {
        SnapshotEntry {
            id: id.into(),
            time: t(time),
        }
    }

    fn pin(id: &str, time: &str) -> VolumeSnapshotRef {
        VolumeSnapshotRef {
            id: id.into(),
            time: time.into(),
        }
    }

    #[test]
    fn a_pinned_snapshot_is_restored_at_its_own_exact_time() {
        let snaps = [
            snap("old", "2026-09-13T08:00:00Z"),
            snap("pinned", "2026-09-14T12:10:05.417123456Z"),
            snap("newer", "2026-09-15T09:00:00Z"),
        ];
        assert_eq!(
            plan_volume(
                "data",
                Some(&pin("pinned", "2026-09-14T12:10:05.417123456Z")),
                &snaps,
                Some("2026-09-14T12:10:02Z")
            ),
            VolumePlan::RestoreAsOf(Some("2026-09-14T12:10:05.417123456Z".into())),
            "the pin wins over the cluster snapshot's earlier time"
        );
    }

    #[test]
    fn a_pinned_snapshot_that_was_pruned_refuses_the_restore() {
        let snaps = [snap("other", "2026-09-15T09:00:00Z")];
        match plan_volume(
            "data",
            Some(&pin("gone", "2026-09-14T12:10:05Z")),
            &snaps,
            None,
        ) {
            VolumePlan::Refuse(why) => assert!(why.contains("pruned"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unpinned_volume_whose_only_snapshot_is_too_new_is_refused() {
        let snaps = [snap("a", "2026-09-14T12:10:05Z")];
        match plan_volume(
            "data",
            None,
            &snaps,
            Some("2026-09-14T12:10:02.123456+00:00"),
        ) {
            VolumePlan::Refuse(why) => assert!(why.contains("before this restore point"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unpinned_volume_restores_at_the_cluster_time_when_something_is_old_enough() {
        let snaps = [
            snap("a", "2026-09-14T12:10:05Z"),
            snap("b", "2026-09-14T12:10:02Z"),
        ];
        assert_eq!(
            plan_volume("data", None, &snaps, Some("2026-09-14T12:10:02Z")),
            VolumePlan::RestoreAsOf(Some("2026-09-14T12:10:02Z".into())),
            "exactly at the restore point counts"
        );
        assert_eq!(
            plan_volume("data", None, &snaps, None),
            VolumePlan::RestoreAsOf(None)
        );
        assert_eq!(
            plan_volume("data", None, &snaps, Some("not a time")),
            VolumePlan::RestoreAsOf(None)
        );
    }

    #[test]
    fn a_volume_with_no_snapshots_at_all_has_no_backup_pinned_or_not() {
        assert_eq!(plan_volume("data", None, &[], None), VolumePlan::NoBackup);
        assert_eq!(
            plan_volume("data", Some(&pin("x", "2026-09-14T12:10:05Z")), &[], None),
            VolumePlan::NoBackup
        );
    }

    #[test]
    fn snapshots_are_read_from_restic_json_and_bad_entries_skipped() {
        let v = json!([
            {"id": "a", "time": "2026-09-14T12:10:05.417+00:00"},
            {"id": "b"},
            {"time": "2026-09-14T12:10:05Z"},
            {"id": "c", "time": "yesterday"},
        ]);
        assert_eq!(
            parse_snapshots(&v),
            vec![snap("a", "2026-09-14T12:10:05.417Z")]
        );
        assert!(parse_snapshots(&json!({})).is_empty());
    }

    #[test]
    fn catalog_pvcs_read_names_capacity_and_the_pinned_snapshot() {
        let catalog = json!({"services": [
            {"namespace": "yolab-gitea", "pvcs": [
                {"name": "gitea-data", "capacity": "5Gi",
                 "snapshot_id": "abc", "snapshot_time": "2026-09-14T12:10:05Z"},
                {"name": "gitea-db"},
                {"name": "half-pinned", "snapshot_id": "abc"},
            ]},
            {"namespace": "yolab-other", "pvcs": [{"name": "other-data", "capacity": "5Gi"}]},
        ]});
        let pvcs = catalog_pvcs(&catalog, "yolab-gitea");
        assert_eq!(
            pvcs,
            vec![
                CatalogPvc {
                    name: "gitea-data".into(),
                    capacity: "5Gi".into(),
                    snapshot: Some(pin("abc", "2026-09-14T12:10:05Z")),
                },
                CatalogPvc {
                    name: "gitea-db".into(),
                    capacity: "10Gi".into(),
                    snapshot: None,
                },
                CatalogPvc {
                    name: "half-pinned".into(),
                    capacity: "10Gi".into(),
                    snapshot: None,
                },
            ]
        );
        assert!(catalog_pvcs(&catalog, "yolab-nope").is_empty());
        assert!(catalog_pvcs(&json!({}), "yolab-gitea").is_empty());
    }

    #[test]
    fn only_apps_that_recorded_their_chart_can_be_reinstalled() {
        let catalog = json!({"services": [
            {"namespace": "yolab-fb-pgxw", "app_id": "filebrowser", "instance_name": "fb-pgxw"},
            {"namespace": "yolab-legacy", "app_id": ""},
            {"namespace": "yolab-nameless", "app_id": "gitea"},
            {"app_id": "broken"},
        ]});
        assert_eq!(
            catalog_apps(&catalog),
            vec![
                CatalogApp {
                    namespace: "yolab-fb-pgxw".into(),
                    app_id: "filebrowser".into(),
                    instance_name: "fb-pgxw".into(),
                },
                CatalogApp {
                    namespace: "yolab-nameless".into(),
                    app_id: "gitea".into(),
                    instance_name: "nameless".into(),
                },
            ]
        );
        assert!(catalog_apps(&json!({})).is_empty());
    }

    #[test]
    fn the_saved_config_comes_back_with_its_credentials() {
        use base64::Engine as _;
        let config = json!({"password": "s3cr\"et", "domain": "fb"});
        let encoded = base64::engine::general_purpose::STANDARD.encode(config.to_string());
        let objects = json!({"kind": "List", "items": [
            {"kind": "Secret", "metadata": {"name": "fb-admin"}, "data": {"password": "eA=="}},
            {"kind": "ConfigMap", "metadata": {"name": "yolab-config"}, "data": {}},
            {"kind": "Secret", "metadata": {"name": "yolab-config"}, "data": {"config.json": encoded}},
        ]});
        assert_eq!(
            saved_config(&objects),
            Some(config.as_object().unwrap().clone())
        );
    }

    #[test]
    fn a_missing_or_unreadable_saved_config_is_none() {
        assert_eq!(saved_config(&json!({"items": []})), None);
        let bad = json!({"items": [
            {"kind": "Secret", "metadata": {"name": "yolab-config"}, "data": {"config.json": "!!!"}}
        ]});
        assert_eq!(saved_config(&bad), None);
    }

    #[test]
    fn each_app_lists_the_snapshots_holding_it_newest_first() {
        let snapshots = json!([
            {"id": "aaaa1111", "short_id": "aaaa", "time": "2026-09-10T02:00:00+02:00"},
            {"id": "bbbb2222", "short_id": "bbbb", "time": "2026-09-12T02:00:00Z"},
            {"id": "cccc3333", "short_id": "cccc", "time": "2026-09-11T02:00:00Z"},
            {"id": "broken"},
        ]);
        let found = json!([
            {"snapshot": "aaaa", "matches": [
                {"path": "/var/lib/yolab/backup-staging/yolab-a.yaml"},
            ]},
            {"snapshot": "bbbb2222", "matches": [
                {"path": "/var/lib/yolab/backup-staging/yolab-a.yaml"},
                {"path": "/var/lib/yolab/backup-staging/yolab-b.yaml"},
                {"path": "/var/lib/yolab/backup-staging/yolab-b.yaml"},
            ]},
            {"snapshot": "cccc3333", "matches": []},
            {"snapshot": "dddd", "matches": [{"path": "/x/yolab-ghost.yaml"}]},
            {"snapshot": "cccc", "matches": [{"path": "/x/.yaml"}, {"path": "/x/catalog.json"}]},
        ]);
        let v = versions_by_app(&snapshots, &found);
        let ids =
            |ns: &str| -> Vec<String> { v[ns].iter().map(|x| x.snapshot_id.clone()).collect() };
        assert_eq!(v.keys().collect::<Vec<_>>(), ["yolab-a", "yolab-b"]);
        assert_eq!(
            ids("yolab-a"),
            ["bbbb2222", "aaaa1111"],
            "newest first, full ids"
        );
        assert_eq!(ids("yolab-b"), ["bbbb2222"], "listed once");
        assert_eq!(v["yolab-a"][1].time, "2026-09-10T02:00:00+02:00");
    }

    #[test]
    fn no_snapshots_or_unreadable_find_output_means_no_versions() {
        assert!(versions_by_app(&json!([]), &json!([])).is_empty());
        assert!(versions_by_app(
            &json!([{"id": "a", "time": "2026-09-10T02:00:00Z"}]),
            &Value::Null
        )
        .is_empty());
    }
}
