//! Lost disks: record the loss, heal what is disposable, and on request reset
//! storage and reinstall every app from backup.
//!
//! THREE PHASES, AND ONLY THE LAST ONE DESTROYS ANYTHING.
//!
//! 1. Notice. A disk is gone once its OSD has stayed down past a grace period with
//!    positive proof: the disk is absent from its machine's fresh inventory (15 min),
//!    or the machine itself has been NotReady (60 min). A reboot, a deploy or a
//!    machine that merely cannot be read never counts.
//!
//! 2. Record and self-heal. Placement groups whose every copy sat on gone disks are
//!    written down right away, because this is the only moment the evidence exists:
//!    `nix/tests/disk-loss.nix` showed that once an OSD is declared lost, Ceph brings
//!    such a placement group back as `active+clean` and EMPTY. The gone OSDs are then
//!    marked `out` — reversible, it moves nothing when there is no other copy — so
//!    the placement groups of the two pools a machine can always refill (`images`,
//!    `.mgr`) are rebuilt empty on the disks that remain. App data is not touched:
//!    plugging the disk back in brings every file back, and the record clears itself.
//!
//! 3. Recover, when the owner presses "Recover health from backup". Not a repair —
//!    a reset. Nothing a half-broken cluster reports is trusted:
//!
//!    purge the gone OSDs → remove every app → delete the CephFS pools →
//!    recreate them → restart the CSI driver → reinstall every app in the newest
//!    backup, with its settings and its volumes at the snapshots that backup pinned
//!
//!    Apps that are not in the backup do not come back. Every step is persisted before
//!    and after it runs, so a restart resumes the run instead of starting another.
//!
//! WHAT CANNOT RECOVER. A cluster that loses Ceph monitor quorum or the Kubernetes
//! API cannot run any of this; with one mon and one etcd member per machine, losing
//! one of two machines stops both.
//!
//! Single writer: the loop acts only on the disk-reconciler lease holder.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use anyhow::{bail, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::host::{Host, RealHost};
use crate::AppState;

const STATE_CM: &str = "yolab-storage-heal";
const STATE_NS: &str = "kube-system";
const DISK_STATUS_CM: &str = "yolab-disk-status";
const DISK_NS: &str = "rook-ceph";
const CSI_NS: &str = "rook-ceph";
const MANAGED_SELECTOR: &str = "yolab.io/managed=true";

const FS_NAME: &str = "yolab-fs";
const FS_META_POOL: &str = "yolab-fs-metadata";
const FS_DATA_POOL: &str = "yolab-fs-data0";
const FS_SUBVOLUME_GROUP: &str = "csi";

/// Pools holding nothing a machine cannot fetch or regenerate again.
const DISPOSABLE_POOLS: &[&str] = &[".mgr", "images"];

/// A node's disk status older than this is not evidence of anything. The
/// reconciler publishes every 30s.
const STATUS_FRESH_SECS: u64 = 300;
const TICK: Duration = Duration::from_secs(30);

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

type PgsByPool = BTreeMap<String, BTreeSet<String>>;

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
struct HealState {
    /// OSD id → unix time it was first seen gone.
    #[serde(default)]
    gone_since: BTreeMap<i64, u64>,
    #[serde(default)]
    loss: Option<Loss>,
    /// The current or most recent recovery.
    #[serde(default)]
    recovery: Option<Recovery>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Loss {
    osds: BTreeSet<i64>,
    /// Placement groups with no copy left, by pool name.
    pgs: PgsByPool,
    /// Disposable placement groups already rebuilt empty.
    #[serde(default)]
    rebuilt: BTreeSet<String>,
    detected_at: u64,
}

impl Loss {
    /// Whether anything other than disposable data is gone.
    fn needs_recovery(&self) -> bool {
        self.pgs
            .keys()
            .any(|pool| !DISPOSABLE_POOLS.contains(&pool.as_str()))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Recovery {
    step: Step,
    started_at: u64,
    #[serde(default)]
    finished_at: Option<u64>,
    osds: BTreeSet<i64>,
    /// Every app installed when the recovery started, by namespace.
    removed: Vec<String>,
    /// The apps in the newest backup, decided when reinstalling begins.
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    outcomes: BTreeMap<String, AppOutcome>,
}

impl Recovery {
    fn running(&self) -> bool {
        self.finished_at.is_none()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Step {
    PurgeOsds,
    RemoveApps,
    DeleteStorage,
    RecreateStorage,
    RestartCsi,
    ReinstallApps,
}

impl Step {
    fn next(self) -> Option<Step> {
        use Step::*;
        Some(match self {
            PurgeOsds => RemoveApps,
            RemoveApps => DeleteStorage,
            DeleteStorage => RecreateStorage,
            RecreateStorage => RestartCsi,
            RestartCsi => ReinstallApps,
            ReinstallApps => return None,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
enum AppOutcome {
    Restored,
    Failed { error: String },
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
        // acting without knowing whether a recovery is half done is how two start.
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

pub(crate) async fn is_recovering() -> bool {
    read_state(&RealHost)
        .await
        .is_ok_and(|s| s.recovery.as_ref().is_some_and(Recovery::running))
}

/// Why a backup must not run right now, if it must not. Backing up while app data
/// is lost — or while a recovery is putting apps back — would upload broken or
/// empty volumes as the newest copy of those apps.
pub(crate) async fn backups_blocked() -> Option<&'static str> {
    let state = read_state(&RealHost).await.ok()?;
    blocked_reason(&state)
}

fn blocked_reason(state: &HealState) -> Option<&'static str> {
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        return Some("storage is being recovered from backup");
    }
    if state.loss.as_ref().is_some_and(Loss::needs_recovery) {
        return Some("a disk holding app data is gone — reconnect it or recover from backup first");
    }
    None
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
    in_osds: HashSet<i64>,
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

/// Advance every down OSD's clock and return the ones proven gone past their grace.
fn advance_clocks(state: &mut HealState, obs: &Observed, policy: &HealPolicy) -> BTreeSet<i64> {
    state.gone_since.retain(|id, _| obs.osds.contains_key(id));
    let mut gone = BTreeSet::new();
    for (&id, &up) in &obs.osds {
        if up {
            state.gone_since.remove(&id);
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
            gone.insert(id);
        }
    }
    gone
}

fn parse_observed(dump: &Value, tree: &Value, nodes: &Value, status: &Value, now: u64) -> Observed {
    let mut obs = Observed {
        now,
        ..Default::default()
    };
    for o in dump["osds"].as_array().into_iter().flatten() {
        if let Some(id) = o["osd"].as_i64() {
            obs.osds.insert(id, o["up"].as_i64() == Some(1));
            if o["in"].as_i64() == Some(1) {
                obs.in_osds.insert(id);
            }
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

fn pg_items(pgs: &Value) -> &[Value] {
    pgs["pg_stats"]
        .as_array()
        .or_else(|| pgs.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn pool_names(dump: &Value) -> HashMap<i64, String> {
    dump["pools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["pool"].as_i64()?, p["pool_name"].as_str()?.to_string())))
        .collect()
}

fn osd_list(v: &Value) -> Option<Vec<i64>> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
        .filter(|a| !a.is_empty())
}

/// Placement groups whose every copy is on a gone OSD, by pool name. Read while the
/// OSDs are still `in`: that is when the acting set still names where the data was.
fn lost_pgs(dump: &Value, pgs: &Value, gone: &BTreeSet<i64>) -> PgsByPool {
    let names = pool_names(dump);
    let mut lost = PgsByPool::new();
    if gone.is_empty() {
        return lost;
    }
    for pg in pg_items(pgs) {
        let Some(pgid) = pg["pgid"].as_str() else {
            continue;
        };
        let Some(holders) = osd_list(&pg["acting"]).or_else(|| osd_list(&pg["up"])) else {
            continue;
        };
        if !holders.iter().all(|id| gone.contains(id)) {
            continue;
        }
        let Some(pool) = pgid
            .split('.')
            .next()
            .and_then(|p| p.parse::<i64>().ok())
            .and_then(|id| names.get(&id))
        else {
            continue;
        };
        lost.entry(pool.clone())
            .or_default()
            .insert(pgid.to_string());
    }
    lost
}

/// Folds what this tick found into the record, which only ever grows until the
/// disks come back or a recovery consumes it.
fn merge_loss(loss: &mut Option<Loss>, gone: &BTreeSet<i64>, found: PgsByPool, now: u64) {
    if found.is_empty() {
        return;
    }
    let l = loss.get_or_insert_with(|| Loss {
        osds: BTreeSet::new(),
        pgs: PgsByPool::new(),
        rebuilt: BTreeSet::new(),
        detected_at: now,
    });
    l.osds.extend(gone.iter().copied());
    for (pool, ids) in found {
        l.pgs.entry(pool).or_default().extend(ids);
    }
}

/// Every recorded OSD running again means reconnecting worked: nothing is lost.
fn reconnected(loss: &Loss, obs: &Observed) -> bool {
    loss.osds.iter().all(|id| obs.osds.get(id) == Some(&true))
}

// ── The loop ──────────────────────────────────────────────────────────────────

/// Reinstalling apps from backup. A seam so the recovery can be tested without
/// restic, VolSync or helm.
pub(crate) trait AppRecovery: Send + Sync {
    /// Every app in the newest backup, by namespace.
    fn backed_up_apps(&self) -> impl Future<Output = Result<Vec<String>>> + Send + '_;
    fn reinstall<'a>(&'a self, namespace: &'a str) -> impl Future<Output = Result<()>> + Send + 'a;
}

pub(crate) struct RealApps;

#[allow(clippy::manual_async_fn)]
impl AppRecovery for RealApps {
    fn backed_up_apps(&self) -> impl Future<Output = Result<Vec<String>>> + Send + '_ {
        async move { Ok(crate::routers::restore::backup_contents().await?.apps) }
    }
    fn reinstall<'a>(&'a self, namespace: &'a str) -> impl Future<Output = Result<()>> + Send + 'a {
        async move { crate::routers::restore::reinstall_from_backup(namespace).await }
    }
}

pub async fn run() {
    tokio::time::sleep(Duration::from_secs(120)).await;
    let policy = HealPolicy::from_env();
    loop {
        if crate::disks_reconciler::is_reconcile_leader().await
            && !crate::routers::restore::is_running().await
        {
            if let Err(e) = tick(&RealHost, &RealApps, &policy, now_secs()).await {
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

async fn tick<H: Host, A: AppRecovery>(
    host: &H,
    apps: &A,
    policy: &HealPolicy,
    now: u64,
) -> Result<()> {
    if !host.reachable().await {
        return Ok(());
    }
    let mut state = read_state(host).await?;
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        return continue_recovery(host, apps, &mut state, now).await;
    }

    let dump = host.ceph_json(&["osd", "dump"]).await?;
    let tree = host.ceph_json(&["osd", "tree"]).await?;
    let nodes = host.kubectl_json(&["get", "nodes", "-o", "json"]).await?;
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
        .await?;
    let obs = parse_observed(&dump, &tree, &nodes, &status, now);

    let before = state.clone();
    let gone = advance_clocks(&mut state, &obs, policy);

    if state.loss.as_ref().is_some_and(|l| reconnected(l, &obs)) {
        tracing::info!("storage-heal: every lost disk is back — nothing needs recovering");
        state.loss = None;
    }
    if !gone.is_empty() {
        // Unreadable placement groups skip the record this tick rather than record
        // nothing: an empty answer here is not proof that nothing was lost.
        if let Ok(pgs) = host.ceph_json(&["pg", "dump", "pgs_brief"]).await {
            merge_loss(&mut state.loss, &gone, lost_pgs(&dump, &pgs, &gone), now);
        }
    }
    if state != before {
        // Persisted BEFORE anything below changes where Ceph maps those groups.
        write_state(host, &state).await?;
    }

    let Some(loss) = state.loss.clone() else {
        return Ok(());
    };
    for id in &loss.osds {
        if obs.osds.get(id) == Some(&false) && obs.in_osds.contains(id) {
            tracing::warn!(
                "storage-heal: osd.{id} is gone — marking it out so what can be rebuilt is \
                 rebuilt on the disks that remain (reconnecting it brings it back in)"
            );
            if let Err(e) = host.ceph(&["osd", "out", &format!("osd.{id}")]).await {
                tracing::warn!("storage-heal: could not mark osd.{id} out: {e}");
            }
        }
    }
    // Only once they are all out as of this tick's observation, so the rebuilt
    // groups map to a disk that can hold them rather than to the missing one.
    if !loss.osds.iter().all(|id| !obs.in_osds.contains(id)) {
        return Ok(());
    }
    let mut changed = false;
    for pool in DISPOSABLE_POOLS {
        for pg in loss.pgs.get(*pool).into_iter().flatten() {
            if loss.rebuilt.contains(pg) {
                continue;
            }
            match host
                .ceph(&["osd", "force-create-pg", pg, "--yes-i-really-mean-it"])
                .await
            {
                Ok(_) => {
                    tracing::info!("storage-heal: rebuilt {pg} ({pool}) empty");
                    if let Some(l) = state.loss.as_mut() {
                        l.rebuilt.insert(pg.clone());
                        changed = true;
                    }
                }
                Err(e) => tracing::warn!("storage-heal: could not rebuild {pg}: {e}"),
            }
        }
    }
    if changed {
        write_state(host, &state).await?;
    }
    Ok(())
}

// ── Starting a recovery ───────────────────────────────────────────────────────

async fn managed_namespaces<H: Host>(host: &H) -> Result<Vec<String>> {
    let v = host
        .kubectl_json(&["get", "namespaces", "-l", MANAGED_SELECTOR, "-o", "json"])
        .await?;
    Ok(v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["metadata"]["name"].as_str().map(str::to_string))
        .collect())
}

async fn start_recovery<H: Host>(host: &H, now: u64) -> Result<()> {
    let mut state = read_state(host).await?;
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        bail!("a recovery is already running");
    }
    let Some(loss) = state.loss.clone() else {
        bail!("no data has been lost — there is nothing to recover");
    };
    if !loss.needs_recovery() {
        bail!("only data that rebuilds itself was lost — there is nothing to restore");
    }
    let dump = host.ceph_json(&["osd", "dump"]).await?;
    let back = osds_up(&dump, &loss.osds);
    if !back.is_empty() {
        bail!("a lost disk is running again ({back:?}) — wait for it to catch up instead");
    }
    let removed = managed_namespaces(host).await?;
    tracing::warn!(
        "storage-heal: recovery from backup started — resetting storage, OSDs {:?}, {} app(s) \
         to remove",
        loss.osds,
        removed.len()
    );
    state.recovery = Some(Recovery {
        step: Step::PurgeOsds,
        started_at: now,
        finished_at: None,
        osds: loss.osds,
        removed,
        apps: None,
        outcomes: BTreeMap::new(),
    });
    write_state(host, &state).await
}

fn osds_up(dump: &Value, ids: &BTreeSet<i64>) -> Vec<i64> {
    dump["osds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["up"].as_i64() == Some(1))
        .filter_map(|o| o["osd"].as_i64())
        .filter(|id| ids.contains(id))
        .collect()
}

// ── Running a recovery ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum StepResult {
    Done,
    /// Look again next tick.
    NotYet,
    /// A lost disk came back before anything was destroyed.
    Cancel,
}

async fn continue_recovery<H: Host, A: AppRecovery>(
    host: &H,
    apps: &A,
    state: &mut HealState,
    now: u64,
) -> Result<()> {
    loop {
        let Some(recovery) = state.recovery.clone().filter(Recovery::running) else {
            return Ok(());
        };
        tracing::info!("storage-heal: recovery step {:?}", recovery.step);
        let result = if recovery.step == Step::ReinstallApps {
            reinstall_apps(host, apps, state).await?;
            StepResult::Done
        } else {
            run_step(host, &recovery).await?
        };
        match result {
            StepResult::NotYet => return Ok(()),
            StepResult::Cancel => {
                tracing::warn!(
                    "storage-heal: a lost disk came back before recovery destroyed anything — \
                     recovery cancelled"
                );
                state.recovery = None;
                return write_state(host, state).await;
            }
            StepResult::Done => {}
        }
        let r = state.recovery.as_mut().expect("checked above");
        match r.step.next() {
            Some(step) => r.step = step,
            None => {
                r.finished_at = Some(now);
                for id in &r.osds {
                    state.gone_since.remove(id);
                }
                state.loss = None;
                tracing::info!("storage-heal: recovery from backup finished");
            }
        }
        write_state(host, state).await?;
    }
}

async fn run_step<H: Host>(host: &H, r: &Recovery) -> Result<StepResult> {
    use StepResult::*;
    match r.step {
        Step::PurgeOsds => {
            let dump = host.ceph_json(&["osd", "dump"]).await?;
            if !osds_up(&dump, &r.osds).is_empty() {
                return Ok(Cancel);
            }
            let existing = host.osd_ids().await?;
            for id in r.osds.iter().filter(|id| existing.contains(id)) {
                if let Err(e) = host.osd_purge(*id).await {
                    tracing::warn!("storage-heal: could not purge osd.{id}: {e}");
                }
            }
            let left = host.osd_ids().await?;
            Ok(if r.osds.iter().any(|id| left.contains(id)) {
                NotYet
            } else {
                Done
            })
        }
        Step::RemoveApps => remove_apps(host).await,
        Step::DeleteStorage => {
            if fs_exists(host).await? {
                host.ceph(&["fs", "fail", FS_NAME]).await?;
                host.ceph(&["fs", "rm", FS_NAME, "--yes-i-really-mean-it"])
                    .await?;
            }
            let existing = pools(host).await?;
            let doomed: Vec<&str> = [FS_META_POOL, FS_DATA_POOL]
                .into_iter()
                .filter(|p| existing.contains(*p))
                .collect();
            if doomed.is_empty() {
                return Ok(Done);
            }
            // Pool deletion stays off everywhere else. It is switched on for exactly
            // these two names and switched off again whatever happened in between.
            host.ceph(&["config", "set", "mon", "mon_allow_pool_delete", "true"])
                .await?;
            let mut result = Ok(());
            for pool in doomed {
                if let Err(e) = host
                    .ceph(&[
                        "osd",
                        "pool",
                        "delete",
                        pool,
                        pool,
                        "--yes-i-really-really-mean-it",
                    ])
                    .await
                {
                    result = Err(e);
                    break;
                }
            }
            let off = host
                .ceph(&["config", "set", "mon", "mon_allow_pool_delete", "false"])
                .await;
            result?;
            off?;
            Ok(Done)
        }
        Step::RecreateStorage => {
            let existing = pools(host).await?;
            for (pool, pgs) in [(FS_META_POOL, "16"), (FS_DATA_POOL, "32")] {
                if !existing.contains(pool) {
                    host.ceph(&["osd", "pool", "create", pool, pgs, pgs])
                        .await?;
                }
            }
            if !fs_exists(host).await? {
                host.ceph(&["fs", "new", FS_NAME, FS_META_POOL, FS_DATA_POOL])
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
            Ok(Done)
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
            Ok(Done)
        }
        Step::ReinstallApps => unreachable!("handled by reinstall_apps"),
    }
}

/// Tears every app down without waiting on anything the broken filesystem holds:
/// pods are forced off, and the finalizers that would wait for the CSI driver to
/// delete a volume from a filesystem that cannot answer are removed.
async fn remove_apps<H: Host>(host: &H) -> Result<StepResult> {
    const NO_FINALIZERS: &str = r#"{"metadata":{"finalizers":null}}"#;
    let live = managed_namespaces(host).await?;
    for ns in &live {
        let ns = ns.as_str();
        let _ = host
            .kubectl(&[
                "scale",
                "deployment,statefulset",
                "--all",
                "-n",
                ns,
                "--replicas=0",
            ])
            .await;
        let _ = host
            .kubectl(&[
                "delete",
                "pod",
                "--all",
                "-n",
                ns,
                "--force",
                "--grace-period=0",
                "--wait=false",
            ])
            .await;
        let pvcs = host
            .kubectl_json(&["get", "pvc", "-n", ns, "-o", "json"])
            .await
            .unwrap_or(Value::Null);
        for p in pvcs["items"].as_array().into_iter().flatten() {
            let Some(name) = p["metadata"]["name"].as_str() else {
                continue;
            };
            let _ = host
                .kubectl(&[
                    "patch",
                    "pvc",
                    name,
                    "-n",
                    ns,
                    "--type",
                    "merge",
                    "-p",
                    NO_FINALIZERS,
                ])
                .await;
            if let Some(pv) = p["spec"]["volumeName"].as_str() {
                let _ = host
                    .kubectl(&["patch", "pv", pv, "--type", "merge", "-p", NO_FINALIZERS])
                    .await;
                let _ = host
                    .kubectl(&["delete", "pv", pv, "--wait=false", "--ignore-not-found"])
                    .await;
            }
        }
        let _ = host
            .kubectl(&[
                "delete",
                "namespace",
                ns,
                "--wait=false",
                "--ignore-not-found",
            ])
            .await;
    }
    Ok(if live.is_empty() {
        StepResult::Done
    } else {
        StepResult::NotYet
    })
}

/// Reinstalls every app in the newest backup, persisting the list first and each
/// outcome as it lands, so a restart never reinstalls the same app twice.
async fn reinstall_apps<H: Host, A: AppRecovery>(
    host: &H,
    apps: &A,
    state: &mut HealState,
) -> Result<()> {
    let list = match state.recovery.as_ref().and_then(|r| r.apps.clone()) {
        Some(list) => list,
        None => {
            let list = apps.backed_up_apps().await?;
            if let Some(r) = state.recovery.as_mut() {
                r.apps = Some(list.clone());
            }
            write_state(host, state).await?;
            list
        }
    };
    for ns in &list {
        if state
            .recovery
            .as_ref()
            .is_some_and(|r| r.outcomes.contains_key(ns))
        {
            continue;
        }
        let outcome = match apps.reinstall(ns).await {
            Ok(()) => AppOutcome::Restored,
            Err(e) => AppOutcome::Failed {
                error: format!("{e:#}"),
            },
        };
        tracing::info!("storage-heal: {ns}: {outcome:?}");
        if let Some(r) = state.recovery.as_mut() {
            r.outcomes.insert(ns.clone(), outcome);
        }
        write_state(host, state).await?;
    }
    Ok(())
}

async fn fs_exists<H: Host>(host: &H) -> Result<bool> {
    let ls = host.ceph_json(&["fs", "ls"]).await?;
    Ok(ls
        .as_array()
        .is_some_and(|a| a.iter().any(|f| f["name"].as_str() == Some(FS_NAME))))
}

async fn pools<H: Host>(host: &H) -> Result<HashSet<String>> {
    Ok(host
        .ceph(&["osd", "pool", "ls"])
        .await?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

fn instance_name(namespace: &str) -> &str {
    namespace.strip_prefix("yolab-").unwrap_or(namespace)
}

/// What the Backups page renders: the loss, and the current or last recovery.
fn status_json(state: &HealState) -> Value {
    let loss = state.loss.as_ref().map(|l| {
        json!({
            "osds": l.osds,
            "pools": l.pgs.keys().collect::<Vec<_>>(),
            "placement_groups": l.pgs.values().map(BTreeSet::len).sum::<usize>(),
            "needs_recovery": l.needs_recovery(),
            "detected_at": l.detected_at,
        })
    });
    let recovery = state.recovery.as_ref().map(|r| {
        let apps: Vec<Value> = r
            .apps
            .iter()
            .flatten()
            .map(|ns| {
                json!({
                    "namespace": ns,
                    "instance_name": instance_name(ns),
                    "outcome": r.outcomes.get(ns),
                })
            })
            .collect();
        // Known only once the backup has been read; until then nobody can say.
        let not_restored: Option<Vec<&str>> = r.apps.as_ref().map(|apps| {
            r.removed
                .iter()
                .filter(|ns| !apps.contains(ns))
                .map(|ns| instance_name(ns))
                .collect()
        });
        json!({
            "step": serde_json::to_value(r.step).unwrap_or(Value::Null),
            "running": r.running(),
            "started_at": r.started_at,
            "finished_at": r.finished_at,
            "apps": apps,
            "not_restored": not_restored,
        })
    });
    json!({ "loss": loss, "recovery": recovery })
}

/// What pressing the button would do: which apps come back, and which do not.
fn preview_json(installed: &[String], contents: &crate::routers::restore::BackupContents) -> Value {
    json!({
        "backup_taken_at": contents.taken_at,
        "restored": contents.apps.iter().map(|ns| instance_name(ns)).collect::<Vec<_>>(),
        "not_restored": installed
            .iter()
            .filter(|ns| !contents.apps.contains(ns))
            .map(|ns| instance_name(ns))
            .collect::<Vec<_>>(),
    })
}

/// `GET /api/storage/recovery`
pub async fn get_status(State(_s): State<AppState>) -> (StatusCode, Json<Value>) {
    match read_state(&RealHost).await {
        Ok(state) => (StatusCode::OK, Json(status_json(&state))),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// `POST /api/storage/recovery`
pub async fn post_recover(State(_s): State<AppState>) -> (StatusCode, Json<Value>) {
    match start_recovery(&RealHost, now_secs()).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// `GET /api/storage/recovery/preview`
pub async fn get_preview(State(_s): State<AppState>) -> (StatusCode, Json<Value>) {
    let installed = match managed_namespaces(&RealHost).await {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    match crate::routers::restore::backup_contents().await {
        Ok(contents) => (StatusCode::OK, Json(preview_json(&installed, &contents))),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": format!("could not read the backup: {e}") })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use std::sync::Mutex;

    const NOW: u64 = 1_000_000;

    fn policy() -> HealPolicy {
        HealPolicy {
            disk_grace: Duration::from_secs(900),
            node_grace: Duration::from_secs(3600),
        }
    }

    fn set<T: Ord + Clone>(items: &[T]) -> BTreeSet<T> {
        items.iter().cloned().collect()
    }

    fn pgs_by_pool(entries: &[(&str, &[&str])]) -> PgsByPool {
        entries
            .iter()
            .map(|(pool, ids)| {
                (
                    pool.to_string(),
                    ids.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── Presence and clocks ──────────────────────────────────────────────────

    /// node1 carries osd.0 and osd.1; osd.1 is down.
    fn obs(ready: bool, fresh: bool, reported: &[i64]) -> Observed {
        let mut o = Observed {
            now: NOW,
            ..Default::default()
        };
        o.osds.insert(0, true);
        o.osds.insert(1, false);
        o.in_osds.extend([0, 1]);
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
    fn a_stale_report_or_an_unknown_osd_map_proves_nothing() {
        assert_eq!(presence(&obs(true, false, &[0]), 1), Presence::Unknown);
        let mut o = obs(true, true, &[0]);
        o.disks.get_mut("node1").unwrap().osd_map_known = false;
        assert_eq!(presence(&o, 1), Presence::Unknown);
        o.disks.clear();
        assert_eq!(presence(&o, 1), Presence::Unknown);
    }

    #[test]
    fn a_not_ready_node_is_gone_and_an_unheard_of_host_is_unknown() {
        assert_eq!(presence(&obs(false, false, &[]), 1), Presence::NodeGone);
        let mut o = obs(true, true, &[0]);
        o.osd_host.insert(1, "somewhere-else".into());
        assert_eq!(presence(&o, 1), Presence::Unknown);
        o.osd_host.remove(&1);
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
        assert_eq!(advance_clocks(&mut state, &o, &policy()), set(&[1]));
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
        assert_eq!(advance_clocks(&mut state, &o, &policy()), set(&[0, 1]));
    }

    #[test]
    fn the_disk_coming_back_resets_the_clock() {
        let mut state = HealState::default();
        advance_clocks(&mut state, &obs(true, true, &[0]), &policy());
        advance_clocks(&mut state, &obs(true, true, &[0, 1]), &policy());
        assert!(state.gone_since.is_empty());
    }

    #[test]
    fn unknown_neither_starts_nor_clears_a_clock_and_never_counts() {
        let mut state = HealState::default();
        state.gone_since.insert(1, NOW - 10_000);
        assert!(advance_clocks(&mut state, &obs(true, false, &[0]), &policy()).is_empty());
        assert_eq!(state.gone_since.get(&1), Some(&(NOW - 10_000)));
    }

    #[test]
    fn an_osd_that_no_longer_exists_loses_its_clock() {
        let mut state = HealState::default();
        state.gone_since.insert(9, NOW - 5);
        advance_clocks(&mut state, &obs(true, true, &[0]), &policy());
        assert!(!state.gone_since.contains_key(&9));
    }

    #[test]
    fn each_disk_keeps_its_own_clock() {
        let mut state = HealState::default();
        let mut o = obs(true, true, &[]);
        o.osds.insert(0, false);
        state.gone_since.insert(0, NOW - 900);
        assert_eq!(advance_clocks(&mut state, &o, &policy()), set(&[0]));
        assert_eq!(state.gone_since[&1], NOW, "osd.1 only started counting now");
    }

    // ── Parsing ──────────────────────────────────────────────────────────────

    fn dump() -> Value {
        json!({
            "osds": [{"osd": 0, "up": 1, "in": 1}, {"osd": 1, "up": 0, "in": 1}],
            "pools": [
                {"pool": 1, "pool_name": ".mgr"},
                {"pool": 2, "pool_name": FS_META_POOL},
                {"pool": 3, "pool_name": FS_DATA_POOL},
                {"pool": 4, "pool_name": "images"},
            ]
        })
    }

    #[test]
    fn parse_observed_reads_all_four_sources() {
        let tree = json!({"nodes": [
            {"id": -1, "type": "root", "name": "default", "children": [-3, 5]},
            {"id": -3, "type": "host", "name": "node1", "children": [1, 0, -9]},
        ]});
        let nodes = json!({"items": [
            {"metadata": {"name": "node1"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}},
            {"metadata": {"name": "node2"}, "status": {"conditions": [{"type": "Ready", "status": "Unknown"}]}},
            {"metadata": {"name": "node3"}, "status": {}},
        ]});
        let status = json!({"data": {
            "node1": json!({"published_at": NOW - 20, "osd_map_known": true,
                            "disks": {"system": {"osd_id": 0}, "sdc": {}}}).to_string(),
            "node2": json!({"published_at": NOW - 301, "osd_map_known": true, "disks": {}}).to_string(),
            "node3": json!({"published_at": NOW - 300, "disks": {}}).to_string(),
            "node4": "not json",
        }});
        let o = parse_observed(&dump(), &tree, &nodes, &status, NOW);
        assert_eq!(o.osds.get(&1), Some(&false));
        assert!(o.in_osds.contains(&1));
        assert_eq!(o.osd_host.len(), 2, "only host buckets place OSDs");
        assert_eq!(o.osd_host[&1], "node1");
        assert!(o.ready_nodes.contains("node1"));
        assert!(!o.ready_nodes.contains("node2") && !o.ready_nodes.contains("node3"));
        assert!(o.known_nodes.contains("node3"));
        assert!(o.disks["node1"].fresh && o.disks["node1"].osd_map_known);
        assert_eq!(o.disks["node1"].osd_ids, HashSet::from([0]));
        assert!(!o.disks["node2"].fresh, "301s is stale");
        assert!(o.disks["node3"].fresh, "300s is not");
        assert!(
            !o.disks["node3"].osd_map_known,
            "a report that does not say is not known"
        );
        assert!(!o.disks.contains_key("node4"));
    }

    // ── Which placement groups are lost ──────────────────────────────────────

    #[test]
    fn a_pg_is_lost_only_when_every_holder_is_gone() {
        let pgs = json!({"pg_stats": [
            {"pgid": "2.1", "state": "stale+active+clean", "acting": [1], "up": [1]},
            {"pgid": "3.4", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "3.5", "state": "active+undersized", "acting": [0, 1]},
            {"pgid": "4.2", "state": "active+clean", "acting": [0]},
            {"pgid": "4.7", "state": "stale+down", "acting": [], "up": [1]},
            {"pgid": "1.0", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "9.9", "state": "stale", "acting": [1]},
            {"pgid": "2.2", "state": "unknown"},
        ]});
        assert_eq!(
            lost_pgs(&dump(), &pgs, &set(&[1])),
            pgs_by_pool(&[
                (".mgr", &["1.0"]),
                (FS_META_POOL, &["2.1"]),
                (FS_DATA_POOL, &["3.4"]),
                ("images", &["4.7"]),
            ])
        );
    }

    #[test]
    fn nothing_is_lost_while_no_disk_is_proven_gone() {
        let pgs = json!([{"pgid": "2.1", "state": "stale", "acting": [1]}]);
        assert!(lost_pgs(&dump(), &pgs, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn two_gone_disks_holding_both_copies_lose_the_pg() {
        let pgs = json!([{"pgid": "3.1", "state": "stale", "acting": [1, 2]}]);
        assert_eq!(
            lost_pgs(&dump(), &pgs, &set(&[1, 2])),
            pgs_by_pool(&[(FS_DATA_POOL, &["3.1"])])
        );
        assert!(lost_pgs(&dump(), &pgs, &set(&[1])).is_empty());
    }

    #[test]
    fn the_record_only_grows_and_starts_at_its_first_sighting() {
        let mut loss = None;
        merge_loss(&mut loss, &set(&[1]), PgsByPool::new(), NOW);
        assert!(
            loss.is_none(),
            "a gone disk that held nothing unique records nothing"
        );
        merge_loss(
            &mut loss,
            &set(&[1]),
            pgs_by_pool(&[("images", &["4.1"])]),
            NOW,
        );
        merge_loss(
            &mut loss,
            &set(&[2]),
            pgs_by_pool(&[("images", &["4.2"]), (FS_META_POOL, &["2.0"])]),
            NOW + 60,
        );
        let l = loss.unwrap();
        assert_eq!(l.detected_at, NOW);
        assert_eq!(l.osds, set(&[1, 2]));
        assert_eq!(
            l.pgs["images"],
            set(&["4.1".to_string(), "4.2".to_string()])
        );
        assert!(l.needs_recovery());
    }

    #[test]
    fn losing_only_disposable_pools_needs_no_recovery() {
        let l = Loss {
            osds: set(&[1]),
            pgs: pgs_by_pool(&[("images", &["4.1"]), (".mgr", &["1.0"])]),
            rebuilt: BTreeSet::new(),
            detected_at: NOW,
        };
        assert!(!l.needs_recovery());
    }

    // ── State ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_unreadable_state_map_stops_everything() {
        let host = FakeHost::new().ok("ceph -s", "").fail(
            "kubectl get configmap yolab-storage-heal",
            "connection refused",
        );
        assert!(tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .is_err());
        assert_eq!(host.calls().len(), 2, "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_missing_state_map_is_a_healthy_cluster_and_junk_is_unreadable() {
        let host = FakeHost::new().fail(
            "kubectl get configmap yolab-storage-heal",
            "Error from server (NotFound): configmaps not found",
        );
        assert_eq!(read_state(&host).await.unwrap(), HealState::default());
        let host = FakeHost::new().ok("kubectl get configmap yolab-storage-heal", "{}");
        assert!(read_state(&host).await.is_err());
    }

    #[test]
    fn state_round_trips_through_json() {
        let s = HealState {
            gone_since: BTreeMap::from([(3, 10), (12, 20)]),
            loss: Some(Loss {
                osds: set(&[3]),
                pgs: pgs_by_pool(&[(FS_META_POOL, &["2.1"])]),
                rebuilt: set(&["4.1".to_string()]),
                detected_at: 5,
            }),
            recovery: Some(Recovery {
                step: Step::ReinstallApps,
                started_at: 7,
                finished_at: None,
                osds: set(&[3]),
                removed: strings(&["yolab-a", "yolab-b"]),
                apps: Some(strings(&["yolab-a"])),
                outcomes: BTreeMap::from([(
                    "yolab-a".into(),
                    AppOutcome::Failed { error: "x".into() },
                )]),
            }),
        };
        let raw = serde_json::to_string(&s).unwrap();
        assert!(raw.contains(r#""step":"reinstall_apps""#));
        assert!(raw.contains(r#"{"result":"failed","error":"x"}"#));
        assert_eq!(serde_json::from_str::<HealState>(&raw).unwrap(), s);
    }

    #[test]
    fn backups_are_blocked_while_app_data_is_lost_or_being_recovered() {
        let mut s = HealState::default();
        assert_eq!(blocked_reason(&s), None);
        s.loss = Some(Loss {
            osds: set(&[1]),
            pgs: pgs_by_pool(&[("images", &["4.1"])]),
            rebuilt: BTreeSet::new(),
            detected_at: NOW,
        });
        assert_eq!(blocked_reason(&s), None, "only the image store is gone");
        s.loss.as_mut().unwrap().pgs = pgs_by_pool(&[(FS_DATA_POOL, &["3.1"])]);
        assert!(blocked_reason(&s).unwrap().contains("reconnect"));
        s.loss = None;
        s.recovery = Some(recovery_at(Step::RemoveApps));
        assert!(blocked_reason(&s).unwrap().contains("recovered"));
        s.recovery.as_mut().unwrap().finished_at = Some(NOW);
        assert_eq!(blocked_reason(&s), None);
    }

    // ── The tick ─────────────────────────────────────────────────────────────

    /// A null `pgs` leaves the PG dump unscripted so a test can script its failure:
    /// a second answer would queue behind this one rather than replace it.
    fn cluster_host(state: &Value, dump: &Value, pgs: &Value, reported: &[i64]) -> FakeHost {
        let tree =
            json!({"nodes": [{"id": -3, "type": "host", "name": "node1", "children": [0, 1]}]});
        let nodes = json!({"items": [{"metadata": {"name": "node1"},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}}]});
        let disks: serde_json::Map<String, Value> = reported
            .iter()
            .map(|id| (format!("d{id}"), json!({"osd_id": id})))
            .collect();
        let status = json!({"data": {"node1": json!({
            "published_at": NOW, "osd_map_known": true, "disks": disks
        }).to_string()}});
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": state.to_string()}}).to_string(),
            )
            .ok("ceph osd dump", &dump.to_string())
            .ok("ceph osd tree", &tree.to_string())
            .ok("kubectl get nodes", &nodes.to_string())
            .ok(
                "kubectl get configmap yolab-disk-status",
                &status.to_string(),
            )
            .ok("kubectl-apply", "");
        if pgs.is_null() {
            host
        } else {
            host.ok("ceph pg dump pgs_brief", &pgs.to_string())
        }
    }

    fn applied_states(host: &FakeHost) -> Vec<HealState> {
        host.calls()
            .iter()
            .filter_map(|c| c.strip_prefix("kubectl-apply "))
            .filter_map(|m| serde_json::from_str::<Value>(m).ok())
            .filter_map(|m| serde_json::from_str(m["data"]["state"].as_str()?).ok())
            .collect()
    }

    fn lost_pg_dump() -> Value {
        json!({"pg_stats": [
            {"pgid": "2.1", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "4.1", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "4.2", "state": "active+clean", "acting": [0]},
        ]})
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_not_even_asked_about_its_state() {
        let host = FakeHost::new().fail("ceph -s", "timed out");
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();
        assert_eq!(host.calls(), vec!["ceph -s".to_string()]);
    }

    #[tokio::test]
    async fn a_disk_inside_its_grace_period_is_only_timed() {
        let host = cluster_host(&json!({}), &dump(), &lost_pg_dump(), &[0]);
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();
        assert!(
            !host.ran("pg dump"),
            "no PG is judged before a disk is proven gone"
        );
        assert!(!host.ran("ceph osd out"));
        assert_eq!(applied_states(&host)[0].gone_since.get(&1), Some(&NOW));
    }

    #[tokio::test]
    async fn a_proven_gone_disk_is_recorded_before_it_is_marked_out() {
        let state = json!({"gone_since": {"1": NOW - 900}});
        let host = cluster_host(&state, &dump(), &lost_pg_dump(), &[0]).ok("ceph osd out", "");
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();

        let recorded = applied_states(&host)[0].loss.clone().expect("recorded");
        assert_eq!(recorded.osds, set(&[1]));
        assert_eq!(
            recorded.pgs,
            pgs_by_pool(&[(FS_META_POOL, &["2.1"]), ("images", &["4.1"])])
        );
        let calls = host.calls();
        let record = calls
            .iter()
            .position(|c| c.starts_with("kubectl-apply"))
            .unwrap();
        let out = calls
            .iter()
            .position(|c| c == "ceph osd out osd.1")
            .unwrap();
        assert!(
            record < out,
            "the evidence must be saved before it changes: {calls:#?}"
        );
        assert!(!host.ran("force-create-pg"), "not until the OSD is out");
        assert!(!host.ran("osd purge") && !host.ran("pool delete"));
    }

    #[tokio::test]
    async fn once_out_the_disposable_pools_rebuild_empty_and_app_data_is_left_alone() {
        let state = json!({
            "gone_since": {"1": NOW - 2000},
            "loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"], "images": ["4.1"], ".mgr": ["1.0"]},
                     "detected_at": NOW - 100},
        });
        let mut d = dump();
        d["osds"][1]["in"] = json!(0);
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "down", "acting": [0]}]});
        let host = cluster_host(&state, &d, &pgs, &[0]).ok("ceph osd force-create-pg", "");
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();

        assert!(host.ran("force-create-pg 4.1"));
        assert!(host.ran("force-create-pg 1.0"));
        assert!(!host.ran("force-create-pg 2.1"));
        assert!(!host.ran("ceph osd out"), "already out");
        assert!(!host.ran("fs fail") && !host.ran("pool delete") && !host.ran("osd purge"));
        let loss = applied_states(&host).pop().unwrap().loss.unwrap();
        assert_eq!(loss.rebuilt, set(&["1.0".to_string(), "4.1".to_string()]));
        assert_eq!(
            loss.pgs[FS_META_POOL],
            set(&["2.1".to_string()]),
            "the record is kept"
        );
    }

    #[tokio::test]
    async fn an_already_rebuilt_pg_is_not_rebuilt_again() {
        let state = json!({
            "gone_since": {"1": NOW - 2000},
            "loss": {"osds": [1], "pgs": {"images": ["4.1"]}, "rebuilt": ["4.1"], "detected_at": 1},
        });
        let mut d = dump();
        d["osds"][1]["in"] = json!(0);
        let host = cluster_host(&state, &d, &json!({"pg_stats": []}), &[0]);
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();
        assert!(!host.ran("force-create-pg"));
    }

    #[tokio::test]
    async fn plugging_the_disk_back_in_clears_the_record() {
        let state =
            json!({"loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"]}, "detected_at": 1}});
        let mut d = dump();
        d["osds"][1] = json!({"osd": 1, "up": 1, "in": 0});
        let host = cluster_host(&state, &d, &json!({}), &[0, 1]);
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();
        assert!(applied_states(&host).pop().unwrap().loss.is_none());
        assert!(!host.ran("force-create-pg"));
    }

    #[tokio::test]
    async fn an_unreadable_pg_dump_records_nothing_rather_than_an_empty_loss() {
        let state = json!({"gone_since": {"1": NOW - 900}});
        let host = cluster_host(&state, &dump(), &Value::Null, &[0])
            .fail("ceph pg dump pgs_brief", "timeout");
        tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .unwrap();
        assert!(applied_states(&host).iter().all(|s| s.loss.is_none()));
        assert!(!host.ran("ceph osd out"));
    }

    #[tokio::test]
    async fn an_unreadable_cluster_shape_is_an_error_and_changes_nothing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": "{}"}}).to_string(),
            )
            .ok("ceph osd dump", &dump().to_string())
            .fail("ceph osd tree", "timeout");
        assert!(tick(&host, &FakeApps::default(), &policy(), NOW)
            .await
            .is_err());
        assert!(!host.ran("kubectl-apply"));
    }

    #[tokio::test]
    async fn a_running_recovery_takes_over_the_tick() {
        let state = HealState {
            recovery: Some(recovery_at(Step::RestartCsi)),
            ..Default::default()
        };
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": serde_json::to_string(&state).unwrap()}})
                    .to_string(),
            )
            .ok("kubectl delete pod", "")
            .ok("kubectl-apply", "");
        let apps = FakeApps::failing_list();
        let err = tick(&host, &apps, &policy(), NOW).await.unwrap_err();
        assert!(err.to_string().contains("restic unreachable"), "{err}");
        assert!(!host.ran("ceph osd tree"), "no detection while recovering");
        assert!(host.ran("app=csi-cephfsplugin"));
    }

    // ── Starting a recovery ──────────────────────────────────────────────────

    fn start_host(state: &Value, dump: &Value) -> FakeHost {
        FakeHost::new()
            .ok(
                "kubectl get configmap yolab-storage-heal",
                &json!({"kind": "ConfigMap", "data": {"state": state.to_string()}}).to_string(),
            )
            .ok("ceph osd dump", &dump.to_string())
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                &json!({"items": [{"metadata": {"name": "yolab-a"}}, {"metadata": {"name": "yolab-b"}}]})
                    .to_string(),
            )
            .ok("kubectl-apply", "")
    }

    fn app_loss() -> Value {
        json!({"loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"]}, "detected_at": 1}})
    }

    #[tokio::test]
    async fn the_button_records_the_installed_apps_and_starts_by_purging_the_disks() {
        let host = start_host(&app_loss(), &dump());
        start_recovery(&host, NOW).await.unwrap();
        let r = applied_states(&host).pop().unwrap().recovery.unwrap();
        assert_eq!(r.step, Step::PurgeOsds);
        assert_eq!(r.started_at, NOW);
        assert_eq!(r.osds, set(&[1]));
        assert_eq!(r.removed, strings(&["yolab-a", "yolab-b"]));
        assert_eq!(r.apps, None, "the backup is read when reinstalling starts");
        assert!(
            !host.ran("osd purge"),
            "the loop does the work, not the request"
        );
    }

    #[tokio::test]
    async fn the_button_refuses_without_a_loss_to_recover() {
        let err = start_recovery(&start_host(&json!({}), &dump()), NOW)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing to recover"));

        let only_images =
            json!({"loss": {"osds": [1], "pgs": {"images": ["4.1"]}, "detected_at": 1}});
        let err = start_recovery(&start_host(&only_images, &dump()), NOW)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing to restore"));
    }

    #[tokio::test]
    async fn the_button_refuses_twice_and_refuses_when_the_disk_is_back() {
        let mut running = app_loss();
        running["recovery"] = serde_json::to_value(recovery_at(Step::RemoveApps)).unwrap();
        let err = start_recovery(&start_host(&running, &dump()), NOW)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already running"));

        let mut d = dump();
        d["osds"][1]["up"] = json!(1);
        let err = start_recovery(&start_host(&app_loss(), &d), NOW)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("running again"));
    }

    #[tokio::test]
    async fn a_finished_recovery_does_not_block_the_next_one() {
        let mut finished = recovery_at(Step::ReinstallApps);
        finished.finished_at = Some(1);
        let mut state = app_loss();
        state["recovery"] = serde_json::to_value(finished).unwrap();
        let host = start_host(&state, &dump());
        start_recovery(&host, NOW).await.unwrap();
        assert!(applied_states(&host)
            .pop()
            .unwrap()
            .recovery
            .unwrap()
            .running());
    }

    // ── Recovery steps ───────────────────────────────────────────────────────

    fn recovery_at(step: Step) -> Recovery {
        Recovery {
            step,
            started_at: NOW - 10,
            finished_at: None,
            osds: set(&[1]),
            removed: strings(&["yolab-a", "yolab-b"]),
            apps: None,
            outcomes: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn purge_osds_purges_the_recorded_disks_and_confirms_it() {
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph osd ls", "[0, 1]")
            .ok("ceph osd ls", "[0]")
            .ok("ceph osd purge", "");
        assert_eq!(
            run_step(&host, &recovery_at(Step::PurgeOsds))
                .await
                .unwrap(),
            StepResult::Done
        );
        assert!(host.ran("ceph osd purge osd.1 --yes-i-really-mean-it"));
    }

    #[tokio::test]
    async fn purge_osds_waits_when_the_list_still_has_the_disk() {
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph osd ls", "[0, 1]")
            .fail("ceph osd purge", "EBUSY");
        assert_eq!(
            run_step(&host, &recovery_at(Step::PurgeOsds))
                .await
                .unwrap(),
            StepResult::NotYet
        );
    }

    #[tokio::test]
    async fn purge_osds_skips_disks_already_purged_by_an_earlier_run() {
        let host = FakeHost::new()
            .ok("ceph osd dump", r#"{"osds": [{"osd": 0, "up": 1}]}"#)
            .ok("ceph osd ls", "[0]");
        assert_eq!(
            run_step(&host, &recovery_at(Step::PurgeOsds))
                .await
                .unwrap(),
            StepResult::Done
        );
        assert!(!host.ran("osd purge"));
    }

    #[tokio::test]
    async fn a_disk_coming_back_cancels_before_anything_is_destroyed() {
        let mut d = dump();
        d["osds"][1]["up"] = json!(1);
        let host = FakeHost::new()
            .ok("ceph osd dump", &d.to_string())
            .ok("kubectl-apply", "");
        let mut state = HealState {
            loss: serde_json::from_value(app_loss()["loss"].clone()).unwrap(),
            recovery: Some(recovery_at(Step::PurgeOsds)),
            ..Default::default()
        };
        continue_recovery(&host, &FakeApps::default(), &mut state, NOW)
            .await
            .unwrap();
        assert!(state.recovery.is_none());
        assert!(
            state.loss.is_some(),
            "the loop clears the record once it sees the disk up"
        );
        assert!(!host.ran("osd purge") && !host.ran("kubectl delete"));
    }

    #[tokio::test]
    async fn remove_apps_forces_every_app_off_without_waiting_on_the_filesystem() {
        let pvcs = json!({"items": [
            {"metadata": {"name": "data"}, "spec": {"volumeName": "pv-data"}},
            {"metadata": {"name": "pending"}, "spec": {}},
        ]});
        let host = FakeHost::new()
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": [{"metadata": {"name": "yolab-a"}}]}"#,
            )
            .ok("kubectl scale", "")
            .ok("kubectl delete", "")
            .ok("kubectl patch", "")
            .ok("kubectl get pvc -n yolab-a", &pvcs.to_string());
        assert_eq!(
            run_step(&host, &recovery_at(Step::RemoveApps))
                .await
                .unwrap(),
            StepResult::NotYet,
            "not done while a namespace is still listed"
        );
        let nf = r#"{"metadata":{"finalizers":null}}"#;
        for cmd in [
            "kubectl scale deployment,statefulset --all -n yolab-a --replicas=0".to_string(),
            "kubectl delete pod --all -n yolab-a --force --grace-period=0 --wait=false".to_string(),
            format!("kubectl patch pvc data -n yolab-a --type merge -p {nf}"),
            format!("kubectl patch pvc pending -n yolab-a --type merge -p {nf}"),
            format!("kubectl patch pv pv-data --type merge -p {nf}"),
            "kubectl delete pv pv-data --wait=false".to_string(),
            "kubectl delete namespace yolab-a --wait=false".to_string(),
        ] {
            assert!(host.ran(&cmd), "never ran {cmd}: {:#?}", host.calls());
        }
    }

    #[tokio::test]
    async fn remove_apps_is_done_once_no_app_namespace_is_left() {
        let host = FakeHost::new().ok(
            "kubectl get namespaces -l yolab.io/managed=true",
            r#"{"items": []}"#,
        );
        assert_eq!(
            run_step(&host, &recovery_at(Step::RemoveApps))
                .await
                .unwrap(),
            StepResult::Done
        );
        let host = FakeHost::new().fail("kubectl get namespaces", "API down");
        assert!(run_step(&host, &recovery_at(Step::RemoveApps))
            .await
            .is_err());
    }

    fn storage_host(fs: &str, pool_ls: &str) -> FakeHost {
        FakeHost::new()
            .ok("ceph fs ls", fs)
            .ok("ceph fs fail", "")
            .ok("ceph fs rm", "")
            .ok("ceph osd pool ls", pool_ls)
            .ok("ceph config set mon mon_allow_pool_delete", "")
            .ok("ceph osd pool delete", "")
    }

    #[tokio::test]
    async fn delete_storage_removes_the_filesystem_and_exactly_its_two_pools() {
        let host = storage_host(
            r#"[{"name": "yolab-fs"}]"#,
            ".mgr\nimages\nyolab-fs-metadata\nyolab-fs-data0\n",
        );
        assert_eq!(
            run_step(&host, &recovery_at(Step::DeleteStorage))
                .await
                .unwrap(),
            StepResult::Done
        );
        let calls = host.calls();
        let pos = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap();
        assert!(pos("ceph fs fail yolab-fs") < pos("ceph fs rm yolab-fs --yes-i-really-mean-it"));
        assert!(pos("mon_allow_pool_delete true") < pos("pool delete yolab-fs-metadata"));
        assert!(pos("pool delete yolab-fs-data0") < pos("mon_allow_pool_delete false"));
        assert!(host.ran(
            "ceph osd pool delete yolab-fs-metadata yolab-fs-metadata --yes-i-really-really-mean-it"
        ));
        assert!(!host.ran("pool delete images") && !host.ran("pool delete .mgr"));
    }

    #[tokio::test]
    async fn pool_deletion_is_switched_back_off_even_when_a_delete_fails() {
        let host = FakeHost::new()
            .ok("ceph fs ls", "[]")
            .ok("ceph osd pool ls", "yolab-fs-metadata\nyolab-fs-data0")
            .ok("ceph config set mon mon_allow_pool_delete", "")
            .fail("ceph osd pool delete yolab-fs-metadata", "EBUSY");
        assert!(run_step(&host, &recovery_at(Step::DeleteStorage))
            .await
            .is_err());
        assert!(host.ran("mon_allow_pool_delete false"));
        assert!(!host.ran("pool delete yolab-fs-data0"));
    }

    #[tokio::test]
    async fn delete_storage_on_a_resumed_run_with_nothing_left_touches_nothing() {
        let host = FakeHost::new()
            .ok("ceph fs ls", "[]")
            .ok("ceph osd pool ls", ".mgr\nimages");
        assert_eq!(
            run_step(&host, &recovery_at(Step::DeleteStorage))
                .await
                .unwrap(),
            StepResult::Done
        );
        assert!(!host.ran("mon_allow_pool_delete") && !host.ran("fs fail"));
    }

    #[tokio::test]
    async fn recreate_storage_makes_what_is_missing_and_nothing_else() {
        let host = FakeHost::new()
            .ok("ceph osd pool ls", ".mgr\nimages")
            .ok("ceph osd pool create", "")
            .ok("ceph fs ls", "[]")
            .ok("ceph fs new", "")
            .ok("ceph fs subvolumegroup create", "");
        assert_eq!(
            run_step(&host, &recovery_at(Step::RecreateStorage))
                .await
                .unwrap(),
            StepResult::Done
        );
        assert!(host.ran("ceph osd pool create yolab-fs-metadata 16 16"));
        assert!(host.ran("ceph osd pool create yolab-fs-data0 32 32"));
        assert!(host.ran("ceph fs new yolab-fs yolab-fs-metadata yolab-fs-data0"));
        assert!(host.ran("ceph fs subvolumegroup create yolab-fs csi"));

        let host = FakeHost::new()
            .ok("ceph osd pool ls", "yolab-fs-metadata\nyolab-fs-data0")
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .fail("ceph fs subvolumegroup create", "no MDS yet");
        assert!(run_step(&host, &recovery_at(Step::RecreateStorage))
            .await
            .is_err());
        assert!(!host.ran("pool create") && !host.ran("fs new"));
    }

    #[tokio::test]
    async fn restart_csi_bounces_the_node_plugin_and_the_provisioner() {
        let host = FakeHost::new().ok("kubectl delete pod", "");
        run_step(&host, &recovery_at(Step::RestartCsi))
            .await
            .unwrap();
        assert!(host.ran("-l app=csi-cephfsplugin --wait=false"));
        assert!(host.ran("-l app=csi-cephfsplugin-provisioner --wait=false"));
    }

    #[test]
    fn the_steps_run_in_the_order_that_keeps_every_resume_safe() {
        let mut s = Step::PurgeOsds;
        let mut order = vec![s];
        while let Some(n) = s.next() {
            order.push(n);
            s = n;
        }
        assert_eq!(
            order,
            vec![
                Step::PurgeOsds,
                Step::RemoveApps,
                Step::DeleteStorage,
                Step::RecreateStorage,
                Step::RestartCsi,
                Step::ReinstallApps,
            ],
            "apps go before their storage, storage comes back before apps return"
        );
    }

    // ── Reinstalling ─────────────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeApps {
        backed_up: Vec<String>,
        list_fails: bool,
        failing: HashMap<String, String>,
        reinstalled: Mutex<Vec<String>>,
    }

    impl FakeApps {
        fn with(backed_up: &[&str]) -> Self {
            Self {
                backed_up: strings(backed_up),
                ..Default::default()
            }
        }
        fn failing_list() -> Self {
            Self {
                list_fails: true,
                ..Default::default()
            }
        }
        fn fail(mut self, ns: &str, error: &str) -> Self {
            self.failing.insert(ns.into(), error.into());
            self
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl AppRecovery for FakeApps {
        fn backed_up_apps(&self) -> impl Future<Output = Result<Vec<String>>> + Send + '_ {
            async move {
                if self.list_fails {
                    bail!("restic unreachable");
                }
                Ok(self.backed_up.clone())
            }
        }
        fn reinstall<'a>(
            &'a self,
            namespace: &'a str,
        ) -> impl Future<Output = Result<()>> + Send + 'a {
            async move {
                self.reinstalled.lock().unwrap().push(namespace.into());
                match self.failing.get(namespace) {
                    Some(e) => bail!("{e}"),
                    None => Ok(()),
                }
            }
        }
    }

    fn reinstalling() -> HealState {
        HealState {
            gone_since: BTreeMap::from([(1, NOW - 5000)]),
            loss: serde_json::from_value(app_loss()["loss"].clone()).unwrap(),
            recovery: Some(recovery_at(Step::ReinstallApps)),
        }
    }

    #[tokio::test]
    async fn every_app_in_the_backup_is_reinstalled_and_the_recovery_finishes() {
        let apps = FakeApps::with(&["yolab-a", "yolab-c"]).fail("yolab-c", "helm timed out");
        let mut state = reinstalling();
        let host = FakeHost::new().ok("kubectl-apply", "");
        continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .unwrap();

        let r = state.recovery.clone().unwrap();
        assert_eq!(r.finished_at, Some(NOW));
        assert_eq!(r.apps, Some(strings(&["yolab-a", "yolab-c"])));
        assert_eq!(r.outcomes["yolab-a"], AppOutcome::Restored);
        assert_eq!(
            r.outcomes["yolab-c"],
            AppOutcome::Failed {
                error: "helm timed out".into()
            }
        );
        assert_eq!(
            *apps.reinstalled.lock().unwrap(),
            strings(&["yolab-a", "yolab-c"]),
            "yolab-b was not in the backup, so it does not come back"
        );
        assert!(state.loss.is_none(), "the loss is consumed");
        assert!(state.gone_since.is_empty());
        assert_eq!(blocked_reason(&state), None, "backups may run again");
    }

    #[tokio::test]
    async fn the_backup_list_is_saved_before_the_first_reinstall() {
        let apps = FakeApps::with(&["yolab-a"]);
        let mut state = reinstalling();
        let host = FakeHost::new().ok("kubectl-apply", "");
        continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .unwrap();
        let first = applied_states(&host).remove(0).recovery.unwrap();
        assert_eq!(first.apps, Some(strings(&["yolab-a"])));
        assert!(first.outcomes.is_empty());
    }

    #[tokio::test]
    async fn a_resumed_run_keeps_its_saved_list_and_skips_finished_apps() {
        let apps = FakeApps::with(&["yolab-new-in-a-later-backup"]);
        let mut state = reinstalling();
        let r = state.recovery.as_mut().unwrap();
        r.apps = Some(strings(&["yolab-a", "yolab-b"]));
        r.outcomes.insert("yolab-a".into(), AppOutcome::Restored);
        let host = FakeHost::new().ok("kubectl-apply", "");
        continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .unwrap();
        assert_eq!(*apps.reinstalled.lock().unwrap(), strings(&["yolab-b"]));
    }

    #[tokio::test]
    async fn an_unreadable_backup_stops_before_reinstalling_anything() {
        let apps = FakeApps::failing_list();
        let mut state = reinstalling();
        let host = FakeHost::new().ok("kubectl-apply", "");
        assert!(continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .is_err());
        assert!(state.recovery.unwrap().running());
    }

    #[tokio::test]
    async fn a_save_that_fails_stops_before_the_next_app() {
        let apps = FakeApps::with(&["yolab-a", "yolab-b"]);
        let mut state = reinstalling();
        state.recovery.as_mut().unwrap().apps = Some(strings(&["yolab-a", "yolab-b"]));
        let host = FakeHost::new().fail("kubectl-apply", "etcd timeout");
        assert!(continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .is_err());
        assert_eq!(*apps.reinstalled.lock().unwrap(), strings(&["yolab-a"]));
        assert!(state.recovery.unwrap().running());
    }

    #[tokio::test]
    async fn a_whole_recovery_runs_in_order_across_ticks() {
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph osd ls", "[0, 1]")
            .ok("ceph osd ls", "[0]")
            .ok("ceph osd purge", "")
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": [{"metadata": {"name": "yolab-a"}}]}"#,
            )
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": []}"#,
            )
            .ok("kubectl get pvc -n yolab-a", r#"{"items": []}"#)
            .ok("kubectl scale", "")
            .ok("kubectl delete", "")
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs ls", "[]")
            .ok("ceph fs fail", "")
            .ok("ceph fs rm", "")
            .ok(
                "ceph osd pool ls",
                "images\nyolab-fs-metadata\nyolab-fs-data0",
            )
            .ok("ceph osd pool ls", "images")
            .ok("ceph config set mon", "")
            .ok("ceph osd pool delete", "")
            .ok("ceph osd pool create", "")
            .ok("ceph fs new", "")
            .ok("ceph fs subvolumegroup create", "")
            .ok("kubectl-apply", "");
        let apps = FakeApps::with(&["yolab-a"]);
        let mut state = reinstalling();
        state.recovery.as_mut().unwrap().step = Step::PurgeOsds;

        continue_recovery(&host, &apps, &mut state, NOW)
            .await
            .unwrap();
        assert_eq!(
            state.recovery.as_ref().unwrap().step,
            Step::RemoveApps,
            "the first tick waits for the namespace to go"
        );
        continue_recovery(&host, &apps, &mut state, NOW + 30)
            .await
            .unwrap();
        let r = state.recovery.clone().unwrap();
        assert_eq!(r.finished_at, Some(NOW + 30), "{r:?}");
        assert_eq!(r.outcomes["yolab-a"], AppOutcome::Restored);

        let calls = host.calls();
        let pos = |needle: &str| {
            calls
                .iter()
                .position(|c| c.contains(needle))
                .unwrap_or_else(|| panic!("never ran {needle}: {calls:#?}"))
        };
        let order = [
            "ceph osd purge osd.1",
            "kubectl delete namespace yolab-a",
            "ceph fs rm yolab-fs",
            "pool delete yolab-fs-metadata",
            "mon_allow_pool_delete false",
            "pool create yolab-fs-metadata",
            "ceph fs new",
            "subvolumegroup create",
            "app=csi-cephfsplugin",
        ];
        for w in order.windows(2) {
            assert!(pos(w[0]) < pos(w[1]), "{} must come before {}", w[0], w[1]);
        }
        assert_eq!(*apps.reinstalled.lock().unwrap(), strings(&["yolab-a"]));
    }

    // ── What the page is told ────────────────────────────────────────────────

    #[test]
    fn status_reports_the_loss_the_step_and_each_apps_outcome() {
        let mut r = recovery_at(Step::ReinstallApps);
        r.apps = Some(strings(&["yolab-a", "yolab-c"]));
        r.outcomes.insert("yolab-a".into(), AppOutcome::Restored);
        let state = HealState {
            loss: Some(Loss {
                osds: set(&[1, 3]),
                pgs: pgs_by_pool(&[(FS_META_POOL, &["2.1", "2.2"]), ("images", &["4.1"])]),
                rebuilt: BTreeSet::new(),
                detected_at: 42,
            }),
            recovery: Some(r),
            ..Default::default()
        };
        let v = status_json(&state);
        assert_eq!(v["loss"]["osds"], json!([1, 3]));
        assert_eq!(v["loss"]["pools"], json!(["images", FS_META_POOL]));
        assert_eq!(v["loss"]["placement_groups"], 3);
        assert_eq!(v["loss"]["needs_recovery"], true);
        assert_eq!(v["recovery"]["step"], "reinstall_apps");
        assert_eq!(v["recovery"]["running"], true);
        assert_eq!(
            v["recovery"]["apps"],
            json!([
                {"namespace": "yolab-a", "instance_name": "a", "outcome": {"result": "restored"}},
                {"namespace": "yolab-c", "instance_name": "c", "outcome": null},
            ])
        );
        assert_eq!(v["recovery"]["not_restored"], json!(["b"]));
    }

    #[test]
    fn before_the_backup_is_read_nobody_knows_what_will_not_come_back() {
        let state = HealState {
            recovery: Some(recovery_at(Step::RemoveApps)),
            ..Default::default()
        };
        let v = status_json(&state);
        assert_eq!(v["recovery"]["apps"], json!([]));
        assert_eq!(v["recovery"]["not_restored"], Value::Null);
    }

    #[test]
    fn status_of_a_healthy_cluster_is_all_null() {
        assert_eq!(
            status_json(&HealState::default()),
            json!({"loss": null, "recovery": null})
        );
    }

    #[test]
    fn the_preview_splits_installed_apps_into_restored_and_not() {
        let contents = crate::routers::restore::BackupContents {
            taken_at: Some("2026-09-14T12:10:02Z".into()),
            apps: strings(&["yolab-a", "yolab-gone-now"]),
        };
        assert_eq!(
            preview_json(&strings(&["yolab-a", "yolab-b"]), &contents),
            json!({
                "backup_taken_at": "2026-09-14T12:10:02Z",
                "restored": ["a", "gone-now"],
                "not_restored": ["b"],
            })
        );
        let none = crate::routers::restore::BackupContents {
            taken_at: None,
            apps: vec![],
        };
        assert_eq!(
            preview_json(&strings(&["yolab-a"]), &none),
            json!({"backup_taken_at": null, "restored": [], "not_restored": ["a"]})
        );
    }
}
