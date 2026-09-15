//! Per-app restore — the replacement for the whole-cluster RestoreRun.
//!
//! Restoring one application is: scale its deployments to zero, recreate its PVCs
//! from their VolSync restic repos, re-apply the app's backed-up K8s objects, and
//! scale back up. The app's data and config are both taken from a chosen
//! `cluster-backup` snapshot (the id the history picker selects).
//!
//! A restore is recorded in a ConfigMap with the same three-state model as a
//! backup: *running*, *succeeded*, or *failed*. "Running" is decided by the
//! record's `Claim` (see `ops`): the node driving it heartbeats while it works,
//! so every node — not just the one that started it — can tell a live restore
//! from one whose process died. The cluster-scoped watchdog scales an ABANDONED
//! restore's app back up, and nothing else.
//!
//! This used to key "running" off a per-process `static` list, and the watchdog
//! ran on every node: node2 saw node1's live restore as crashed and scaled the
//! app back up in the middle of node1 replacing its volumes.

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

/// How long to wait for a PVC to actually disappear before failing the volume.
const PVC_DELETE_TIMEOUT_SECS: u64 = 180;
/// How long to wait for a VolSync ReplicationDestination to finish pulling a volume.
const RD_TIMEOUT_SECS: u64 = 3600;
const WATCHDOG_TICK_SECS: u64 = 30;

/// Ids this process is actively restoring.
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

// ── ConfigMap records ──────────────────────────────────────────────────────────

fn upsert(sets: &mut Vec<RestoreSet>, set: RestoreSet) {
    sets.retain(|s| s.id != set.id);
    sets.insert(0, set);
    sets.truncate(MAX_RESTORES);
}

async fn read_sets() -> anyhow::Result<Vec<RestoreSet>> {
    Ok(RESTORES.read(&RealHost).await?)
}

/// Compare-and-swap update of one record. `update` may run more than once.
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

// ── The operation ──────────────────────────────────────────────────────────────

/// Starts restoring one app and returns immediately. The work runs detached; the
/// watchdog catches a crash and scales the app back up.
pub(crate) async fn start(namespace: &str, snapshot_id: Option<String>) -> anyhow::Result<String> {
    let Some(cfg) = read_master_config().await else {
        anyhow::bail!("backup not configured");
    };
    match crate::heal::heal_running().await {
        Ok(false) => {}
        Ok(true) => anyhow::bail!(
            "the cluster is being healed — add apps back from backup once it finishes"
        ),
        Err(e) => anyhow::bail!("cannot tell whether the cluster is being healed: {e:#}"),
    }

    // Resolve the snapshot up front so the record (and the page) always shows the
    // concrete id being restored, even for "restore latest".
    let resolved = resolve_snapshot(&cfg, snapshot_id).await?;
    let Some(snapshot_id) = resolved else {
        anyhow::bail!("no cluster-backup snapshot to restore from");
    };

    let (id, original, guard) = begin(namespace, &snapshot_id).await?;
    let task_id = id.clone();
    let ns = namespace.to_string();
    tokio::spawn(async move {
        // Held for the whole run and dropped even if the task panics, so the
        // claim can never outlive the work.
        let _guard = guard;
        let result = run_restore(&ns, &snapshot_id, &cfg, &original).await;
        record_done(&task_id, &result).await;
        crate::runtime::wake("restore-watchdog");
    });

    Ok(id)
}

/// Records a restore as running and claims it for this process.
///
/// Fails — before touching the app — when the record cannot be written: a
/// restore with no record is a restore the watchdog can never bring back up.
async fn begin(
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<(String, Vec<DeploymentScale>, ops::InFlightGuard)> {
    // Record the live replica counts BEFORE scaling down, so a crash mid-restore can
    // still bring the app back to a running state.
    let scaled_deployments = read_deployment_scales(namespace).await?;

    let id = format!("rs-{}", random_hex(8));
    // Claimed before the record exists, so this node's watchdog never sees the
    // new record without the claim.
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

/// The actual restore, guarded so a failure always brings the app back up. The
/// inner work scales the app to zero and re-applies its config at the end; if any
/// step before that fails, the app must not be left dark.
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

/// Ok(true) when at least one volume was restored from backup.
async fn restore_inner(
    namespace: &str,
    snapshot_id: &str,
    cfg: &BackupConfig,
) -> anyhow::Result<bool> {
    // 1. Scale the app down so its pods release the PVCs being replaced. `?`: if
    //    this did not happen, the pods still hold the volumes about to be deleted,
    //    and nothing has been touched yet — stop here.
    crate::kubectl::run(&[
        "scale",
        "deployment",
        "--all",
        "-n",
        namespace,
        "--replicas=0",
    ])
    .await?;

    // 2. Pull the app's config + catalog from the chosen snapshot.
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;

    let catalog = extract_json_file(&repo, cfg, snapshot_id, "catalog.json").await?;
    let ns_yaml = extract_file(&repo, cfg, snapshot_id, &format!("**/{namespace}.yaml")).await?;
    let restore_as_of = snapshot_time(&repo, cfg, snapshot_id).await;

    // 3. Check every volume BEFORE replacing any. VolSync restores the newest snapshot
    //    no newer than `restoreAsOf`, and when none qualifies it logs "No eligible
    //    snapshots found" and exits successfully — so an unchecked restore deleted the
    //    live volume and reported success over an empty one.
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

    // 4. Recreate each PVC from its own VolSync restic repo.
    for (pvc, as_of) in &backed_up {
        restore_volume(namespace, &pvc.name, &pvc.capacity, cfg, as_of.as_deref()).await?;
    }

    // 5. Re-apply the app's backed-up objects (deploy/secret/configmap/etc.), which
    //    restores its config and brings it back up at its recorded replica counts.
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

/// What to do with one volume of a restore.
#[derive(Debug, PartialEq)]
enum VolumePlan {
    /// Nothing of it was ever backed up: leave it as it is.
    NoBackup,
    /// Refuse the whole restore before anything is replaced.
    Refuse(String),
    /// Hand VolSync this `restoreAsOf`.
    RestoreAsOf(Option<String>),
}

/// A snapshot as restic lists it.
#[derive(Debug, Clone, PartialEq)]
struct SnapshotEntry {
    id: String,
    time: chrono::DateTime<Utc>,
}

/// VolSync restores the newest snapshot no newer than `restoreAsOf`, and when none
/// qualifies it logs "No eligible snapshots found" and exits successfully. So:
///
///   * a volume the backup pinned is restored at that snapshot's own time — which
///     selects exactly it — provided it still exists (retention may have pruned it);
///   * a volume from an older backup, with nothing pinned, is restored at the
///     cluster snapshot's time, and only if some snapshot is old enough.
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

// ── Reinstalling from backup ───────────────────────────────────────────────────

/// One point in time an app can be reinstalled from: a `cluster-backup` snapshot
/// that holds the app's saved objects.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AppVersion {
    pub snapshot_id: String,
    pub time: String,
}

/// What the backups hold, app by app.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BackupVersions {
    /// Whether backups were ever enabled.
    pub configured: bool,
    /// Every backed-up app by namespace, with its versions newest first.
    pub apps: BTreeMap<String, Vec<AppVersion>>,
}

/// Every app in any backup, and every point in time it can go back to.
///
/// Two restic calls whatever the number of snapshots: the list of snapshots, and
/// one `find` across all of them for the per-app files a backup writes
/// (`<namespace>.yaml`, see `backup::snapshot_cluster_inner`).
///
/// An unreadable backup config or repository is an error, never an empty
/// answer: "nothing is backed up" on a recovery screen reads as "every app is
/// gone", and would be the reason someone clicks through.
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

/// `Ok(None)` when the repository was never created — backups enabled, none
/// taken yet.
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

/// Joins `restic snapshots --json` with `restic find --json` into each app's
/// versions, newest first.
///
/// `find` names a snapshot by its short id in some restic versions and by its
/// full id in others, so the two are matched by prefix either way round. A find
/// entry with no matches, or naming a snapshot the list does not have, adds
/// nothing.
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
            versions.sort_by(|a, b| b.0.cmp(&a.0));
            (ns, versions.into_iter().map(|(_, v)| v).collect())
        })
        .collect()
}

/// Install an app that is not on this machine from one backup: its chart with
/// the settings it had then, then its volumes as that backup pinned them, then
/// its saved objects.
pub(crate) async fn reinstall_from_backup(
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<()> {
    let Some(cfg) = load_master_config().await? else {
        anyhow::bail!("backup not configured");
    };
    let repo = cfg.restic_repo("cluster-backup");
    cfg.unlock("cluster-backup").await;
    let catalog = extract_json_file(&repo, &cfg, snapshot_id, "catalog.json").await?;
    let Some(app) = catalog_apps(&catalog)
        .into_iter()
        .find(|a| a.namespace == namespace)
    else {
        anyhow::bail!("{namespace} is not in backup {snapshot_id}");
    };
    let Some(path) =
        extract_file(&repo, &cfg, snapshot_id, &format!("**/{namespace}.yaml")).await?
    else {
        anyhow::bail!("backup {snapshot_id} has no saved settings for {namespace}");
    };
    let objects: Value = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    let config = saved_config(&objects).unwrap_or_default();

    crate::routers::apps::install_now(&app.app_id, &app.instance_name, &config).await?;

    let (id, original, _guard) = begin(namespace, snapshot_id).await?;
    let result = run_restore(namespace, snapshot_id, &cfg, &original).await;
    record_done(&id, &result).await;
    result?;

    // Only now: wiring backups up earlier would have uploaded the empty volume the
    // chart created, as the newest copy of this app.
    crate::routers::backups::setup_namespace_backup(namespace).await?;
    Ok(())
}

#[derive(Debug, PartialEq)]
struct CatalogApp {
    namespace: String,
    app_id: String,
    instance_name: String,
}

/// Apps a backup can reinstall — those that recorded which chart they came from.
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

/// The app's full settings, credentials included, from its backed-up `yolab-config`
/// Secret. Charts derive their passwords from these, so reinstalling with them is
/// what lets the restored data be opened again.
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

    // Delete the live PVC and wait for it to actually go away.
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

// ── Cluster observation helpers ────────────────────────────────────────────────

/// The replica counts to bring an app back to if its restore dies.
///
/// An error, never an empty list. This used to read a failed `kubectl get` as
/// "no deployments": the restore then scaled the app to zero anyway, and if it
/// crashed the watchdog had nothing to scale back up — the app stayed dark for
/// good, the exact outcome this record exists to prevent.
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
            // Kubernetes defaults an unset `replicas` to 1.
            let replicas = d["spec"]["replicas"].as_u64().unwrap_or(1);
            Ok(DeploymentScale {
                name: name.to_string(),
                replicas: u32::try_from(replicas)?,
            })
        })
        .collect()
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
    let out = restic(
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
    .await?;
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
    let out = restic(repo, cfg, &["snapshots", "--no-lock", id, "--json"])
        .await
        .ok()?;
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

/// The snapshot a backup pinned for one volume.
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

// ── Read side ──────────────────────────────────────────────────────────────────

fn liveness_of(s: &RestoreSet) -> Liveness {
    s.liveness(&crate::system::hostname(), &RESTORE_IN_FLIGHT)
}

/// Every recorded restore, newest first, classified into running/succeeded/failed.
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

/// Whether a restore is running on ANY node. `Err` when the records cannot be
/// read — callers must not read that as "no".
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

/// Restores recorded running whose driver is gone.
fn abandoned(sets: &[RestoreSet], me: &str) -> Vec<RestoreSet> {
    sets.iter()
        .filter(|s| s.is_running() && s.liveness(me, &RESTORE_IN_FLIGHT) == Liveness::Abandoned)
        .cloned()
        .collect()
}

/// Brings an abandoned restore's app back up so it never stays dark.
///
/// Cluster-scoped: one node acts. Each record is re-checked inside the
/// compare-and-swap that marks it failed, so a heartbeat that lands in between
/// (the driver was only slow) wins and the app is left to its driver.
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
        // The bug: node2's watchdog scaling node1's live restore back up.
        let now = Utc::now();
        let mut live = set("rs-live", "running");
        live.claim = Claim {
            owner: "node1".into(),
            heartbeat: (now - chrono::Duration::seconds(10)).to_rfc3339(),
        };
        // Its timestamp is ten minutes old — a skewed clock, or an API outage —
        // but this process has only just seen it, so it is not yet abandoned. It
        // becomes so after STALE_AFTER of being watched unchanged (see ops tests).
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

    // ── Which snapshot a volume is restored from ──────────────────────────────

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

    /// The live case: cluster snapshot 12:10:02, the only volume snapshot 12:10:05,
    /// and a backup from before snapshots were pinned. VolSync would restore
    /// nothing and call it success.
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

    // ── The catalog ──────────────────────────────────────────────────────────

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

    // ── backup_versions ──────────────────────────────────────────────────────

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
            |ns: &str| -> Vec<&str> { v[ns].iter().map(|x| x.snapshot_id.as_str()).collect() };
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
