//! Storage self-heal after disks or whole machines are permanently gone.
//!
//! THE ONLY IRREPLACEABLE DATA IS WHAT IS INSIDE APP VOLUMES, AND THAT HAS A
//! SECOND COPY IN THE CLOUD BACKUP. Everything else Ceph holds — the container
//! image store, the mgr's pool, the CephFS filesystem structure itself — can be
//! rebuilt empty by a machine. So once placement groups are provably gone, this
//! rebuilds all of it and brings every app back up on a fresh, empty volume,
//! marked corrupted so its owner restores it from backup.
//!
//! WHEN IS A PLACEMENT GROUP GONE. Never on a guess: `down` means "no copy is
//! reachable right now", which a reboot or a deploy also produces. An OSD is
//! declared lost only after it has stayed down for a grace period AND one of two
//! positive signals holds for that whole time:
//!
//!   * its disk is physically absent — the machine it lived on is up and its disk
//!     reconciler, publishing fresh, no longer reports any disk carrying that OSD;
//!   * its machine is gone — Kubernetes has reported the node NotReady for the
//!     (longer) node grace period.
//!
//! A machine that merely cannot be read (stale status, unknown OSD map) never
//! advances anything. Placement groups are only rebuilt once EVERY down OSD in
//! the cluster has been declared lost: while any down OSD could still return, an
//! inactive placement group might be waiting for exactly that disk.
//!
//! WHAT CANNOT HEAL. A cluster that loses Ceph monitor quorum or the Kubernetes
//! API cannot run any of this — with one machine per mon and per etcd member,
//! losing one of two machines stops both. Three or more machines survive losing one.
//!
//! Single writer: runs only on the disk-reconciler lease holder. Every step is
//! persisted before and after it runs, so a restart mid-rebuild resumes where it
//! stopped instead of starting a second rebuild over a half-finished one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::host::{Host, RealHost};

const STATE_CM: &str = "yolab-storage-heal";
const STATE_NS: &str = "kube-system";
const DATA_LOSS_CM: &str = "yolab-data-loss";
const DATA_LOSS_NS: &str = "kube-system";
const DISK_STATUS_CM: &str = "yolab-disk-status";
const DISK_NS: &str = "rook-ceph";
const CSI_NS: &str = "rook-ceph";

const FS_NAME: &str = "yolab-fs";
const FS_META_POOL: &str = "yolab-fs-metadata";
const FS_DATA_POOL: &str = "yolab-fs-data0";
const FS_SUBVOLUME_GROUP: &str = "csi";
const STORAGE_CLASS: &str = "yolab-cephfs";

/// Pools holding nothing a machine cannot fetch or regenerate again.
const DISPOSABLE_POOLS: &[&str] = &[".mgr", "images"];

/// A node's disk status older than this is not evidence of anything. The
/// reconciler publishes every 30s.
const STATUS_FRESH_SECS: u64 = 300;
const TICK: Duration = Duration::from_secs(60);
/// `rados purge` walks every object; bound it by the data, not the 30s CLI bound.
const POOL_PURGE_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct HealPolicy {
    pub disk_grace: Duration,
    pub node_grace: Duration,
}

impl HealPolicy {
    fn from_env() -> Self {
        let secs = |name: &str, default: u64| {
            Duration::from_secs(
                std::env::var(name)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(default),
            )
        };
        Self {
            disk_grace: secs("YOLAB_STORAGE_HEAL_DISK_GRACE_SECS", 900),
            node_grace: secs("YOLAB_STORAGE_HEAL_NODE_GRACE_SECS", 3600),
        }
    }
}

// ── Persisted state ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
struct HealState {
    /// OSD id → unix time it was first seen gone.
    #[serde(default)]
    gone_since: BTreeMap<i64, u64>,
    /// Declared lost (`ceph osd lost`), waiting to be purged once nothing is inactive.
    #[serde(default)]
    lost: Vec<i64>,
    #[serde(default)]
    rebuild: Option<Rebuild>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Rebuild {
    step: Step,
    apps: Vec<AppVolumes>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Step {
    StopApps,
    FailFilesystem,
    RecreatePgs,
    PurgePools,
    RemoveFilesystem,
    CreateFilesystem,
    ReplaceVolumes,
    RestartCsi,
    StartApps,
}

impl Step {
    fn next(self) -> Option<Step> {
        use Step::*;
        Some(match self {
            StopApps => FailFilesystem,
            FailFilesystem => RecreatePgs,
            RecreatePgs => PurgePools,
            PurgePools => RemoveFilesystem,
            RemoveFilesystem => CreateFilesystem,
            CreateFilesystem => ReplaceVolumes,
            ReplaceVolumes => RestartCsi,
            RestartCsi => StartApps,
            StartApps => return None,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct AppVolumes {
    namespace: String,
    workloads: Vec<Workload>,
    pvcs: Vec<SavedPvc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Workload {
    kind: String,
    name: String,
    replicas: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct SavedPvc {
    name: String,
    volume_name: Option<String>,
    /// What to recreate it from. None for VolSync's own PVCs, which it recreates.
    manifest: Option<Value>,
}

async fn read_state<H: Host>(host: &H) -> Result<HealState> {
    match host
        .kubectl_json(&["get", "configmap", STATE_CM, "-n", STATE_NS, "-o", "json"])
        .await
    {
        Ok(v) => {
            if v["kind"].as_str() != Some("ConfigMap") {
                bail!("{STATE_CM}: unreadable");
            }
            Ok(serde_json::from_str(
                v["data"]["state"].as_str().unwrap_or("{}"),
            )?)
        }
        // Absent is the normal state of a healthy cluster; unreadable is not, and
        // acting without knowing whether a rebuild is half done is how two start.
        Err(e) if crate::kubectl::is_not_found(&e) => Ok(HealState::default()),
        Err(e) => Err(e),
    }
}

async fn write_state<H: Host>(host: &H, state: &HealState) -> Result<()> {
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": STATE_CM, "namespace": STATE_NS },
        "data": { "state": serde_json::to_string(state)? },
    });
    host.kubectl_apply(&manifest.to_string()).await
}

// ── Corrupted apps (read by the damage screen, cleared by a restore) ──────────

async fn read_corrupted<H: Host>(host: &H) -> Vec<String> {
    host.kubectl_json(&[
        "get",
        "configmap",
        DATA_LOSS_CM,
        "-n",
        DATA_LOSS_NS,
        "-o",
        "json",
    ])
    .await
    .ok()
    .and_then(|v| serde_json::from_str(v["data"]["apps"].as_str()?).ok())
    .unwrap_or_default()
}

async fn write_corrupted<H: Host>(host: &H, apps: &[String]) -> Result<()> {
    let patch = json!({"data": {"apps": serde_json::to_string(apps)?}}).to_string();
    let args = [
        "patch",
        "configmap",
        DATA_LOSS_CM,
        "-n",
        DATA_LOSS_NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ];
    if host.kubectl(&args).await.is_ok() {
        return Ok(());
    }
    // Created bare and patched, never applied: `kubectl apply` of this map elsewhere
    // would prune a key an earlier apply wrote and a later one did not mention.
    let _ = host
        .kubectl(&["create", "configmap", DATA_LOSS_CM, "-n", DATA_LOSS_NS])
        .await;
    host.kubectl(&args).await.map(|_| ())
}

/// Apps whose volumes were replaced with empty ones and not yet restored.
pub(crate) async fn corrupted_namespaces() -> Vec<String> {
    read_corrupted(&RealHost).await
}

/// A successful restore is what makes an app whole again.
pub(crate) async fn clear_corrupted(namespace: &str) {
    let host = RealHost;
    let mut apps = read_corrupted(&host).await;
    let before = apps.len();
    apps.retain(|a| a != namespace);
    if apps.len() != before {
        if let Err(e) = write_corrupted(&host, &apps).await {
            tracing::warn!(
                "storage-heal: could not clear {namespace} from the corrupted list: {e}"
            );
        }
    }
}

pub(crate) async fn is_rebuilding() -> bool {
    read_state(&RealHost)
        .await
        .map(|s| s.rebuild.is_some())
        .unwrap_or(false)
}

// ── Observation ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// Its disk is reported on a live machine: the daemon is down, the data is not.
    Present,
    DiskGone,
    NodeGone,
    /// Nothing trustworthy to go on this tick.
    Unknown,
}

#[derive(Default)]
struct NodeDisks {
    fresh: bool,
    osd_map_known: bool,
    osd_ids: HashSet<i64>,
}

#[derive(Default)]
struct Observed {
    now: u64,
    /// id → up
    osds: BTreeMap<i64, bool>,
    osd_host: HashMap<i64, String>,
    known_nodes: HashSet<String>,
    ready_nodes: HashSet<String>,
    disks: HashMap<String, NodeDisks>,
}

fn presence(obs: &Observed, osd: i64) -> Presence {
    let Some(host) = obs.osd_host.get(&osd) else {
        return Presence::Unknown;
    };
    // A CRUSH host Kubernetes has never heard of is a naming mismatch, not proof
    // the machine is gone.
    if !obs.known_nodes.contains(host) {
        return Presence::Unknown;
    }
    if !obs.ready_nodes.contains(host) {
        return Presence::NodeGone;
    }
    match obs.disks.get(host) {
        Some(d) if d.fresh && d.osd_map_known => {
            if d.osd_ids.contains(&osd) {
                Presence::Present
            } else {
                Presence::DiskGone
            }
        }
        _ => Presence::Unknown,
    }
}

/// Advance every down OSD's clock and return the ones now due to be declared lost.
fn advance_clocks(state: &mut HealState, obs: &Observed, policy: &HealPolicy) -> Vec<i64> {
    state.gone_since.retain(|id, _| obs.osds.contains_key(id));
    // A lost disk that came back before it was purged is simply an OSD again.
    state
        .lost
        .retain(|id| obs.osds.get(id).is_some_and(|up| !up));

    let mut due = Vec::new();
    for (&id, &up) in &obs.osds {
        if up {
            state.gone_since.remove(&id);
            continue;
        }
        if state.lost.contains(&id) {
            continue;
        }
        let grace = match presence(obs, id) {
            Presence::Present => {
                state.gone_since.remove(&id);
                continue;
            }
            // Keeps the clock where it is: not evidence either way.
            Presence::Unknown => continue,
            Presence::DiskGone => policy.disk_grace,
            Presence::NodeGone => policy.node_grace,
        };
        let since = *state.gone_since.entry(id).or_insert(obs.now);
        if obs.now.saturating_sub(since) >= grace.as_secs() {
            due.push(id);
        }
    }
    due
}

async fn observe<H: Host>(host: &H, now: u64) -> Option<Observed> {
    let dump = host.ceph_json(&["osd", "dump"]).await.ok()?;
    let tree = host.ceph_json(&["osd", "tree"]).await.ok()?;
    let nodes = host
        .kubectl_json(&["get", "nodes", "-o", "json"])
        .await
        .ok()?;
    let status = host
        .kubectl_json(&[
            "get",
            "configmap",
            DISK_STATUS_CM,
            "-n",
            DISK_NS,
            "-o",
            "json",
        ])
        .await
        .ok()?;
    Some(parse_observed(&dump, &tree, &nodes, &status, now))
}

fn parse_observed(dump: &Value, tree: &Value, nodes: &Value, status: &Value, now: u64) -> Observed {
    let mut obs = Observed {
        now,
        ..Default::default()
    };
    for o in dump["osds"].as_array().into_iter().flatten() {
        if let Some(id) = o["osd"].as_i64() {
            obs.osds.insert(id, o["up"].as_i64() == Some(1));
        }
    }
    for n in tree["nodes"].as_array().into_iter().flatten() {
        if n["type"].as_str() != Some("host") {
            continue;
        }
        let Some(name) = n["name"].as_str() else {
            continue;
        };
        for c in n["children"].as_array().into_iter().flatten() {
            if let Some(id) = c.as_i64().filter(|id| *id >= 0) {
                obs.osd_host.insert(id, name.to_string());
            }
        }
    }
    for n in nodes["items"].as_array().into_iter().flatten() {
        let Some(name) = n["metadata"]["name"].as_str() else {
            continue;
        };
        obs.known_nodes.insert(name.to_string());
        let ready = n["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["type"] == "Ready" && c["status"] == "True");
        if ready {
            obs.ready_nodes.insert(name.to_string());
        }
    }
    for (node, raw) in status["data"].as_object().into_iter().flatten() {
        let Some(payload) = raw
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        else {
            continue;
        };
        let fresh = payload["published_at"]
            .as_u64()
            .is_some_and(|t| now.saturating_sub(t) <= STATUS_FRESH_SECS);
        let osd_ids = payload["disks"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(_, d)| d["osd_id"].as_i64())
            .collect();
        obs.disks.insert(
            node.clone(),
            NodeDisks {
                fresh,
                osd_map_known: payload["osd_map_known"].as_bool() == Some(true),
                osd_ids,
            },
        );
    }
    obs
}

// ── Placement groups ──────────────────────────────────────────────────────────

#[derive(Default, Debug, PartialEq)]
struct PgPicture {
    /// Inactive with no copy anywhere, by pool name.
    lost: BTreeMap<String, Vec<String>>,
    inactive: usize,
}

fn is_lost_state(state: &str) -> bool {
    state.split('+').any(|s| s == "down" || s == "incomplete")
}

fn pg_picture(dump: &Value, pgs: &Value) -> Option<PgPicture> {
    let names: HashMap<i64, String> = dump["pools"]
        .as_array()?
        .iter()
        .filter_map(|p| Some((p["pool"].as_i64()?, p["pool_name"].as_str()?.to_string())))
        .collect();
    let items = pgs["pg_stats"].as_array().or_else(|| pgs.as_array())?;
    let mut pic = PgPicture::default();
    for pg in items {
        let state = pg["state"].as_str().unwrap_or("");
        if state.split('+').any(|s| s == "active") {
            continue;
        }
        pic.inactive += 1;
        if !is_lost_state(state) {
            continue;
        }
        let Some(pgid) = pg["pgid"].as_str() else {
            continue;
        };
        let Some(pool) = pgid
            .split('.')
            .next()
            .and_then(|p| p.parse::<i64>().ok())
            .and_then(|id| names.get(&id))
        else {
            continue;
        };
        pic.lost
            .entry(pool.clone())
            .or_default()
            .push(pgid.to_string());
    }
    Some(pic)
}

async fn read_pgs<H: Host>(host: &H) -> Option<PgPicture> {
    let dump = host.ceph_json(&["osd", "dump"]).await.ok()?;
    let pgs = host.ceph_json(&["pg", "dump", "pgs_brief"]).await.ok()?;
    pg_picture(&dump, &pgs)
}

async fn force_create<H: Host>(host: &H, pgids: &[String]) {
    for pg in pgids {
        match host
            .ceph(&["osd", "force-create-pg", pg, "--yes-i-really-mean-it"])
            .await
        {
            Ok(_) => tracing::info!("storage-heal: rebuilt {pg} empty"),
            Err(e) => tracing::warn!("storage-heal: could not rebuild {pg}: {e}"),
        }
    }
}

// ── The tick ──────────────────────────────────────────────────────────────────

pub async fn run() {
    tokio::time::sleep(Duration::from_secs(120)).await;
    let policy = HealPolicy::from_env();
    loop {
        if crate::disks_reconciler::is_reconcile_leader().await
            && !crate::routers::restore::is_running().await
        {
            if let Err(e) = tick(&RealHost, &policy, now_secs()).await {
                tracing::warn!("storage-heal: {e:#}");
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn tick<H: Host>(host: &H, policy: &HealPolicy, now: u64) -> Result<()> {
    if !host.reachable().await {
        return Ok(());
    }
    let mut state = read_state(host).await?;

    if state.rebuild.is_some() {
        return continue_rebuild(host, &mut state).await;
    }

    let Some(obs) = observe(host, now).await else {
        return Ok(());
    };
    let before = state.clone();
    let due = advance_clocks(&mut state, &obs, policy);
    let declared_now = !due.is_empty();
    for id in due {
        if declare_lost(host, id).await {
            state.gone_since.remove(&id);
            state.lost.push(id);
        }
    }
    if state != before {
        write_state(host, &state).await?;
    }
    // Placement groups are judged on a later tick, once peering has absorbed the
    // declaration — right after `osd lost` they are still peering, not settled.
    if declared_now {
        return Ok(());
    }

    let waiting: Vec<i64> = obs
        .osds
        .iter()
        .filter(|(id, up)| !**up && !state.lost.contains(id))
        .map(|(id, _)| *id)
        .collect();
    if !waiting.is_empty() {
        if !state.lost.is_empty() {
            tracing::info!(
                "storage-heal: waiting on down OSD(s) {waiting:?} before rebuilding anything — \
                 they may still come back"
            );
        }
        return Ok(());
    }

    let Some(pgs) = read_pgs(host).await else {
        return Ok(());
    };

    for pool in DISPOSABLE_POOLS {
        if let Some(ids) = pgs.lost.get(*pool) {
            tracing::warn!(
                "storage-heal: {pool} lost {} placement group(s) — rebuilding them empty",
                ids.len()
            );
            force_create(host, ids).await;
        }
    }
    for (pool, ids) in &pgs.lost {
        if !DISPOSABLE_POOLS.contains(&pool.as_str())
            && pool != FS_META_POOL
            && pool != FS_DATA_POOL
        {
            tracing::warn!(
                "storage-heal: {pool} lost {} placement group(s); it is not a pool YoLab \
                 manages, so it is left for a person to decide",
                ids.len()
            );
        }
    }

    if pgs.lost.contains_key(FS_META_POOL) {
        return start_rebuild(host, &mut state).await;
    }
    if let Some(ids) = pgs.lost.get(FS_DATA_POOL) {
        // The filesystem's structure survived, so it keeps working, but some files
        // now have holes and which app they belong to cannot be told from here.
        tracing::warn!(
            "storage-heal: {FS_DATA_POOL} lost {} placement group(s) — rebuilding them empty \
             and marking every app on CephFS as corrupted",
            ids.len()
        );
        let apps = snapshot_apps(host).await?;
        mark_corrupted(
            host,
            apps.iter()
                .filter(|a| a.namespace.starts_with("yolab-"))
                .map(|a| a.namespace.clone()),
        )
        .await?;
        force_create(host, ids).await;
        return Ok(());
    }

    if pgs.inactive == 0 && !state.lost.is_empty() {
        purge_lost(host, &mut state).await?;
    }
    Ok(())
}

async fn declare_lost<H: Host>(host: &H, id: i64) -> bool {
    tracing::warn!(
        "storage-heal: osd.{id} has been gone past its grace period — declaring it lost"
    );
    let osd = format!("osd.{id}");
    if let Err(e) = host.ceph(&["osd", "out", &osd]).await {
        tracing::warn!("storage-heal: could not mark {osd} out: {e}");
        return false;
    }
    match host
        .ceph(&["osd", "lost", &id.to_string(), "--yes-i-really-mean-it"])
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("storage-heal: could not mark {osd} lost: {e}");
            false
        }
    }
}

async fn purge_lost<H: Host>(host: &H, state: &mut HealState) -> Result<()> {
    for id in state.lost.clone() {
        if let Err(e) = host.osd_purge(id).await {
            tracing::warn!("storage-heal: could not purge osd.{id}: {e}");
            continue;
        }
        if host.osd_ids().await.is_ok_and(|ids| !ids.contains(&id)) {
            tracing::info!("storage-heal: osd.{id} purged — the cluster no longer waits for it");
            state.lost.retain(|x| *x != id);
        }
    }
    write_state(host, state).await
}

async fn mark_corrupted<H: Host>(host: &H, namespaces: impl Iterator<Item = String>) -> Result<()> {
    let mut apps = read_corrupted(host).await;
    for ns in namespaces {
        if !apps.contains(&ns) {
            apps.push(ns);
        }
    }
    apps.sort();
    write_corrupted(host, &apps).await
}

// ── Filesystem rebuild ────────────────────────────────────────────────────────

/// Every namespace with a volume on CephFS, with what is needed to put it back.
async fn snapshot_apps<H: Host>(host: &H) -> Result<Vec<AppVolumes>> {
    let pvcs = host
        .kubectl_json(&["get", "pvc", "-A", "-o", "json"])
        .await?;
    let mut by_ns: BTreeMap<String, Vec<SavedPvc>> = BTreeMap::new();
    for p in pvcs["items"].as_array().into_iter().flatten() {
        if p["spec"]["storageClassName"].as_str() != Some(STORAGE_CLASS) {
            continue;
        }
        let (Some(ns), Some(name)) = (
            p["metadata"]["namespace"].as_str(),
            p["metadata"]["name"].as_str(),
        ) else {
            continue;
        };
        by_ns.entry(ns.to_string()).or_default().push(SavedPvc {
            name: name.to_string(),
            volume_name: p["spec"]["volumeName"].as_str().map(str::to_string),
            manifest: (!name.starts_with("volsync-")).then(|| recreatable_pvc(p)),
        });
    }

    let mut apps = Vec::new();
    for (namespace, pvcs) in by_ns {
        if pvcs.iter().all(|p| p.manifest.is_none()) {
            continue;
        }
        let w = host
            .kubectl_json(&[
                "get",
                "deployment,statefulset",
                "-n",
                &namespace,
                "-o",
                "json",
            ])
            .await?;
        let workloads = w["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                Some(Workload {
                    kind: i["kind"].as_str()?.to_lowercase(),
                    name: i["metadata"]["name"].as_str()?.to_string(),
                    replicas: i["spec"]["replicas"].as_u64().unwrap_or(1),
                })
            })
            .collect();
        apps.push(AppVolumes {
            namespace,
            workloads,
            pvcs,
        });
    }
    Ok(apps)
}

/// A PVC with everything that tied it to its old volume removed.
fn recreatable_pvc(p: &Value) -> Value {
    let annotations: serde_json::Map<String, Value> = p["metadata"]["annotations"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| {
            !k.starts_with("pv.kubernetes.io/")
                && !k.ends_with("storage-provisioner")
                && k.as_str() != "kubectl.kubernetes.io/last-applied-configuration"
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let spec = &p["spec"];
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": p["metadata"]["name"],
            "namespace": p["metadata"]["namespace"],
            "labels": p["metadata"]["labels"].as_object().cloned().unwrap_or_default(),
            "annotations": annotations,
        },
        "spec": {
            "accessModes": spec["accessModes"],
            "resources": { "requests": spec["resources"]["requests"] },
            "storageClassName": STORAGE_CLASS,
            "volumeMode": spec["volumeMode"].as_str().unwrap_or("Filesystem"),
        },
    })
}

async fn start_rebuild<H: Host>(host: &H, state: &mut HealState) -> Result<()> {
    let apps = snapshot_apps(host).await?;
    tracing::warn!(
        "storage-heal: {FS_META_POOL} lost placement groups, so the filesystem cannot be \
         read. Rebuilding it empty; {} app(s) will come back on empty volumes and be marked \
         for restore from backup",
        apps.len()
    );
    // Recorded before anything is touched: the owner must learn which apps lost
    // their data even if this process dies on the very next line.
    mark_corrupted(
        host,
        apps.iter()
            .filter(|a| a.namespace.starts_with("yolab-"))
            .map(|a| a.namespace.clone()),
    )
    .await?;
    state.rebuild = Some(Rebuild {
        step: Step::StopApps,
        apps,
    });
    write_state(host, state).await?;
    continue_rebuild(host, state).await
}

async fn continue_rebuild<H: Host>(host: &H, state: &mut HealState) -> Result<()> {
    loop {
        let Some(rebuild) = state.rebuild.clone() else {
            return Ok(());
        };
        tracing::info!("storage-heal: rebuild step {:?}", rebuild.step);
        if !run_step(host, &rebuild).await? {
            return Ok(()); // not finished yet — next tick looks again
        }
        match rebuild.step.next() {
            Some(step) => state.rebuild = Some(Rebuild { step, ..rebuild }),
            None => {
                tracing::info!(
                    "storage-heal: filesystem rebuilt and every app is back up — restore the \
                     corrupted ones from backup"
                );
                state.rebuild = None;
            }
        }
        write_state(host, state).await?;
    }
}

/// Ok(true) when the step is complete, Ok(false) when it must be looked at again
/// next tick, Err when it failed and will be retried.
async fn run_step<H: Host>(host: &H, rebuild: &Rebuild) -> Result<bool> {
    match rebuild.step {
        Step::StopApps => {
            for app in &rebuild.apps {
                for w in &app.workloads {
                    let target = format!("{}/{}", w.kind, w.name);
                    let _ = host
                        .kubectl(&["scale", &target, "-n", &app.namespace, "--replicas=0"])
                        .await;
                }
                // Their mounts point at a filesystem that no longer answers; a graceful
                // stop waits on an unmount that never returns.
                let _ = host
                    .kubectl(&[
                        "delete",
                        "pod",
                        "--all",
                        "-n",
                        &app.namespace,
                        "--force",
                        "--grace-period=0",
                        "--wait=false",
                    ])
                    .await;
            }
            Ok(true)
        }
        Step::FailFilesystem => {
            if fs_exists(host).await? {
                host.ceph(&["fs", "fail", FS_NAME]).await?;
            }
            Ok(true)
        }
        Step::RecreatePgs => {
            let Some(pgs) = read_pgs(host).await else {
                return Ok(false);
            };
            let mut waiting = false;
            for pool in [FS_META_POOL, FS_DATA_POOL] {
                if let Some(ids) = pgs.lost.get(pool) {
                    force_create(host, ids).await;
                    waiting = true;
                }
            }
            // Also wait out PGs that are merely creating/peering in these two pools,
            // or the purge below blocks on them.
            let still_inactive = fs_pools_inactive(host).await?;
            Ok(!waiting && !still_inactive)
        }
        Step::PurgePools => {
            for pool in [FS_META_POOL, FS_DATA_POOL] {
                let out = host
                    .run_cmd_bounded(
                        "rados",
                        &["purge", pool, "--yes-i-really-really-mean-it"],
                        POOL_PURGE_TIMEOUT,
                    )
                    .await?;
                if !out.success {
                    bail!("rados purge {pool}: {}", out.stderr.trim());
                }
            }
            Ok(true)
        }
        Step::RemoveFilesystem => {
            if fs_exists(host).await? {
                host.ceph(&["fs", "rm", FS_NAME, "--yes-i-really-mean-it"])
                    .await?;
            }
            Ok(true)
        }
        Step::CreateFilesystem => {
            if !fs_exists(host).await? {
                host.ceph(&["fs", "new", FS_NAME, FS_META_POOL, FS_DATA_POOL, "--force"])
                    .await?;
            }
            // Needs an MDS to have taken the new filesystem, which can take a moment.
            host.ceph(&[
                "fs",
                "subvolumegroup",
                "create",
                FS_NAME,
                FS_SUBVOLUME_GROUP,
            ])
            .await?;
            Ok(true)
        }
        Step::ReplaceVolumes => {
            let mut all_gone = true;
            for app in &rebuild.apps {
                let ns = app.namespace.as_str();
                for pvc in &app.pvcs {
                    let exists = host.kubectl(&["get", "pvc", &pvc.name, "-n", ns]).await;
                    if let Err(e) = &exists {
                        if !crate::kubectl::is_not_found(e) {
                            bail!("reading {ns}/{}: {e}", pvc.name);
                        }
                    }
                    if exists.is_ok() {
                        let _ = host
                            .kubectl(&[
                                "patch",
                                "pvc",
                                &pvc.name,
                                "-n",
                                ns,
                                "--type",
                                "merge",
                                "-p",
                                r#"{"metadata":{"finalizers":null}}"#,
                            ])
                            .await;
                        let _ = host
                            .kubectl(&[
                                "delete",
                                "pvc",
                                &pvc.name,
                                "-n",
                                ns,
                                "--wait=false",
                                "--ignore-not-found",
                            ])
                            .await;
                    }
                    if let Some(pv) = &pvc.volume_name {
                        // The CSI driver cannot delete a subvolume from a filesystem
                        // that no longer exists, so its finalizer would hold forever.
                        let _ = host
                            .kubectl(&[
                                "patch",
                                "pv",
                                pv,
                                "--type",
                                "merge",
                                "-p",
                                r#"{"metadata":{"finalizers":null}}"#,
                            ])
                            .await;
                        let _ = host
                            .kubectl(&["delete", "pv", pv, "--wait=false", "--ignore-not-found"])
                            .await;
                    }
                    if host
                        .kubectl(&["get", "pvc", &pvc.name, "-n", ns])
                        .await
                        .is_ok()
                    {
                        all_gone = false;
                    }
                }
            }
            if !all_gone {
                return Ok(false);
            }
            for app in &rebuild.apps {
                for pvc in &app.pvcs {
                    if let Some(m) = &pvc.manifest {
                        host.kubectl_apply(&m.to_string()).await?;
                    }
                }
            }
            Ok(true)
        }
        Step::RestartCsi => {
            // Both hold state about the filesystem that was just replaced.
            for app in ["csi-cephfsplugin", "csi-cephfsplugin-provisioner"] {
                let selector = format!("app={app}");
                let _ = host
                    .kubectl(&[
                        "delete",
                        "pod",
                        "-n",
                        CSI_NS,
                        "-l",
                        &selector,
                        "--wait=false",
                    ])
                    .await;
            }
            Ok(true)
        }
        Step::StartApps => {
            for app in &rebuild.apps {
                for w in &app.workloads {
                    let target = format!("{}/{}", w.kind, w.name);
                    let replicas = format!("--replicas={}", w.replicas);
                    host.kubectl(&["scale", &target, "-n", &app.namespace, &replicas])
                        .await?;
                }
            }
            Ok(true)
        }
    }
}

async fn fs_exists<H: Host>(host: &H) -> Result<bool> {
    let ls = host.ceph_json(&["fs", "ls"]).await?;
    Ok(ls
        .as_array()
        .is_some_and(|a| a.iter().any(|f| f["name"].as_str() == Some(FS_NAME))))
}

async fn fs_pools_inactive<H: Host>(host: &H) -> Result<bool> {
    let dump = host.ceph_json(&["osd", "dump"]).await?;
    let pgs = host.ceph_json(&["pg", "dump", "pgs_brief"]).await?;
    let ids: HashSet<i64> = dump["pools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            matches!(
                p["pool_name"].as_str(),
                Some(FS_META_POOL) | Some(FS_DATA_POOL)
            )
        })
        .filter_map(|p| p["pool"].as_i64())
        .collect();
    Ok(pgs["pg_stats"]
        .as_array()
        .or_else(|| pgs.as_array())
        .into_iter()
        .flatten()
        .filter(|pg| {
            pg["pgid"]
                .as_str()
                .and_then(|id| id.split('.').next()?.parse::<i64>().ok())
                .is_some_and(|pool| ids.contains(&pool))
        })
        .any(|pg| {
            !pg["state"]
                .as_str()
                .unwrap_or("")
                .split('+')
                .any(|s| s == "active")
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const NOW: u64 = 1_000_000;

    fn policy() -> HealPolicy {
        HealPolicy {
            disk_grace: Duration::from_secs(900),
            node_grace: Duration::from_secs(3600),
        }
    }

    /// node1 carries osd.0 and osd.1; osd.1 is down.
    fn obs(ready: bool, fresh: bool, reported: &[i64]) -> Observed {
        let mut o = Observed {
            now: NOW,
            ..Default::default()
        };
        o.osds.insert(0, true);
        o.osds.insert(1, false);
        o.osd_host.insert(0, "node1".into());
        o.osd_host.insert(1, "node1".into());
        o.known_nodes.insert("node1".into());
        if ready {
            o.ready_nodes.insert("node1".into());
        }
        o.disks.insert(
            "node1".into(),
            NodeDisks {
                fresh,
                osd_map_known: true,
                osd_ids: reported.iter().copied().collect(),
            },
        );
        o
    }

    #[test]
    fn a_disk_still_reported_is_a_down_daemon_not_a_lost_disk() {
        assert_eq!(presence(&obs(true, true, &[0, 1]), 1), Presence::Present);
    }

    #[test]
    fn a_disk_missing_from_a_fresh_report_is_gone() {
        assert_eq!(presence(&obs(true, true, &[0]), 1), Presence::DiskGone);
    }

    #[test]
    fn a_stale_report_proves_nothing() {
        assert_eq!(presence(&obs(true, false, &[0]), 1), Presence::Unknown);
    }

    #[test]
    fn an_unknown_osd_map_proves_nothing() {
        let mut o = obs(true, true, &[0]);
        o.disks.get_mut("node1").unwrap().osd_map_known = false;
        assert_eq!(presence(&o, 1), Presence::Unknown);
    }

    #[test]
    fn a_not_ready_node_is_node_gone_and_an_unheard_of_host_is_unknown() {
        assert_eq!(presence(&obs(false, false, &[]), 1), Presence::NodeGone);
        let mut o = obs(true, true, &[0]);
        o.osd_host.insert(1, "somewhere-else".into());
        assert_eq!(presence(&o, 1), Presence::Unknown);
    }

    #[test]
    fn a_gone_disk_waits_out_its_grace_period() {
        let mut state = HealState::default();
        let mut o = obs(true, true, &[0]);
        assert!(advance_clocks(&mut state, &o, &policy()).is_empty());
        assert_eq!(state.gone_since.get(&1), Some(&NOW));
        o.now = NOW + 899;
        assert!(advance_clocks(&mut state, &o, &policy()).is_empty());
        o.now = NOW + 900;
        assert_eq!(advance_clocks(&mut state, &o, &policy()), vec![1]);
    }

    #[test]
    fn a_whole_machine_gets_the_longer_grace() {
        let mut state = HealState::default();
        let mut o = obs(false, false, &[]);
        o.osds.insert(0, false);
        advance_clocks(&mut state, &o, &policy());
        o.now = NOW + 900;
        assert!(advance_clocks(&mut state, &o, &policy()).is_empty());
        o.now = NOW + 3600;
        assert_eq!(advance_clocks(&mut state, &o, &policy()), vec![0, 1]);
    }

    #[test]
    fn the_disk_coming_back_resets_the_clock() {
        let mut state = HealState::default();
        let mut o = obs(true, true, &[0]);
        advance_clocks(&mut state, &o, &policy());
        o = obs(true, true, &[0, 1]);
        o.now = NOW + 600;
        advance_clocks(&mut state, &o, &policy());
        assert!(state.gone_since.is_empty());
    }

    #[test]
    fn unknown_neither_starts_nor_clears_a_clock_and_never_fires() {
        let mut state = HealState::default();
        state.gone_since.insert(1, NOW - 10_000);
        let o = obs(true, false, &[0]);
        assert!(advance_clocks(&mut state, &o, &policy()).is_empty());
        assert_eq!(state.gone_since.get(&1), Some(&(NOW - 10_000)));
    }

    #[test]
    fn a_lost_osd_that_comes_back_up_is_no_longer_lost() {
        let mut state = HealState {
            lost: vec![1],
            ..Default::default()
        };
        let mut o = obs(true, true, &[0, 1]);
        o.osds.insert(1, true);
        advance_clocks(&mut state, &o, &policy());
        assert!(state.lost.is_empty());
    }

    fn dump() -> Value {
        json!({
            "osds": [{"osd": 0, "up": 1}, {"osd": 1, "up": 0}],
            "pools": [
                {"pool": 1, "pool_name": ".mgr"},
                {"pool": 2, "pool_name": FS_META_POOL},
                {"pool": 3, "pool_name": FS_DATA_POOL},
                {"pool": 4, "pool_name": "images"},
            ]
        })
    }

    #[test]
    fn pg_picture_groups_lost_pgs_by_pool_and_counts_all_inactive() {
        let pgs = json!({"pg_ready": true, "pg_stats": [
            {"pgid": "2.1", "state": "down"},
            {"pgid": "3.4", "state": "incomplete"},
            {"pgid": "4.2", "state": "stale+down"},
            {"pgid": "4.3", "state": "peering"},
            {"pgid": "1.0", "state": "active+clean"},
        ]});
        let pic = pg_picture(&dump(), &pgs).unwrap();
        assert_eq!(pic.inactive, 4);
        assert_eq!(pic.lost.get(FS_META_POOL), Some(&vec!["2.1".to_string()]));
        assert_eq!(pic.lost.get(FS_DATA_POOL), Some(&vec!["3.4".to_string()]));
        assert_eq!(pic.lost.get("images"), Some(&vec!["4.2".to_string()]));
    }

    #[test]
    fn parse_observed_reads_all_four_sources() {
        let tree = json!({"nodes": [
            {"id": -1, "type": "root", "name": "default", "children": [-3]},
            {"id": -3, "type": "host", "name": "node1", "children": [1, 0]},
        ]});
        let nodes = json!({"items": [
            {"metadata": {"name": "node1"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}},
            {"metadata": {"name": "node2"}, "status": {"conditions": [{"type": "Ready", "status": "Unknown"}]}},
        ]});
        let payload = json!({
            "published_at": NOW - 20,
            "osd_map_known": true,
            "disks": {"system": {"osd_id": 0}, "sdc": {}},
        });
        let status = json!({"data": {"node1": payload.to_string()}});
        let o = parse_observed(&dump(), &tree, &nodes, &status, NOW);
        assert_eq!(o.osds.get(&1), Some(&false));
        assert_eq!(o.osd_host.get(&1).map(String::as_str), Some("node1"));
        assert!(o.ready_nodes.contains("node1") && !o.ready_nodes.contains("node2"));
        assert!(o.known_nodes.contains("node2"));
        let d = o.disks.get("node1").unwrap();
        assert!(d.fresh && d.osd_map_known);
        assert_eq!(d.osd_ids, HashSet::from([0]));
    }

    #[test]
    fn a_recreated_pvc_forgets_its_old_volume() {
        let p = json!({
            "metadata": {
                "name": "data", "namespace": "yolab-x", "uid": "u", "resourceVersion": "9",
                "labels": {"app.kubernetes.io/managed-by": "Helm"},
                "annotations": {
                    "meta.helm.sh/release-name": "x",
                    "pv.kubernetes.io/bind-completed": "yes",
                    "volume.kubernetes.io/storage-provisioner": "rook-ceph.cephfs.csi.ceph.com",
                },
                "finalizers": ["kubernetes.io/pvc-protection"],
            },
            "spec": {
                "accessModes": ["ReadWriteMany"],
                "resources": {"requests": {"storage": "5Gi"}},
                "storageClassName": STORAGE_CLASS,
                "volumeMode": "Filesystem",
                "volumeName": "pvc-old",
            },
            "status": {"phase": "Bound"},
        });
        let m = recreatable_pvc(&p);
        assert_eq!(m["spec"]["volumeName"], Value::Null);
        assert_eq!(m["metadata"]["uid"], Value::Null);
        assert_eq!(m["metadata"]["finalizers"], Value::Null);
        assert_eq!(m["status"], Value::Null);
        assert_eq!(
            m["metadata"]["annotations"]["meta.helm.sh/release-name"],
            "x"
        );
        assert_eq!(
            m["metadata"]["annotations"]["pv.kubernetes.io/bind-completed"],
            Value::Null
        );
        assert_eq!(m["spec"]["resources"]["requests"]["storage"], "5Gi");
    }

    #[test]
    fn steps_run_in_the_order_that_keeps_every_resume_safe() {
        let mut s = Step::StopApps;
        let mut order = vec![s];
        while let Some(n) = s.next() {
            order.push(n);
            s = n;
        }
        // Purge before rm: if this dies between the two, cephfs::ensure recreating
        // the filesystem finds empty pools rather than the old filesystem's objects.
        let pos = |x| order.iter().position(|s| *s == x).unwrap();
        assert!(pos(Step::FailFilesystem) < pos(Step::RecreatePgs));
        assert!(pos(Step::RecreatePgs) < pos(Step::PurgePools));
        assert!(pos(Step::PurgePools) < pos(Step::RemoveFilesystem));
        assert!(pos(Step::CreateFilesystem) < pos(Step::ReplaceVolumes));
        assert_eq!(order.last(), Some(&Step::StartApps));
    }

    fn healthy_cluster_host(state: &str) -> FakeHost {
        cluster_host(state, dump())
    }

    fn cluster_host(state: &str, dump: Value) -> FakeHost {
        cluster_host_ready(state, dump, true)
    }

    fn cluster_host_ready(state: &str, dump: Value, ready: bool) -> FakeHost {
        let tree =
            json!({"nodes": [{"id": -3, "type": "host", "name": "node1", "children": [0, 1]}]});
        let ready = if ready { "True" } else { "Unknown" };
        let nodes = json!({"items": [{"metadata": {"name": "node1"},
            "status": {"conditions": [{"type": "Ready", "status": ready}]}}]});
        let status = json!({"data": {"node1": json!({
            "published_at": NOW, "osd_map_known": true, "disks": {"system": {"osd_id": 0}}
        }).to_string()}});
        FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": state}}).to_string(),
            )
            .ok("ceph osd dump", &dump.to_string())
            .ok("ceph osd tree", &tree.to_string())
            .ok("kubectl get nodes", &nodes.to_string())
            .ok(
                "kubectl get configmap yolab-disk-status",
                &status.to_string(),
            )
            .ok("kubectl-apply", "")
    }

    #[tokio::test]
    async fn a_disk_inside_its_grace_period_changes_nothing_in_ceph() {
        let host = healthy_cluster_host("{}");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("ceph osd out"));
        assert!(!host.ran("force-create-pg"));
        assert!(host.ran("kubectl-apply"), "the clock must be persisted");
    }

    #[tokio::test]
    async fn past_the_grace_period_the_osd_is_declared_lost_and_nothing_else_happens_yet() {
        let state = json!({"gone_since": {"1": NOW - 901}}).to_string();
        let host = healthy_cluster_host(&state)
            .ok("ceph osd out osd.1", "")
            .ok("ceph osd lost 1", "");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("ceph osd lost 1 --yes-i-really-mean-it"));
        assert!(
            !host.ran("pg dump"),
            "PGs are judged on a later tick, after peering"
        );
    }

    #[tokio::test]
    async fn a_down_osd_that_is_not_yet_lost_blocks_every_rebuild() {
        let state = json!({"lost": [1], "gone_since": {}}).to_string();
        let mut d = dump();
        d["osds"] = json!([{"osd": 0, "up": 1}, {"osd": 1, "up": 0}, {"osd": 2, "up": 0}]);
        let host = cluster_host(&state, d);
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("pg dump"));
        assert!(!host.ran("force-create-pg"));
    }

    #[tokio::test]
    async fn lost_metadata_starts_a_rebuild_that_marks_apps_before_touching_them() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "down"}, {"pgid": "4.0", "state": "incomplete"}]});
        let pvcs = json!({"items": [{
            "metadata": {"name": "data", "namespace": "yolab-fb"},
            "spec": {"storageClassName": STORAGE_CLASS, "volumeName": "pv1",
                     "accessModes": ["ReadWriteMany"], "resources": {"requests": {"storage": "1Gi"}}},
        }]});
        let workloads = json!({"items": [{"kind": "Deployment", "metadata": {"name": "fb"}, "spec": {"replicas": 1}}]});
        let host = healthy_cluster_host(&state)
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("ceph osd force-create-pg", "")
            .ok("kubectl get pvc -A", &pvcs.to_string())
            .ok(
                "kubectl get deployment,statefulset -n yolab-fb",
                &workloads.to_string(),
            )
            .ok("kubectl get configmap yolab-data-loss", "{}")
            .ok("kubectl patch configmap yolab-data-loss", "")
            .ok("kubectl scale", "")
            .ok("kubectl delete pod", "")
            // Stops at FailFilesystem so the test covers the hand-off, not the whole run.
            .fail("ceph fs ls", "mon unreachable");
        let err = tick(&host, &policy(), NOW).await.unwrap_err();
        assert!(err.to_string().contains("mon unreachable"));

        let calls = host.calls();
        let pos = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap();
        assert!(
            host.ran("force-create-pg 4.0"),
            "the image store is rebuilt straight away"
        );
        assert!(
            !host.ran("force-create-pg 2.1"),
            "metadata PGs wait for the filesystem to be failed"
        );
        assert!(pos("patch configmap yolab-data-loss") < pos("kubectl scale deployment/fb"));
        assert!(host.ran(r#"yolab-fb"#));
    }

    #[tokio::test]
    async fn nothing_inactive_purges_the_lost_osds() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "active+clean"}]});
        let host = healthy_cluster_host(&state)
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("ceph osd purge osd.1", "")
            .ok("ceph osd ls", "[0]");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("ceph osd purge osd.1 --yes-i-really-mean-it"));
    }

    #[tokio::test]
    async fn a_rebuild_resumes_at_its_recorded_step() {
        let rebuild = Rebuild {
            step: Step::CreateFilesystem,
            apps: vec![],
        };
        let state = serde_json::to_string(&HealState {
            rebuild: Some(rebuild),
            ..Default::default()
        })
        .unwrap();
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": state}}).to_string(),
            )
            .ok("ceph fs ls", "[]")
            .ok("ceph fs new", "")
            .ok("ceph fs subvolumegroup create", "")
            .ok("kubectl delete pod", "")
            .ok("kubectl-apply", "");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("ceph fs new yolab-fs yolab-fs-metadata yolab-fs-data0 --force"));
        assert!(!host.ran("rados purge"), "earlier steps are not repeated");
        assert!(!host.ran("fs fail"));
        assert!(host.ran("kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin"));
    }

    // ── State persistence ────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_unreadable_state_map_stops_the_tick_before_anything_is_touched() {
        let host = FakeHost::new().ok("ceph -s", "").fail(
            "kubectl get configmap yolab-storage-heal",
            "connection refused",
        );
        assert!(tick(&host, &policy(), NOW).await.is_err());
        assert_eq!(host.calls().len(), 2, "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_missing_state_map_is_a_healthy_cluster_not_an_error() {
        let host = FakeHost::new().fail(
            "kubectl get configmap yolab-storage-heal",
            "Error from server (NotFound): configmaps not found",
        );
        assert_eq!(read_state(&host).await.unwrap(), HealState::default());
    }

    #[tokio::test]
    async fn something_that_is_not_a_configmap_is_unreadable() {
        let host = FakeHost::new().ok("kubectl get configmap yolab-storage-heal", "{}");
        assert!(read_state(&host).await.is_err());
    }

    #[test]
    fn state_round_trips_through_json_with_numeric_osd_keys() {
        let s = HealState {
            gone_since: BTreeMap::from([(3, 10), (12, 20)]),
            lost: vec![7],
            rebuild: Some(Rebuild {
                step: Step::ReplaceVolumes,
                apps: vec![app("yolab-a")],
            }),
        };
        let raw = serde_json::to_string(&s).unwrap();
        assert!(raw.contains(r#""step":"replace_volumes""#));
        assert_eq!(serde_json::from_str::<HealState>(&raw).unwrap(), s);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_not_even_asked_about_its_state() {
        let host = FakeHost::new().fail("ceph -s", "timed out");
        tick(&host, &policy(), NOW).await.unwrap();
        assert_eq!(host.calls(), vec!["ceph -s".to_string()]);
    }

    // ── Observation details ──────────────────────────────────────────────────

    #[test]
    fn a_report_without_a_timestamp_or_with_an_old_one_is_not_fresh() {
        let status = json!({"data": {
            "node1": json!({"osd_map_known": true, "disks": {}}).to_string(),
            "node2": json!({"published_at": NOW - 301, "osd_map_known": true, "disks": {}}).to_string(),
            "node3": json!({"published_at": NOW - 300, "disks": {}}).to_string(),
            "node4": "not json",
        }});
        let o = parse_observed(&json!({}), &json!({}), &json!({}), &status, NOW);
        assert!(!o.disks["node1"].fresh);
        assert!(!o.disks["node2"].fresh);
        assert!(o.disks["node3"].fresh);
        assert!(
            !o.disks["node3"].osd_map_known,
            "a report that does not say is not known"
        );
        assert!(!o.disks.contains_key("node4"));
    }

    #[test]
    fn a_node_without_a_ready_condition_is_not_ready() {
        let nodes = json!({"items": [{"metadata": {"name": "node1"}, "status": {}}]});
        let o = parse_observed(&json!({}), &json!({}), &nodes, &json!({}), NOW);
        assert!(o.known_nodes.contains("node1"));
        assert!(!o.ready_nodes.contains("node1"));
    }

    #[test]
    fn only_host_buckets_place_osds() {
        let tree = json!({"nodes": [
            {"id": -1, "type": "root", "name": "default", "children": [-3, 5]},
            {"id": -3, "type": "host", "name": "node1", "children": [0, -9]},
        ]});
        let o = parse_observed(&json!({}), &tree, &json!({}), &json!({}), NOW);
        assert_eq!(o.osd_host.len(), 1);
        assert_eq!(o.osd_host[&0], "node1");
    }

    #[test]
    fn an_osd_without_a_host_is_unknown() {
        let mut o = obs(true, true, &[0]);
        o.osd_host.remove(&1);
        assert_eq!(presence(&o, 1), Presence::Unknown);
    }

    #[test]
    fn a_node_with_no_report_at_all_is_unknown() {
        let mut o = obs(true, true, &[0]);
        o.disks.clear();
        assert_eq!(presence(&o, 1), Presence::Unknown);
    }

    // ── Clocks ───────────────────────────────────────────────────────────────

    #[test]
    fn an_osd_removed_from_the_cluster_loses_its_clock() {
        let mut state = HealState::default();
        state.gone_since.insert(9, NOW - 5);
        advance_clocks(&mut state, &obs(true, true, &[0]), &policy());
        assert!(!state.gone_since.contains_key(&9));
    }

    #[test]
    fn an_already_lost_osd_is_never_declared_twice() {
        let mut state = HealState {
            lost: vec![1],
            gone_since: BTreeMap::from([(1, NOW - 10_000)]),
            ..Default::default()
        };
        assert!(advance_clocks(&mut state, &obs(true, true, &[0]), &policy()).is_empty());
        assert_eq!(state.lost, vec![1]);
    }

    #[test]
    fn each_disk_keeps_its_own_clock() {
        let mut state = HealState::default();
        let mut o = obs(true, true, &[]);
        o.osds.insert(0, false);
        state.gone_since.insert(0, NOW - 900);
        assert_eq!(advance_clocks(&mut state, &o, &policy()), vec![0]);
        assert_eq!(state.gone_since[&1], NOW, "osd.1 only started counting now");
    }

    #[test]
    fn a_node_that_comes_back_ready_with_its_disk_resets_the_clock() {
        let mut state = HealState::default();
        let mut o = obs(false, false, &[]);
        advance_clocks(&mut state, &o, &policy());
        assert!(state.gone_since.contains_key(&1));
        o = obs(true, true, &[0, 1]);
        advance_clocks(&mut state, &o, &policy());
        assert!(state.gone_since.is_empty());
    }

    // ── Placement groups ─────────────────────────────────────────────────────

    #[test]
    fn lost_means_down_or_incomplete_and_nothing_else() {
        for s in [
            "down",
            "incomplete",
            "stale+down",
            "down+remapped",
            "incomplete+peered",
        ] {
            assert!(is_lost_state(s), "{s}");
        }
        for s in [
            "unknown",
            "stale",
            "peering",
            "creating",
            "activating",
            "downgrade",
        ] {
            assert!(!is_lost_state(s), "{s}");
        }
    }

    #[test]
    fn a_pg_in_a_pool_we_cannot_name_counts_as_inactive_but_not_lost() {
        let pgs = json!([{"pgid": "99.1", "state": "down"}]);
        let pic = pg_picture(&dump(), &pgs).unwrap();
        assert_eq!(pic.inactive, 1);
        assert!(pic.lost.is_empty());
    }

    #[test]
    fn an_osd_dump_without_pools_gives_no_picture() {
        assert!(pg_picture(&json!({}), &json!({"pg_stats": []})).is_none());
        assert!(pg_picture(&dump(), &json!({})).is_none());
    }

    #[test]
    fn active_states_are_never_inactive() {
        let pgs = json!({"pg_stats": [
            {"pgid": "2.0", "state": "active+undersized+degraded"},
            {"pgid": "2.1", "state": "active+recovery_unfound+degraded"},
        ]});
        assert_eq!(pg_picture(&dump(), &pgs).unwrap(), PgPicture::default());
    }

    // ── Tick decisions ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn if_the_osd_cannot_be_marked_out_it_is_not_declared_lost() {
        let state = json!({"gone_since": {"1": NOW - 901}}).to_string();
        let host = healthy_cluster_host(&state).fail("ceph osd out osd.1", "EPERM");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("ceph osd lost"));
        assert!(
            !host.calls().iter().any(|c| c.contains(r#"\"lost\":[1]"#)),
            "{:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn a_whole_machine_gone_past_its_grace_is_declared_lost() {
        let state = json!({"gone_since": {"0": NOW - 3600, "1": NOW - 3600}}).to_string();
        let mut d = dump();
        d["osds"] = json!([{"osd": 0, "up": 0}, {"osd": 1, "up": 0}]);
        let host = cluster_host_ready(&state, d, false)
            .ok("ceph osd out", "")
            .ok("ceph osd lost", "");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("ceph osd lost 0"));
        assert!(host.ran("ceph osd lost 1"));
    }

    #[tokio::test]
    async fn a_pool_yolab_does_not_manage_is_left_alone() {
        let state = json!({"lost": [1]}).to_string();
        let mut d = dump();
        d["pools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pool": 7, "pool_name": "someones-rbd"}));
        let pgs = json!({"pg_stats": [{"pgid": "7.3", "state": "down"}]});
        let host = cluster_host(&state, d).ok("ceph pg dump pgs_brief", &pgs.to_string());
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("force-create-pg"));
        assert!(
            !host.ran("osd purge"),
            "an inactive PG still blocks the purge"
        );
    }

    #[tokio::test]
    async fn only_the_image_store_lost_rebuilds_it_and_touches_no_app() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [
            {"pgid": "4.1", "state": "down"},
            {"pgid": "1.0", "state": "incomplete"},
        ]});
        let host = healthy_cluster_host(&state)
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("ceph osd force-create-pg", "");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("force-create-pg 4.1"));
        assert!(host.ran("force-create-pg 1.0"));
        assert!(!host.ran("yolab-data-loss"));
        assert!(!host.ran("fs fail"));
        assert!(!host.ran("kubectl scale"));
    }

    #[tokio::test]
    async fn only_file_data_lost_rebuilds_those_pgs_and_marks_apps_without_a_rebuild() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [{"pgid": "3.7", "state": "incomplete"}]});
        let pvcs = json!({"items": [
            pvc_item("yolab-fb", "data", STORAGE_CLASS),
            pvc_item("kube-system", "other", STORAGE_CLASS),
        ]});
        let host = healthy_cluster_host(&state)
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("kubectl get pvc -A", &pvcs.to_string())
            .ok("kubectl get deployment,statefulset", r#"{"items": []}"#)
            .ok("kubectl get configmap yolab-data-loss", "{}")
            .ok("kubectl patch configmap yolab-data-loss", "")
            .ok("ceph osd force-create-pg", "");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(host.ran("force-create-pg 3.7"));
        let patch = host
            .calls()
            .into_iter()
            .find(|c| c.starts_with("kubectl patch configmap yolab-data-loss"))
            .unwrap();
        assert!(patch.contains(r#"[\"yolab-fb\"]"#), "{patch}");
        assert!(!host.ran("fs fail"));
        assert!(!host.ran("kubectl scale"));
    }

    #[tokio::test]
    async fn a_purge_the_osd_list_does_not_confirm_keeps_the_osd_lost() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "active+clean"}]});
        let host = healthy_cluster_host(&state)
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("ceph osd purge", "")
            .ok("ceph osd ls", "[0, 1]");
        tick(&host, &policy(), NOW).await.unwrap();
        let written = host
            .calls()
            .into_iter()
            .rfind(|c| c.starts_with("kubectl-apply"))
            .unwrap();
        assert!(written.contains(r#"\"lost\":[1]"#), "{written}");
    }

    #[tokio::test]
    async fn pgs_still_peering_hold_the_purge_back() {
        let state = json!({"lost": [1]}).to_string();
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "peering"}]});
        let host = healthy_cluster_host(&state).ok("ceph pg dump pgs_brief", &pgs.to_string());
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("osd purge"));
        assert!(!host.ran("force-create-pg"));
    }

    #[tokio::test]
    async fn an_unreadable_pg_dump_changes_nothing() {
        let state = json!({"lost": [1]}).to_string();
        let host = healthy_cluster_host(&state).fail("ceph pg dump pgs_brief", "timeout");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("osd purge"));
        assert!(!host.ran("force-create-pg"));
    }

    #[tokio::test]
    async fn an_unreadable_cluster_shape_changes_nothing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": "{}"}}).to_string(),
            )
            .ok("ceph osd dump", &dump().to_string())
            .fail("ceph osd tree", "timeout");
        tick(&host, &policy(), NOW).await.unwrap();
        assert!(!host.ran("kubectl-apply"));
        assert!(!host.ran("ceph osd out"));
    }

    // ── Corrupted list ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn marking_merges_with_what_is_already_recorded() {
        let existing = json!({"data": {"apps": r#"["yolab-b","yolab-a"]"#}}).to_string();
        let host = FakeHost::new()
            .ok("kubectl get configmap yolab-data-loss", &existing)
            .ok("kubectl patch configmap yolab-data-loss", "");
        mark_corrupted(
            &host,
            ["yolab-c".to_string(), "yolab-a".to_string()].into_iter(),
        )
        .await
        .unwrap();
        let patch = host
            .calls()
            .into_iter()
            .find(|c| c.starts_with("kubectl patch"))
            .unwrap();
        assert!(
            patch.contains(r#"[\"yolab-a\",\"yolab-b\",\"yolab-c\"]"#),
            "{patch}"
        );
    }

    #[tokio::test]
    async fn the_map_is_created_when_it_does_not_exist_yet() {
        let host = FakeHost::new()
            .fail("kubectl patch configmap yolab-data-loss", "NotFound")
            .ok("kubectl patch configmap yolab-data-loss", "")
            .ok("kubectl create configmap yolab-data-loss", "");
        write_corrupted(&host, &["yolab-a".to_string()])
            .await
            .unwrap();
        assert!(host.ran("kubectl create configmap yolab-data-loss -n kube-system"));
    }

    // ── Rebuild steps ────────────────────────────────────────────────────────

    fn app(ns: &str) -> AppVolumes {
        AppVolumes {
            namespace: ns.into(),
            workloads: vec![
                Workload {
                    kind: "deployment".into(),
                    name: "web".into(),
                    replicas: 2,
                },
                Workload {
                    kind: "statefulset".into(),
                    name: "db".into(),
                    replicas: 1,
                },
            ],
            pvcs: vec![
                SavedPvc {
                    name: "data".into(),
                    volume_name: Some("pv-data".into()),
                    manifest: Some(
                        json!({"kind": "PersistentVolumeClaim", "metadata": {"name": "data"}}),
                    ),
                },
                SavedPvc {
                    name: "volsync-cache".into(),
                    volume_name: None,
                    manifest: None,
                },
            ],
        }
    }

    fn at(step: Step) -> Rebuild {
        Rebuild {
            step,
            apps: vec![app("yolab-a")],
        }
    }

    fn pvc_item(ns: &str, name: &str, class: &str) -> Value {
        json!({
            "metadata": {"name": name, "namespace": ns},
            "spec": {"storageClassName": class, "volumeName": format!("pv-{name}"),
                     "accessModes": ["ReadWriteMany"], "resources": {"requests": {"storage": "1Gi"}}},
        })
    }

    #[tokio::test]
    async fn stop_apps_scales_every_workload_to_zero_and_forces_pods_off() {
        let host = FakeHost::new()
            .ok("kubectl scale", "")
            .ok("kubectl delete pod", "");
        assert!(run_step(&host, &at(Step::StopApps)).await.unwrap());
        assert!(host.ran("kubectl scale deployment/web -n yolab-a --replicas=0"));
        assert!(host.ran("kubectl scale statefulset/db -n yolab-a --replicas=0"));
        assert!(host.ran("kubectl delete pod --all -n yolab-a --force --grace-period=0"));
    }

    #[tokio::test]
    async fn fail_filesystem_only_fails_one_that_exists() {
        let host = FakeHost::new()
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs fail", "");
        assert!(run_step(&host, &at(Step::FailFilesystem)).await.unwrap());
        assert!(host.ran("ceph fs fail yolab-fs"));

        let host = FakeHost::new().ok("ceph fs ls", "[]");
        assert!(run_step(&host, &at(Step::FailFilesystem)).await.unwrap());
        assert!(!host.ran("fs fail"));

        let host = FakeHost::new().fail("ceph fs ls", "timeout");
        assert!(run_step(&host, &at(Step::FailFilesystem)).await.is_err());
    }

    #[tokio::test]
    async fn recreate_pgs_rebuilds_only_the_filesystem_pools_and_waits_for_them() {
        let lost = json!({"pg_stats": [
            {"pgid": "2.1", "state": "down"},
            {"pgid": "3.2", "state": "incomplete"},
            {"pgid": "4.3", "state": "down"},
        ]});
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph pg dump pgs_brief", &lost.to_string())
            .ok("ceph osd force-create-pg", "");
        assert!(!run_step(&host, &at(Step::RecreatePgs)).await.unwrap());
        assert!(host.ran("force-create-pg 2.1"));
        assert!(host.ran("force-create-pg 3.2"));
        assert!(!host.ran("force-create-pg 4.3"), "not this step's pool");

        let creating = json!({"pg_stats": [{"pgid": "2.1", "state": "creating"}]});
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph pg dump pgs_brief", &creating.to_string());
        assert!(!run_step(&host, &at(Step::RecreatePgs)).await.unwrap());
        assert!(!host.ran("force-create-pg"));

        let settled = json!({"pg_stats": [
            {"pgid": "2.1", "state": "active+clean"},
            {"pgid": "4.3", "state": "down"},
        ]});
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph pg dump pgs_brief", &settled.to_string());
        assert!(
            run_step(&host, &at(Step::RecreatePgs)).await.unwrap(),
            "another pool's inactive PG does not hold the filesystem back"
        );
    }

    #[tokio::test]
    async fn purge_pools_empties_both_and_a_failure_is_retried() {
        let host = FakeHost::new().ok("rados purge", "");
        assert!(run_step(&host, &at(Step::PurgePools)).await.unwrap());
        assert!(host.ran("rados purge yolab-fs-metadata --yes-i-really-really-mean-it"));
        assert!(host.ran("rados purge yolab-fs-data0 --yes-i-really-really-mean-it"));

        let host = FakeHost::new().fail("rados purge yolab-fs-metadata", "EBUSY");
        assert!(run_step(&host, &at(Step::PurgePools)).await.is_err());
        assert!(!host.ran("rados purge yolab-fs-data0"));
    }

    #[tokio::test]
    async fn remove_filesystem_is_a_no_op_when_it_is_already_gone() {
        let host = FakeHost::new().ok("ceph fs ls", "[]");
        assert!(run_step(&host, &at(Step::RemoveFilesystem)).await.unwrap());
        assert!(!host.ran("fs rm"));

        let host = FakeHost::new()
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs rm", "");
        assert!(run_step(&host, &at(Step::RemoveFilesystem)).await.unwrap());
        assert!(host.ran("ceph fs rm yolab-fs --yes-i-really-mean-it"));
    }

    #[tokio::test]
    async fn create_filesystem_retries_the_group_without_recreating_the_filesystem() {
        let host = FakeHost::new()
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .fail("ceph fs subvolumegroup create", "no MDS yet");
        assert!(run_step(&host, &at(Step::CreateFilesystem)).await.is_err());
        assert!(!host.ran("fs new"));

        let host = FakeHost::new()
            .ok("ceph fs ls", "[]")
            .ok("ceph fs new", "")
            .ok("ceph fs subvolumegroup create", "");
        assert!(run_step(&host, &at(Step::CreateFilesystem)).await.unwrap());
        assert!(host.ran("ceph fs subvolumegroup create yolab-fs csi"));
    }

    #[tokio::test]
    async fn replace_volumes_waits_for_the_old_claims_to_be_gone_before_recreating() {
        let host = FakeHost::new()
            .ok("kubectl get pvc data -n yolab-a", "exists")
            .fail("kubectl get pvc volsync-cache -n yolab-a", "NotFound")
            .ok("kubectl patch", "")
            .ok("kubectl delete", "");
        assert!(!run_step(&host, &at(Step::ReplaceVolumes)).await.unwrap());
        assert!(host.ran(
            r#"kubectl patch pvc data -n yolab-a --type merge -p {"metadata":{"finalizers":null}}"#
        ));
        assert!(host.ran("kubectl delete pvc data -n yolab-a --wait=false"));
        assert!(host
            .ran(r#"kubectl patch pv pv-data --type merge -p {"metadata":{"finalizers":null}}"#));
        assert!(host.ran("kubectl delete pv pv-data"));
        assert!(
            !host.ran("kubectl-apply"),
            "never recreated while the old one lingers"
        );
    }

    #[tokio::test]
    async fn replace_volumes_recreates_app_claims_but_not_volsyncs() {
        let host = FakeHost::new()
            .ok("kubectl get pvc data -n yolab-a", "exists")
            .fail(
                "kubectl get pvc data -n yolab-a",
                "Error from server (NotFound)",
            )
            .fail(
                "kubectl get pvc volsync-cache -n yolab-a",
                "Error from server (NotFound)",
            )
            .ok("kubectl patch", "")
            .ok("kubectl delete", "")
            .ok("kubectl-apply", "");
        assert!(run_step(&host, &at(Step::ReplaceVolumes)).await.unwrap());
        let applied: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("kubectl-apply"))
            .collect();
        assert_eq!(applied.len(), 1, "{applied:?}");
        assert!(applied[0].contains(r#""name":"data""#));
    }

    #[tokio::test]
    async fn replace_volumes_stops_when_a_claim_cannot_be_read() {
        let host = FakeHost::new().fail("kubectl get pvc data -n yolab-a", "connection refused");
        assert!(run_step(&host, &at(Step::ReplaceVolumes)).await.is_err());
        assert!(!host.ran("kubectl delete"));
    }

    #[tokio::test]
    async fn restart_csi_bounces_the_node_plugin_and_the_provisioner() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        assert!(run_step(&host, &at(Step::RestartCsi)).await.unwrap());
        assert!(host.ran("-l app=csi-cephfsplugin --wait=false"));
        assert!(host.ran("-l app=csi-cephfsplugin-provisioner --wait=false"));
    }

    #[tokio::test]
    async fn start_apps_puts_back_the_recorded_replica_counts() {
        let host = FakeHost::new().ok("kubectl scale", "");
        assert!(run_step(&host, &at(Step::StartApps)).await.unwrap());
        assert!(host.ran("kubectl scale deployment/web -n yolab-a --replicas=2"));
        assert!(host.ran("kubectl scale statefulset/db -n yolab-a --replicas=1"));

        let host = FakeHost::new().fail("kubectl scale", "forbidden");
        assert!(run_step(&host, &at(Step::StartApps)).await.is_err());
    }

    #[tokio::test]
    async fn a_step_that_is_not_finished_persists_where_it_stopped() {
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok(
                "ceph pg dump pgs_brief",
                &json!({"pg_stats": [{"pgid": "2.1", "state": "creating"}]}).to_string(),
            )
            .ok("kubectl-apply", "");
        let mut state = HealState {
            rebuild: Some(at(Step::RecreatePgs)),
            ..Default::default()
        };
        continue_rebuild(&host, &mut state).await.unwrap();
        assert_eq!(state.rebuild.unwrap().step, Step::RecreatePgs);
        assert!(!host.ran("rados"));
    }

    #[tokio::test]
    async fn a_whole_rebuild_runs_in_order_across_ticks_and_finishes() {
        let lost = json!({"pg_stats": [{"pgid": "2.1", "state": "down"}]});
        let active = json!({"pg_stats": [{"pgid": "2.1", "state": "active+clean"}]});
        let host = FakeHost::new()
            .ok("kubectl scale", "")
            .ok("kubectl delete", "")
            .ok("kubectl patch", "")
            .ok("kubectl-apply", "")
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs ls", "[]")
            .ok("ceph fs fail", "")
            .ok("ceph fs rm", "")
            .ok("ceph fs new", "")
            .ok("ceph fs subvolumegroup create", "")
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph pg dump pgs_brief", &lost.to_string())
            .ok("ceph pg dump pgs_brief", &lost.to_string())
            .ok("ceph pg dump pgs_brief", &active.to_string())
            .ok("ceph osd force-create-pg", "")
            .ok("rados purge", "")
            .ok("kubectl get pvc data -n yolab-a", "exists")
            .fail("kubectl get pvc data -n yolab-a", "NotFound")
            .fail("kubectl get pvc volsync-cache -n yolab-a", "NotFound");
        let mut state = HealState {
            rebuild: Some(at(Step::StopApps)),
            ..Default::default()
        };

        continue_rebuild(&host, &mut state).await.unwrap();
        assert_eq!(
            state.rebuild.as_ref().unwrap().step,
            Step::RecreatePgs,
            "the first tick stops once the PGs are asked to rebuild"
        );
        continue_rebuild(&host, &mut state).await.unwrap();
        assert!(
            state.rebuild.is_none(),
            "the second tick finishes: {state:?}"
        );

        let calls = host.calls();
        let pos = |needle: &str| {
            calls
                .iter()
                .position(|c| c.contains(needle))
                .unwrap_or_else(|| panic!("never ran {needle}: {calls:#?}"))
        };
        let order = [
            "--replicas=0",
            "ceph fs fail",
            "force-create-pg 2.1",
            "rados purge yolab-fs-metadata",
            "ceph fs rm",
            "ceph fs new",
            "subvolumegroup create",
            "kubectl delete pv pv-data",
            r#"kubectl-apply {"kind":"PersistentVolumeClaim""#,
            "app=csi-cephfsplugin",
            "--replicas=2",
        ];
        for w in order.windows(2) {
            assert!(pos(w[0]) < pos(w[1]), "{} must come before {}", w[0], w[1]);
        }
    }

    #[tokio::test]
    async fn snapshot_apps_keeps_only_apps_with_their_own_volume_on_cephfs() {
        let pvcs = json!({"items": [
            pvc_item("yolab-a", "data", STORAGE_CLASS),
            pvc_item("yolab-a", "volsync-a-cache", STORAGE_CLASS),
            pvc_item("yolab-b", "volsync-b-cache", STORAGE_CLASS),
            pvc_item("yolab-c", "local", "local-path"),
        ]});
        let workloads = json!({"items": [
            {"kind": "StatefulSet", "metadata": {"name": "db"}, "spec": {}},
        ]});
        let host = FakeHost::new()
            .ok("kubectl get pvc -A", &pvcs.to_string())
            .ok(
                "kubectl get deployment,statefulset -n yolab-a",
                &workloads.to_string(),
            );
        let apps = snapshot_apps(&host).await.unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].namespace, "yolab-a");
        assert_eq!(apps[0].pvcs.len(), 2);
        assert!(apps[0]
            .pvcs
            .iter()
            .any(|p| p.name == "volsync-a-cache" && p.manifest.is_none()));
        assert_eq!(
            apps[0].workloads,
            vec![Workload {
                kind: "statefulset".into(),
                name: "db".into(),
                replicas: 1
            }]
        );
    }

    #[test]
    fn a_recreated_pvc_drops_the_last_applied_annotation_and_defaults_its_mode() {
        let p = json!({
            "metadata": {"name": "d", "namespace": "n", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "{...}",
                "volume.beta.kubernetes.io/storage-provisioner": "x",
            }},
            "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}},
        });
        let m = recreatable_pvc(&p);
        assert_eq!(m["metadata"]["annotations"], json!({}));
        assert_eq!(m["spec"]["volumeMode"], "Filesystem");
        assert_eq!(m["spec"]["storageClassName"], STORAGE_CLASS);
        assert_eq!(m["metadata"]["labels"], json!({}));
    }
}
