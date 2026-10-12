use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::host::Host;
use crate::ops::{self, Claim, Claimed, InFlight, Liveness, FAILED, RUNNING, SUCCEEDED};
use crate::poll::Step;
use crate::records::Store;
use crate::routers::backup_common::*;
use crate::routers::restore_folders as rf;
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};
use kube::Client;

pub(crate) const RESTORES: Store = Store {
    name: "yolab-restores",
    namespace: "kube-system",
    key: "sets",
};
const MAX_RESTORES: usize = 50;

const RD_TIMEOUT_SECS: u64 = 3600;
const WATCHDOG_TICK_SECS: u64 = 30;

const RD_POLL: Duration = Duration::from_secs(10);

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
        self.state == RUNNING
    }
    fn claim(&self) -> &Claim {
        &self.claim
    }
    fn claim_mut(&mut self) -> &mut Claim {
        &mut self.claim
    }
}

async fn read_sets(client: &Client) -> anyhow::Result<Vec<RestoreSet>> {
    Ok(RESTORES.read(client).await?)
}

async fn patch_set(
    client: &Client,
    id: &str,
    mut update: impl FnMut(&mut RestoreSet),
) -> anyhow::Result<()> {
    RESTORES
        .update(client, |sets: &mut Vec<RestoreSet>| {
            if let Some(s) = sets.iter_mut().find(|s| s.id == id) {
                update(s);
            }
        })
        .await?;
    Ok(())
}

pub(crate) async fn start<H: Host + 'static>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: Option<String>,
    choices: rf::Choices,
) -> anyhow::Result<String> {
    let Some(cfg) = master_config(&b.kube).await? else {
        anyhow::bail!("backup not configured");
    };
    let resolved = resolve_snapshot(&b.host, &cfg, namespace, snapshot_id).await?;
    let Some(snapshot_id) = resolved else {
        anyhow::bail!("no cluster-backup snapshot to restore from");
    };

    let (id, original, guard) = begin(b, namespace, &snapshot_id).await?;
    let task_id = id.clone();
    let ns = namespace.to_string();
    let b = b.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let result = run_restore(&b, &ns, &snapshot_id, &cfg, &choices, &original).await;
        record_done(&b.kube, &task_id, &result).await;
        crate::runtime::wake("restore-watchdog");
    });

    Ok(id)
}

async fn begin<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<(String, Vec<DeploymentScale>, ops::InFlightGuard)> {
    let scaled_deployments = read_deployment_scales(&b.kube, namespace).await?;

    let id = format!("rs-{}", random_hex(8));
    let guard = RESTORE_IN_FLIGHT.claim(&id);
    let set = RestoreSet {
        id: id.clone(),
        namespace: namespace.to_string(),
        snapshot_id: Some(snapshot_id.to_string()),
        started_at: Utc::now().to_rfc3339(),
        state: RUNNING.to_string(),
        finished_at: None,
        error: None,
        scaled_deployments: scaled_deployments.clone(),
        claim: Claim::mine(Utc::now()),
    };
    RESTORES
        .update(&b.kube, |sets: &mut Vec<RestoreSet>| {
            ops::upsert(sets, set.clone(), MAX_RESTORES)
        })
        .await?;
    Ok((id, scaled_deployments, guard))
}

async fn record_done(client: &Client, id: &str, result: &anyhow::Result<bool>) {
    let finished_at = Utc::now().to_rfc3339();
    let error = result.as_ref().err().map(|e| e.to_string());
    patch_set(client, id, |s| {
        s.state = if error.is_none() { SUCCEEDED } else { FAILED }.to_string();
        s.finished_at = Some(finished_at.clone());
        s.error = error.clone();
    })
    .await
    .warn_on_err(format!(
        "restore {id}: could not record the outcome — the watchdog will treat it as abandoned"
    ));
}

async fn run_restore<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
    cfg: &BackupConfig,
    choices: &rf::Choices,
    original: &[DeploymentScale],
) -> anyhow::Result<bool> {
    let result = restore_inner(b, namespace, snapshot_id, cfg, choices).await;
    if result.is_err() {
        for d in original {
            scale(&b.kube, namespace, &d.name, d.replicas)
                .await
                .warn_on_err(format!(
                    "restore of {namespace} failed; scale {} back up",
                    d.name
                ));
        }
        hold_daemonsets(&b.kube, namespace, false)
            .await
            .warn_on_err(format!(
                "restore of {namespace} failed; release its DaemonSets"
            ));
    }
    result
}

async fn restore_inner<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
    cfg: &BackupConfig,
    choices: &rf::Choices,
) -> anyhow::Result<bool> {
    for d in read_deployment_scales(&b.kube, namespace).await? {
        scale(&b.kube, namespace, &d.name, 0).await?;
    }
    hold_daemonsets(&b.kube, namespace, true).await?;

    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;

    let catalog = extract_json_file(&b.host, &repo, cfg, snapshot_id, "catalog.json").await?;
    let ns_yaml = extract_file(
        &b.host,
        &repo,
        cfg,
        snapshot_id,
        &format!("**/{namespace}.yaml"),
    )
    .await?;
    let restore_as_of = snapshot_time(&b.host, &repo, cfg, snapshot_id).await;

    let pvcs = catalog_pvcs(&catalog, namespace);
    let mut backed_up = Vec::new();
    for pvc in &pvcs {
        let wanted = rf::step(
            true,
            true,
            choices.data.get(&pvc.name).copied(),
            rf::Choice::Replace,
        );
        if wanted == rf::Step::Keep {
            continue;
        }
        let snaps = volume_snapshots(&b.host, namespace, &pvc.name, cfg).await?;
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
    let live = crate::folders::list(&b.kube).await?;
    let folders = plan_folders(
        b,
        cfg,
        rf::saved_folders(&catalog, namespace),
        &live,
        &choices.folders,
    )
    .await?;

    for (pvc, as_of) in &backed_up {
        restore_volume(
            b,
            namespace,
            &pvc.name,
            &pvc.capacity,
            cfg,
            as_of.as_deref(),
        )
        .await?;
    }
    for planned in &folders {
        apply_folder_step(b, cfg, planned).await?;
    }

    if let Some(path) = ns_yaml {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            if let Err(e) =
                crate::k8s::apply_documents(&b.kube, &String::from_utf8_lossy(&bytes)).await
            {
                anyhow::bail!("apply {namespace}.yaml: {e}");
            }
        }
    }
    hold_daemonsets(&b.kube, namespace, false).await?;

    tracing::info!("restore: {namespace} restored from {snapshot_id}");
    Ok(!backed_up.is_empty() || !folders.is_empty())
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

async fn volume_snapshots<H: Host>(
    host: &H,
    namespace: &str,
    pvc: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<Vec<SnapshotEntry>> {
    let repo = cfg.restic_repo(&format!("volsync/{namespace}/{}", pvc));
    let out = restic_with(
        host,
        &repo,
        cfg,
        &["snapshots", "--no-lock", "--json"],
        RESTIC_TIMEOUT,
    )
    .await?;
    if !out.success {
        let stderr = &out.stderr;
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(Vec::new());
        }
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(parse_snapshots(&serde_json::from_str(&out.stdout)?))
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

pub(crate) async fn backup_versions<H: Host>(b: &Backend<H>) -> anyhow::Result<BackupVersions> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(BackupVersions::default());
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let Some(snapshots) = restic_json(
        &b.host,
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
        &b.host,
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

async fn restic_json<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    args: &[&str],
) -> anyhow::Result<Option<Value>> {
    let out = restic_with(host, repo, cfg, args, RESTIC_TIMEOUT).await?;
    if !out.success {
        let stderr = &out.stderr;
        if stderr.contains("unable to open config file") || stderr.contains("does not exist") {
            return Ok(None);
        }
        anyhow::bail!("restic {}: {}", args.join(" "), stderr.trim());
    }
    Ok(Some(serde_json::from_str(&out.stdout).map_err(|e| {
        anyhow::anyhow!("restic {}: unreadable output: {e}", args.join(" "))
    })?))
}

pub(crate) async fn restore_points<H: Host>(
    host: &H,
    cfg: &BackupConfig,
    namespace: &str,
) -> anyhow::Result<Vec<AppVersion>> {
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(host, "cluster-backup").await;
    let Some(snapshots) = restic_json(
        host,
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
        return Ok(Vec::new());
    };
    let pattern = format!("{namespace}.yaml");
    let found = restic_json(
        host,
        &repo,
        cfg,
        &[
            "find",
            "--no-lock",
            "--json",
            "--tag",
            "cluster-backup",
            &pattern,
        ],
    )
    .await?
    .unwrap_or(Value::Null);
    Ok(versions_by_app(&snapshots, &found)
        .remove(namespace)
        .unwrap_or_default())
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
                .filter(|ns| crate::routers::install::is_app_namespace(ns))
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

pub(crate) struct BackupPayload {
    cfg: BackupConfig,
    snapshot_id: String,
    source_namespace: String,
    app_id: String,
    objects: Value,
    volumes: Vec<(CatalogPvc, Result<Option<String>, String>)>,
    folders: Vec<rf::SavedFolder>,
    choices: rf::Choices,
}

pub(crate) async fn backup_payload<H: Host>(
    b: &Backend<H>,
    source_namespace: &str,
    snapshot_id: &str,
    choices: rf::Choices,
) -> anyhow::Result<BackupPayload> {
    crate::routers::install::check_backup_ref(source_namespace, snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(cfg) = master_config(&b.kube).await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let catalog = extract_json_file(&b.host, &repo, &cfg, snapshot_id, "catalog.json").await?;
    let Some(app) = catalog_apps(&catalog)
        .into_iter()
        .find(|a| a.namespace == source_namespace)
    else {
        anyhow::bail!("{source_namespace} is not in backup {snapshot_id}");
    };
    let Some(path) = extract_file(
        &b.host,
        &repo,
        &cfg,
        snapshot_id,
        &format!("**/{source_namespace}.yaml"),
    )
    .await?
    else {
        anyhow::bail!("backup {snapshot_id} has no saved settings for {source_namespace}");
    };
    let objects: Value = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    let restore_as_of = snapshot_time(&b.host, &repo, &cfg, snapshot_id).await;

    let mut volumes = Vec::new();
    for pvc in catalog_pvcs(&catalog, source_namespace) {
        let snaps = volume_snapshots(&b.host, source_namespace, &pvc.name, &cfg).await?;
        match plan_volume(
            &pvc.name,
            pvc.snapshot.as_ref(),
            &snaps,
            restore_as_of.as_deref(),
        ) {
            VolumePlan::NoBackup => tracing::warn!(
                "{source_namespace}/{} was never backed up — the chart creates it empty",
                pvc.name
            ),
            VolumePlan::Refuse(why) => volumes.push((pvc, Err(why))),
            VolumePlan::RestoreAsOf(as_of) => volumes.push((pvc, Ok(as_of))),
        }
    }

    Ok(BackupPayload {
        cfg,
        snapshot_id: snapshot_id.to_string(),
        source_namespace: source_namespace.to_string(),
        app_id: app.app_id,
        objects,
        volumes,
        folders: rf::saved_folders(&catalog, source_namespace),
        choices,
    })
}

impl BackupPayload {
    pub(crate) fn app_id(&self) -> &str {
        &self.app_id
    }

    pub(crate) async fn prepare_folders<H: Host>(
        &self,
        b: &Backend<H>,
        wanted: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<Vec<String>> {
        let live = crate::folders::list(&b.kube).await?;
        let planned = plan_folders(
            b,
            &self.cfg,
            rf::for_wanted(&self.folders, wanted),
            &live,
            &self.choices.folders,
        )
        .await?;
        for p in &planned {
            apply_folder_step(b, &self.cfg, p).await?;
        }
        Ok(planned.into_iter().map(|p| p.folder.title).collect())
    }

    pub(crate) async fn fill_volumes<H: Host>(
        &self,
        b: &Backend<H>,
        dest_namespace: &str,
        release: &str,
        members: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let (id, _, _guard) = begin(b, dest_namespace, &self.snapshot_id).await?;
        let outcome = self
            .fill_volumes_inner(b, dest_namespace, release, members)
            .await;
        let recorded = match &outcome {
            Ok(_) => Ok(!self.volumes.is_empty()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        };
        record_done(&b.kube, &id, &recorded).await;
        outcome
    }

    async fn fill_volumes_inner<H: Host>(
        &self,
        b: &Backend<H>,
        dest_namespace: &str,
        release: &str,
        members: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let kept: std::collections::BTreeSet<String> =
            crate::folders::kept_data(&b.kube, release, members)
                .await?
                .into_iter()
                .map(|k| k.claim)
                .collect();
        let reused =
            crate::folders::reuse_data_folders(&b.kube, dest_namespace, release, members).await?;
        for (pvc, planned) in &self.volumes {
            let replace = match rf::step(
                kept.contains(&pvc.name),
                true,
                self.choices.data.get(&pvc.name).copied(),
                rf::Choice::Use,
            ) {
                rf::Step::Restore => false,
                rf::Step::Replace => true,
                rf::Step::Keep | rf::Step::CreateEmpty => continue,
            };
            let as_of = planned.clone().map_err(|why| anyhow::anyhow!("{why}"))?;
            fill_volume(
                b,
                &self.cfg,
                release,
                pvc,
                &VolumeRestore {
                    namespace: dest_namespace,
                    source_namespace: &self.source_namespace,
                    source_pvc: &pvc.name,
                    dest_pvc: &pvc.name,
                    as_of: as_of.as_deref(),
                    replace,
                },
            )
            .await?;
        }
        Ok(reused)
    }

    pub(crate) async fn reapply(&self, client: &Client) -> anyhow::Result<()> {
        let Some(objects) = objects_to_reapply(&self.objects) else {
            return Ok(());
        };
        crate::k8s::apply_documents(client, &objects.to_string())
            .await
            .map_err(|e| anyhow::anyhow!("apply {}.yaml: {e}", self.source_namespace))
    }
}

#[derive(Debug, PartialEq)]
struct CatalogApp {
    namespace: String,
    app_id: String,
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

pub(crate) async fn definition_from_backup<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<crate::routers::apps::AppDefinition> {
    crate::routers::install::check_backup_ref(namespace, snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(cfg) = master_config(&b.kube).await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let catalog = extract_json_file(&b.host, &repo, &cfg, snapshot_id, "catalog.json").await?;
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

    let config = match extract_file(
        &b.host,
        &repo,
        &cfg,
        snapshot_id,
        &format!("**/{namespace}.yaml"),
    )
    .await?
    {
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
        release: String::new(),
        service_name: service["service_name"].as_str().unwrap_or("").to_string(),
        config,
        volumes: Vec::new(),
        resources: Default::default(),
        backup: Default::default(),
    })
}

pub(crate) async fn kept_from_backup<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<(Option<Vec<u8>>, Option<Value>)> {
    crate::routers::install::check_backup_ref(namespace, snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(cfg) = master_config(&b.kube).await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let Some(path) = extract_file(
        &b.host,
        &repo,
        &cfg,
        snapshot_id,
        &format!("**/{namespace}.yaml"),
    )
    .await?
    else {
        return Ok((None, None));
    };
    let objects: Value = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    Ok((
        crate::saved_chart::in_objects(&objects),
        crate::saved_chart::schema_in_objects(&objects),
    ))
}

fn keep_for_reinstall(item: &Value) -> bool {
    if crate::saved_chart::is_kept_with_app(item) {
        return false;
    }
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

async fn fill_volume<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    release: &str,
    pvc: &CatalogPvc,
    restore: &VolumeRestore<'_>,
) -> anyhow::Result<()> {
    let (namespace, dest_name) = (restore.namespace, restore.dest_pvc);
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": dest_name,
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
    crate::k8s::apply(&b.kube, &manifest).await?;
    restore_into(b, cfg, restore).await
}

async fn restore_volume<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    pvc: &str,
    capacity: &str,
    cfg: &BackupConfig,
    restore_as_of: Option<&str>,
) -> anyhow::Result<()> {
    let claim = crate::k8s::reference("v1", "PersistentVolumeClaim", namespace, pvc);
    if !crate::k8s::exists(&b.kube, &claim).await? {
        crate::k8s::apply(
            &b.kube,
            &destination_pvc(pvc, namespace, capacity, "yolab-cephfs", "ReadWriteMany"),
        )
        .await?;
    }
    restore_into(
        b,
        cfg,
        &VolumeRestore {
            namespace,
            source_namespace: namespace,
            source_pvc: pvc,
            dest_pvc: pvc,
            as_of: restore_as_of,
            replace: true,
        },
    )
    .await
}

pub(crate) async fn restore_plan<H: Host>(
    b: &Backend<H>,
    namespace: &str,
    snapshot_id: &str,
    catalog_dir: &std::path::Path,
) -> anyhow::Result<rf::RestorePlan> {
    crate::routers::install::check_backup_ref(namespace, snapshot_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(cfg) = master_config(&b.kube).await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(&b.host, "cluster-backup").await;
    let catalog = extract_json_file(&b.host, &repo, &cfg, snapshot_id, "catalog.json").await?;
    let pvcs = catalog_pvcs(&catalog, namespace);
    let mut backed_up = Vec::new();
    for pvc in &pvcs {
        let snaps = volume_snapshots(&b.host, namespace, &pvc.name, &cfg).await?;
        backed_up.push(!snaps.is_empty());
    }
    let data: Vec<rf::SavedData<'_>> = pvcs
        .iter()
        .zip(&backed_up)
        .map(|(p, saved)| rf::SavedData {
            claim: &p.name,
            capacity: &p.capacity,
            backed_up: *saved,
        })
        .collect();
    let release = rf::saved_release(&catalog, namespace)
        .unwrap_or_else(|| namespace.trim_start_matches("yolab-").to_string());
    let members = rf::saved_app_id(&catalog, namespace)
        .map(|id| crate::routers::apps::chart_members(&catalog_dir.join(id)))
        .unwrap_or_default();
    let kept: BTreeMap<String, String> = crate::folders::kept_data(&b.kube, &release, &members)
        .await?
        .into_iter()
        .map(|k| (k.claim, k.folder))
        .collect();
    let live = crate::folders::list(&b.kube).await?;
    Ok(rf::plan(
        &rf::saved_folders(&catalog, namespace),
        &data,
        &live,
        &kept,
    ))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedFolder {
    folder: rf::SavedFolder,
    step: rf::Step,
    as_of: Option<String>,
}

async fn plan_folders<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    saved: Vec<rf::SavedFolder>,
    live: &[crate::folders::Folder],
    chosen: &BTreeMap<String, rf::Choice>,
) -> anyhow::Result<Vec<PlannedFolder>> {
    let mut planned = Vec::new();
    for folder in saved {
        let exists = live.iter().any(|l| l.name == folder.name);
        let step = rf::step(
            exists,
            folder.snapshot.is_some(),
            chosen.get(&folder.name).copied(),
            rf::Choice::Use,
        );
        let as_of = match step {
            rf::Step::Keep => continue,
            rf::Step::CreateEmpty => None,
            rf::Step::Restore | rf::Step::Replace => saved_folder_as_of(b, cfg, &folder).await?,
        };
        planned.push(PlannedFolder {
            folder,
            step,
            as_of,
        });
    }
    Ok(planned)
}

async fn saved_folder_as_of<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    folder: &rf::SavedFolder,
) -> anyhow::Result<Option<String>> {
    let claim = crate::folders::claim_name(&folder.name);
    let snaps = volume_snapshots(&b.host, crate::folders::NAMESPACE, &claim, cfg).await?;
    let pinned = folder.snapshot.as_ref().map(|s| VolumeSnapshotRef {
        id: s.id.clone(),
        time: s.time.clone(),
    });
    match plan_volume(&folder.title, pinned.as_ref(), &snaps, None) {
        VolumePlan::RestoreAsOf(as_of) => Ok(as_of),
        VolumePlan::Refuse(why) => anyhow::bail!("{why}"),
        VolumePlan::NoBackup => anyhow::bail!(
            "the backup of the folder {} is no longer in your storage — start it empty instead",
            folder.title
        ),
    }
}

async fn apply_folder_step<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    planned: &PlannedFolder,
) -> anyhow::Result<()> {
    let folder = &planned.folder;
    if matches!(planned.step, rf::Step::Restore | rf::Step::CreateEmpty) {
        crate::folders::create_named(&b.kube, &folder.name, &folder.title, &folder.capacity)
            .await?;
    }
    if !matches!(planned.step, rf::Step::Restore | rf::Step::Replace) {
        return Ok(());
    }
    crate::folders::wait_until_ready(&b.kube, &folder.name).await?;
    let claim = crate::folders::claim_name(&folder.name);
    restore_into(
        b,
        cfg,
        &VolumeRestore {
            namespace: crate::folders::NAMESPACE,
            source_namespace: crate::folders::NAMESPACE,
            source_pvc: &claim,
            dest_pvc: &claim,
            as_of: planned.as_of.as_deref(),
            replace: planned.step == rf::Step::Replace,
        },
    )
    .await
}

fn destination_spec(
    secret_name: &str,
    dest_pvc: &str,
    restore_as_of: Option<&str>,
    replace: bool,
) -> Value {
    let mut spec = json!({
        "repository": secret_name,
        "copyMethod": "Direct",
        "cacheStorageClassName": "yolab-cephfs",
        "destinationPVC": dest_pvc,
        "moverSecurityContext": { "runAsUser": 0, "runAsGroup": 0, "fsGroup": 0 }
    });
    if let Some(t) = restore_as_of {
        spec["restoreAsOf"] = Value::String(t.to_string());
    }
    if replace {
        spec["enableFileDeletion"] = Value::Bool(true);
    }
    spec
}

pub(crate) struct VolumeRestore<'a> {
    pub namespace: &'a str,
    pub source_namespace: &'a str,
    pub source_pvc: &'a str,
    pub dest_pvc: &'a str,
    pub as_of: Option<&'a str>,
    pub replace: bool,
}

pub(crate) async fn restore_into<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    r: &VolumeRestore<'_>,
) -> anyhow::Result<()> {
    let (namespace, source_namespace, source_pvc) = (r.namespace, r.source_namespace, r.source_pvc);
    let cid = source_pvc;
    let pvc_repo = cfg.restic_repo(&format!("volsync/{source_namespace}/{cid}"));
    restic_unlock_with(
        &b.host,
        &pvc_repo,
        &cfg.restic_password,
        &cfg.access_key_id,
        &cfg.secret_access_key,
        false,
    )
    .await;
    allow_privileged_movers(&b.kube, namespace).await;

    restic_secret(&b.kube, namespace, source_namespace, source_pvc, cfg).await?;
    let secret_name = format!("{cid}{RESTIC_SECRET_SUFFIX}");
    let restic_spec = destination_spec(&secret_name, r.dest_pvc, r.as_of, r.replace);
    let dest_name = format!("restore-{cid}");
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
    crate::k8s::apply(&b.kube, &manifest).await?;

    wait_for_rd(&b.kube, namespace, &dest_name).await?;
    drop_replication_destination(&b.kube, &dest_name, namespace).await;
    Ok(())
}

async fn wait_for_rd(client: &Client, namespace: &str, dest_name: &str) -> anyhow::Result<()> {
    let destination = crate::k8s::reference(
        "volsync.backube/v1alpha1",
        "ReplicationDestination",
        namespace,
        dest_name,
    );
    let destination = &destination;
    crate::poll::until(
        Duration::from_secs(RD_TIMEOUT_SECS),
        RD_POLL,
        move || async move {
            let v = crate::k8s::get(client, destination).await.ok().flatten();
            match v
                .as_ref()
                .and_then(|v| v["status"]["latestMoverStatus"]["result"].as_str())
            {
                Some("Successful") => Step::Done(()),
                Some("Failed") => {
                    Step::Failed(anyhow::anyhow!("ReplicationDestination reported failure"))
                }
                _ => Step::Pending(anyhow::anyhow!(
                    "restore timed out after {RD_TIMEOUT_SECS}s"
                )),
            }
        },
    )
    .await
}

async fn read_deployment_scales(client: &Client, ns: &str) -> anyhow::Result<Vec<DeploymentScale>> {
    let items = crate::k8s::list(
        client,
        "apps/v1",
        "Deployment",
        Some(ns),
        &Default::default(),
    )
    .await?;
    parse_deployment_scales(&json!({ "items": items }))
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

async fn resolve_snapshot<H: Host>(
    host: &H,
    cfg: &BackupConfig,
    namespace: &str,
    requested: Option<String>,
) -> anyhow::Result<Option<String>> {
    if requested.is_some() {
        return Ok(requested);
    }
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock(host, "cluster-backup").await;
    let Some(snapshots) = restic_json(
        host,
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
        host,
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

async fn snapshot_time<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    id: &str,
) -> Option<String> {
    let out = restic_with(
        host,
        repo,
        cfg,
        &["snapshots", "--no-lock", id, "--json"],
        RESTIC_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.success {
        return None;
    }
    let v: Value = serde_json::from_str(&out.stdout).ok()?;
    v.as_array()?.first()?["time"].as_str().map(String::from)
}

async fn extract_file<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    snapshot_id: &str,
    pattern: &str,
) -> anyhow::Result<Option<String>> {
    let target = format!("/tmp/yolab-restore-{}", random_hex(8));
    let out = restic_with(
        host,
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
        RESTIC_TIMEOUT,
    )
    .await?;
    if !out.success {
        tokio::fs::remove_dir_all(&target)
            .await
            .debug_on_err("clean up a failed restic restore");
        anyhow::bail!("restic restore failed: {}", out.stderr.trim());
    }
    Ok(first_file_under(std::path::Path::new(&target)).map(|p| p.to_string_lossy().into_owned()))
}

fn first_file_under(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in &entries {
        let path = entry.path();
        if path.is_file() {
            return Some(path);
        }
    }
    entries
        .iter()
        .filter(|e| e.path().is_dir())
        .find_map(|e| first_file_under(&e.path()))
}

async fn extract_json_file<H: Host>(
    host: &H,
    repo: &str,
    cfg: &BackupConfig,
    snapshot_id: &str,
    filename: &str,
) -> anyhow::Result<Value> {
    match extract_file(host, repo, cfg, snapshot_id, &format!("**/{filename}")).await? {
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

pub(crate) async fn list(client: &Client) -> anyhow::Result<Vec<Value>> {
    let sets = read_sets(client).await?;
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

pub(crate) async fn running_anywhere(client: &Client) -> anyhow::Result<bool> {
    let sets = read_sets(client).await?;
    Ok(sets
        .iter()
        .any(|s| s.is_running() && liveness_of(s).is_live()))
}

fn classify(s: &RestoreSet, liveness: Liveness) -> &'static str {
    match s.state.as_str() {
        SUCCEEDED => SUCCEEDED,
        FAILED => FAILED,
        _ if liveness.is_live() => RUNNING,
        _ => FAILED,
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
        scale_back_abandoned(&Backend::real().await?, &ctx.node).await
    }
}

async fn scale_back_abandoned<H: Host>(b: &Backend<H>, node: &str) -> anyhow::Result<Tick> {
    let now = Utc::now();
    let crashed = abandoned(&read_sets(&b.kube).await?, node);
    if crashed.is_empty() {
        return Ok(Tick::Idle("no abandoned restores".into()));
    }
    for set in crashed {
        let mut claimed_by_us = false;
        let me = node.to_string();
        RESTORES
            .update(&b.kube, |sets: &mut Vec<RestoreSet>| {
                claimed_by_us = false;
                if let Some(s) = sets.iter_mut().find(|s| s.id == set.id) {
                    if s.is_running() && s.liveness(&me, &RESTORE_IN_FLIGHT) == Liveness::Abandoned
                    {
                        s.state = FAILED.to_string();
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
            scale(&b.kube, &set.namespace, &d.name, d.replicas)
                .await
                .warn_on_err(format!("restore {}: scale {} back up", set.id, d.name));
        }
        hold_daemonsets(&b.kube, &set.namespace, false)
            .await
            .warn_on_err(format!("restore {}: release its DaemonSets", set.id));
    }
    Ok(Tick::Done)
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
            {"kind": "ConfigMap", "metadata": {"name": "yolab-chart"}},
            {"kind": "ConfigMap", "metadata": {"name": "yolab-schema"}},
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
    fn replacing_a_volume_deletes_what_the_backup_does_not_have_and_filling_one_keeps_it() {
        let replace = destination_spec("data-restic", "data", Some("2026-10-12T03:00:00Z"), true);
        assert_eq!(replace["enableFileDeletion"], true);
        assert_eq!(replace["restoreAsOf"], "2026-10-12T03:00:00Z");
        assert_eq!(replace["destinationPVC"], "data");
        let fill = destination_spec("data-restic", "data", None, false);
        assert!(fill.get("enableFileDeletion").is_none());
        assert!(fill.get("restoreAsOf").is_none());
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
                },
                CatalogApp {
                    namespace: "yolab-nameless".into(),
                    app_id: "gitea".into(),
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

    #[test]
    fn the_first_file_is_found_in_name_order_files_before_folders() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(first_file_under(dir.path()), None);
        std::fs::create_dir_all(dir.path().join("a/deep")).unwrap();
        std::fs::write(dir.path().join("a/deep/catalog.json"), "{}").unwrap();
        assert_eq!(
            first_file_under(dir.path()),
            Some(dir.path().join("a/deep/catalog.json"))
        );
        std::fs::write(dir.path().join("z.yaml"), "").unwrap();
        std::fs::write(dir.path().join("m.yaml"), "").unwrap();
        assert_eq!(
            first_file_under(dir.path()),
            Some(dir.path().join("m.yaml"))
        );
        assert_eq!(first_file_under(&dir.path().join("missing")), None);
    }

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list, status};
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, ResponseTemplate};

        fn cfg() -> BackupConfig {
            BackupConfig {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
                bucket: "bucket-1".into(),
                endpoint: "https://s3.example".into(),
                restic_password: "pw".into(),
            }
        }

        fn deployment(name: &str, replicas: u32) -> Value {
            json!({ "metadata": { "name": name, "namespace": "yolab-notes" }, "spec": { "replicas": replicas } })
        }

        fn daemonsets(names: &[&str]) -> Mock {
            let items = names
                .iter()
                .map(|n| json!({ "metadata": { "name": n, "namespace": "yolab-notes" } }))
                .collect();
            Mock::given(method("GET"))
                .and(path("/apis/apps/v1/namespaces/yolab-notes/daemonsets"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list("DaemonSet", items)))
        }

        fn held(name: &str, hold: bool) -> Mock {
            let value = if hold { json!("true") } else { Value::Null };
            Mock::given(method("PATCH"))
                .and(path(format!(
                    "/apis/apps/v1/namespaces/yolab-notes/daemonsets/{name}"
                )))
                .and(body_partial_json(json!({ "spec": { "template": { "spec": {
                    "nodeSelector": { RESTORE_HOLD_LABEL: value }
                } } } })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "apps/v1", "kind": "DaemonSet",
                    "metadata": { "name": name, "namespace": "yolab-notes" }
                })))
        }

        fn scaled(name: &str, replicas: u32) -> Mock {
            Mock::given(method("PATCH"))
                .and(path(format!(
                    "/apis/apps/v1/namespaces/yolab-notes/deployments/{name}"
                )))
                .and(body_partial_json(
                    json!({ "spec": { "replicas": replicas } }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": { "name": name, "namespace": "yolab-notes" }
                })))
        }

        #[tokio::test]
        async fn a_restore_stops_every_deployment_and_daemonset_before_reading_the_backup() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Deployment",
                    vec![deployment("web", 2), deployment("worker", 1)],
                )))
                .mount(&server)
                .await;
            scaled("web", 0).expect(1).mount(&server).await;
            scaled("worker", 0).expect(1).mount(&server).await;
            daemonsets(&["engine"]).mount(&server).await;
            held("engine", true).expect(1).mount(&server).await;
            let b = Backend {
                kube,
                host: FakeHost::new().fail("restic restore", "repository is locked"),
            };

            assert!(
                restore_inner(&b, "yolab-notes", "snap-1", &cfg(), &rf::Choices::default())
                    .await
                    .is_err()
            );
            assert!(b.host.ran("restic restore snap-1"));
            let deleted = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .any(|r| r.method.as_str() == "DELETE");
            assert!(
                !deleted,
                "no volume is touched when the backup cannot be read"
            );
        }

        #[tokio::test]
        async fn a_restore_that_cannot_stop_the_app_goes_no_further() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/apis/apps/v1/namespaces/yolab-notes/deployments"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            assert!(
                restore_inner(&b, "yolab-notes", "snap-1", &cfg(), &rf::Choices::default())
                    .await
                    .is_err()
            );
            assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
        }

        #[tokio::test]
        async fn a_failed_mover_fails_the_restore() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/apis/volsync.backube/v1alpha1/namespaces/yolab-notes/replicationdestinations/rd"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "metadata": { "name": "rd", "namespace": "yolab-notes" },
                    "status": { "latestMoverStatus": { "result": "Failed" } }
                })))
                .mount(&server)
                .await;
            let err = wait_for_rd(&kube, "yolab-notes", "rd").await.unwrap_err();
            assert!(err.to_string().contains("failure"), "{err}");
        }

        #[tokio::test]
        async fn nothing_to_reapply_sends_nothing() {
            let (server, kube) = api_server().await;
            let payload = BackupPayload {
                app_id: "notes".into(),
                source_namespace: "yolab-notes".into(),
                snapshot_id: "snap-1".into(),
                cfg: cfg(),
                volumes: vec![],
                objects: json!({ "items": [] }),
                folders: vec![],
                choices: rf::Choices::default(),
            };
            payload.reapply(&kube).await.unwrap();
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_abandoned_restore_scales_the_app_back_to_what_it_was() {
            let (server, kube) = api_server().await;
            scaled("web", 3).expect(1).mount(&server).await;
            daemonsets(&["engine"]).mount(&server).await;
            held("engine", false).expect(1).mount(&server).await;
            let running = json!([{
                "id": "rs-1", "namespace": "yolab-notes", "started_at": "2026-09-28T00:00:00Z",
                "state": "running", "scaled_deployments": [{ "name": "web", "replicas": 3 }],
                "owner": "node1", "heartbeat": "2026-09-28T00:00:00Z"
            }]);
            crate::records::testing::records(&server, "yolab-restores", running).await;
            crate::records::testing::accept_writes(&server).await;
            let b = Backend {
                kube,
                host: FakeHost::new(),
            };

            assert!(matches!(
                scale_back_abandoned(&b, "node1").await.unwrap(),
                Tick::Done
            ));
            let marked = crate::records::testing::written(&server, "yolab-restores").await;
            assert_eq!(marked[0][0]["state"], "failed");
            assert_eq!(marked[0][0]["error"], "interrupted — scaled back up");
        }
    }
}
