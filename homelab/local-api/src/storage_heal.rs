//! Lost data: notice it the moment Ceph shows it, heal what is disposable, and on
//! request reset storage and reinstall every app from backup.
//!
//! THREE PHASES, AND ONLY THE LAST ONE DESTROYS ANYTHING.
//!
//! 1. Notice. A placement group is lost when every disk holding it is down — the
//!    same signal the home page's "X of Y groups unavailable" banner reads. It is
//!    recorded straight away, because this is the only moment the evidence exists:
//!    `nix/tests/disk-loss.nix` showed that once an OSD is declared lost, Ceph brings
//!    such a placement group back as `active+clean` and EMPTY. A deploy or a reboot
//!    records a loss too; it clears itself the moment the disks are back, and nothing
//!    happens to data unless the owner presses the button.
//!
//! 2. Self-heal what a machine can refill. Once a loss has lasted long enough that it
//!    is plainly not a restart, the lost OSDs are marked `out` (reversible — nothing
//!    moves when there is no other copy) and the lost placement groups of `images`
//!    and `.mgr` are rebuilt empty. App data is never touched here.
//!
//!    THIS IS THE ONLY PLACE THAT DOES IT. A separate `yolab-images-recover` systemd
//!    timer used to rebuild the same `images` placement groups on its own per-node
//!    clock, from its own marker file, without marking the lost OSDs out first — two
//!    owners for one destructive action. It is gone.
//!
//! 3. Recover, when the owner presses "Recover health from backup". Not a repair —
//!    a reset. Nothing a half-broken cluster reports is trusted:
//!
//!    purge the lost OSDs → remove every app → delete the CephFS pools →
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
//! ONE WRITER. The state lives in Ceph (`storage::settings`), which has no
//! compare-and-swap, so it is written only on the cluster leader: the controller
//! is cluster-scoped, and a recovery requested on any other node is forwarded to
//! the leader (`post_recover`). Within that one process every read-modify-write
//! goes through `update_state`, serialised by a lock, so the tick and a request
//! arriving at the same moment compose instead of overwriting each other.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::time::Duration;

use anyhow::{bail, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ceph::destructive::{self, DisposablePg, RecoveryMandate, DISPOSABLE_POOLS};
pub(crate) use crate::ceph::model::PgsByPool;
use crate::ceph::model::{self, OsdDump, PgBrief};
use crate::error::Outcome;
use crate::host::{Host, RealHost};
use crate::runtime::{Activity, Controller, Ctx, Requirement, Scope, Tick};
use crate::storage::settings;
use crate::AppState;
const MANAGED_SELECTOR: &str = "yolab.io/managed=true";

const FS_NAME: &str = destructive::RECOVERABLE_FS;
const FS_META_POOL: &str = "yolab-fs-metadata";
const FS_DATA_POOL: &str = "yolab-fs-data0";
const FS_SUBVOLUME_GROUP: &str = "csi";
const FS_POOL_PGS: [(&str, u32); 2] = [(FS_META_POOL, 16), (FS_DATA_POOL, 32)];

const TICK: Duration = Duration::from_secs(15);

/// How long a loss must last before the disposable pools are rebuilt on their own.
/// Nobody chooses that rebuild, so it waits out deploys (~90s) and reboots (a few
/// minutes) — throwing away every node's image cache on a restart would be a worse
/// outage than the one being repaired.
fn disposable_grace() -> Duration {
    Duration::from_secs(
        std::env::var("YOLAB_STORAGE_HEAL_DISPOSABLE_GRACE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900),
    )
}

// ── Persisted state ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
struct HealState {
    #[serde(default)]
    loss: Option<Loss>,
    /// The current or most recent recovery.
    #[serde(default)]
    recovery: Option<Recovery>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Loss {
    /// The down OSDs that held the lost placement groups.
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

    /// The authority this record carries to destroy things. Only a running,
    /// persisted recovery has one.
    fn mandate(&self) -> Option<RecoveryMandate> {
        self.running()
            .then(|| RecoveryMandate::from_persisted_recovery(self.started_at, self.osds.clone()))
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
    const ALL: [Step; 6] = [
        Step::PurgeOsds,
        Step::RemoveApps,
        Step::DeleteStorage,
        Step::RecreateStorage,
        Step::RestartCsi,
        Step::ReinstallApps,
    ];

    fn next(self) -> Option<Step> {
        let i = Self::ALL.iter().position(|s| *s == self)?;
        Self::ALL.get(i + 1).copied()
    }

    /// Share of the whole run, in percent. Reinstalling pulls every app's files
    /// back down and dwarfs everything before it.
    fn weight(self) -> u32 {
        match self {
            Step::PurgeOsds => 5,
            Step::RemoveApps => 10,
            Step::DeleteStorage => 5,
            Step::RecreateStorage => 15,
            Step::RestartCsi => 5,
            Step::ReinstallApps => 60,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
enum AppOutcome {
    Restored,
    Failed { error: String },
}

/// Absent is the normal state of a healthy cluster; unreadable is not, and acting
/// without knowing whether a recovery is half done is how two start.
async fn read_state<H: Host>(host: &H) -> Result<HealState> {
    Ok(settings::get_json(host, settings::STORAGE_HEAL)
        .await?
        .unwrap_or_default())
}

/// Serialises every read-modify-write of the state in this process. Only the
/// leader writes (see the module header), so this is the whole of the
/// concurrency control.
static STATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Reads the state, applies `f`, and writes it back if `f` changed anything.
async fn update_state<H: Host, R>(host: &H, f: impl FnOnce(&mut HealState) -> R) -> Result<R> {
    let _serialised = STATE_LOCK.lock().await;
    let mut state = read_state(host).await?;
    let before = state.clone();
    let result = f(&mut state);
    if state != before {
        settings::set_json(host, settings::STORAGE_HEAL, &state).await?;
    }
    Ok(result)
}

/// Writes only `loss`, on top of whatever `recovery` is stored right now.
async fn write_loss<H: Host>(host: &H, loss: &Option<Loss>) -> Result<()> {
    update_state(host, |s| s.loss = loss.clone()).await
}

/// Writes only `recovery`, on top of whatever `loss` is stored right now.
async fn write_recovery<H: Host>(host: &H, recovery: &Option<Recovery>) -> Result<()> {
    update_state(host, |s| s.recovery = recovery.clone()).await
}

/// Whether a recovery is running. `Err` when the state cannot be read — callers
/// that gate on this must treat that as "maybe", which the runtime does.
pub(crate) async fn recovery_running() -> Result<bool> {
    let s = read_state(&RealHost).await?;
    Ok(s.recovery.as_ref().is_some_and(Recovery::running))
}

/// Why a backup must not run right now, if it must not. Backing up while app data
/// is lost — or while a recovery is putting apps back — would upload broken or
/// empty volumes as the newest copy of those apps. Not being able to tell is a
/// reason too: this used to answer "go ahead" whenever the state was unreadable.
pub(crate) async fn backups_blocked() -> Option<String> {
    match read_state(&RealHost).await {
        Ok(state) => blocked_reason(&state).map(str::to_string),
        Err(e) => Some(format!("cannot read the storage health state ({e:#})")),
    }
}

fn blocked_reason(state: &HealState) -> Option<&'static str> {
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        return Some("storage is being recovered from backup");
    }
    if state.loss.as_ref().is_some_and(Loss::needs_recovery) {
        return Some(
            "a disk holding app data is unavailable — reconnect it or recover from backup first",
        );
    }
    None
}

// ── Reading Ceph ──────────────────────────────────────────────────────────────

fn active_in_pools(pgs: &[PgBrief], pool_ids: &HashSet<i64>) -> (usize, usize) {
    let in_pools: Vec<&PgBrief> = pgs
        .iter()
        .filter(|pg| pg.pool().is_some_and(|p| pool_ids.contains(&p)))
        .collect();
    (
        in_pools.iter().filter(|pg| pg.is_active()).count(),
        in_pools.len(),
    )
}

/// Folds what this tick found into the record, which only ever grows until the
/// disks come back or a recovery consumes it.
fn merge_loss(loss: &mut Option<Loss>, found: PgsByPool, holders: BTreeSet<i64>, now: u64) {
    if found.is_empty() {
        return;
    }
    let l = loss.get_or_insert_with(|| Loss {
        osds: BTreeSet::new(),
        pgs: PgsByPool::new(),
        rebuilt: BTreeSet::new(),
        detected_at: now,
    });
    l.osds.extend(holders);
    for (pool, ids) in found {
        l.pgs.entry(pool).or_default().extend(ids);
    }
}

/// Every recorded OSD running again means the disks came back: nothing is lost.
fn reconnected(loss: &Loss, dump: &OsdDump) -> bool {
    let up = dump.up();
    loss.osds.iter().all(|id| up.contains(id))
}

// ── The controller ────────────────────────────────────────────────────────────

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

pub struct StorageHealController;

impl Controller for StorageHealController {
    fn name(&self) -> &'static str {
        "storage-heal"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        TICK
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        // Not StorageRecovery: this controller IS the storage recovery.
        &[Activity::Restore]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(60)
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let in_charge = || ctx.still_in_charge();
        tick(
            &RealHost,
            &RealApps,
            disposable_grace(),
            now_secs(),
            &in_charge,
        )
        .await?;
        Ok(Tick::Done)
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
    grace: Duration,
    now: u64,
    in_charge: InCharge<'_>,
) -> Result<()> {
    if !host.reachable().await {
        return Ok(());
    }
    let state = read_state(host).await?;
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        let mut state = state;
        return continue_recovery(host, apps, &mut state, now, in_charge).await;
    }

    let dump = host.osd_dump().await?;
    let pgs = host.pgs_brief().await?;

    let mut loss = state.loss.clone();
    if loss.as_ref().is_some_and(|l| reconnected(l, &dump)) {
        tracing::info!("storage-heal: every disk that held lost data is back");
        loss = None;
    }
    let (found, holders) = model::lost_pgs(&dump, &pgs);
    merge_loss(&mut loss, found, holders, now);
    if loss != state.loss {
        // Persisted BEFORE anything below changes where Ceph maps those groups.
        write_loss(host, &loss).await?;
    }

    let Some(current) = loss.clone() else {
        return Ok(());
    };
    let disposable: Vec<DisposablePg> = DISPOSABLE_POOLS
        .iter()
        .flat_map(|pool| {
            current
                .pgs
                .get(*pool)
                .into_iter()
                .flatten()
                .map(move |pg| (*pool, pg))
        })
        .filter(|(_, pg)| !current.rebuilt.contains(*pg))
        .filter_map(|(pool, pg)| DisposablePg::new(&dump, pool, pg))
        .collect();
    if disposable.is_empty() || now.saturating_sub(current.detected_at) < grace.as_secs() {
        return Ok(());
    }

    let still_in = dump.is_in();
    let mut all_out = true;
    for id in current.osds.iter().filter(|id| still_in.contains(id)) {
        all_out = false;
        tracing::warn!(
            "storage-heal: osd.{id} has been down since the loss began — marking it out so the \
             image store and mgr pool can be rebuilt on the disks that remain"
        );
        host.ceph(&["osd", "out", &format!("osd.{id}")])
            .await
            .warn_on_err(format!("storage-heal: mark osd.{id} out"));
    }
    // Next tick: the rebuilt groups must map to a disk that can hold them rather
    // than to the missing one, which only holds once the OSDs are seen out.
    if !all_out {
        return Ok(());
    }
    let mut rebuilt = current.rebuilt.clone();
    for pg in &disposable {
        match destructive::force_create_pg(host, pg).await {
            Ok(()) => {
                tracing::info!("storage-heal: rebuilt {} ({}) empty", pg.pgid(), pg.pool());
                rebuilt.insert(pg.pgid().to_string());
            }
            Err(e) => tracing::warn!("storage-heal: could not rebuild {}: {e}", pg.pgid()),
        }
    }
    if rebuilt != current.rebuilt {
        let mut updated = current;
        updated.rebuilt = rebuilt;
        write_loss(host, &Some(updated)).await?;
    }
    Ok(())
}

// ── Starting a recovery ───────────────────────────────────────────────────────

async fn managed_namespaces<H: Host>(host: &H) -> Result<Vec<String>> {
    let v = host
        .kubectl_json(&["get", "namespaces", "-l", MANAGED_SELECTOR, "-o", "json"])
        .await?;
    let items = v["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("kubectl get namespaces: no items list"))?;
    Ok(items
        .iter()
        .filter_map(|n| n["metadata"]["name"].as_str().map(str::to_string))
        .collect())
}

/// Why a recovery cannot start from this state, if it cannot.
fn recovery_refusal(state: &HealState, up: &BTreeSet<i64>) -> Option<String> {
    if state.recovery.as_ref().is_some_and(Recovery::running) {
        return Some("a recovery is already running".into());
    }
    let Some(loss) = state.loss.as_ref() else {
        return Some("no data is unavailable — there is nothing to recover".into());
    };
    if !loss.needs_recovery() {
        return Some(
            "only data that rebuilds itself is unavailable — there is nothing to restore".into(),
        );
    }
    let back: Vec<i64> = loss
        .osds
        .iter()
        .copied()
        .filter(|id| up.contains(id))
        .collect();
    if !back.is_empty() {
        return Some(format!(
            "a disk that held the data is running again ({back:?}) — wait for it to catch up"
        ));
    }
    None
}

async fn start_recovery<H: Host>(host: &H, now: u64) -> Result<()> {
    let state = read_state(host).await?;
    let dump = host.osd_dump().await?;
    if let Some(why) = recovery_refusal(&state, &dump.up()) {
        bail!("{why}");
    }
    let removed = managed_namespaces(host).await?;
    let up = dump.up();
    // Decided again under the state lock, against the state as it is at the
    // moment of writing: two requests cannot both start one.
    let started = update_state(host, |s| {
        if let Some(why) = recovery_refusal(s, &up) {
            return Err(why);
        }
        let osds = s.loss.as_ref().map(|l| l.osds.clone()).unwrap_or_default();
        s.recovery = Some(Recovery {
            step: Step::PurgeOsds,
            started_at: now,
            finished_at: None,
            osds,
            removed: removed.clone(),
            apps: None,
            outcomes: BTreeMap::new(),
        });
        Ok(())
    })
    .await?;
    started.map_err(|why| anyhow::anyhow!(why))?;
    tracing::warn!(
        "storage-heal: recovery from backup started — resetting storage, {} app(s) to remove",
        removed.len()
    );
    crate::runtime::wake("storage-heal");
    Ok(())
}

// ── Running a recovery ────────────────────────────────────────────────────────

/// Whether this node may still act — `Ctx::still_in_charge` in production.
type InCharge<'a> = &'a (dyn Fn() -> bool + Sync);

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
    in_charge: InCharge<'_>,
) -> Result<()> {
    loop {
        // Asked before every step, not once per tick: a step can take many
        // minutes, and a node that lost the lease meanwhile must not go on to the
        // next destructive step while the new leader starts the same one.
        if !in_charge() {
            tracing::warn!("storage-heal: no longer the leader — leaving the recovery to it");
            return Ok(());
        }
        let Some(recovery) = state.recovery.clone().filter(Recovery::running) else {
            return Ok(());
        };
        tracing::info!("storage-heal: recovery step {:?}", recovery.step);
        let result = if recovery.step == Step::ReinstallApps {
            if reinstall_apps(host, apps, state, in_charge).await? {
                StepResult::Done
            } else {
                StepResult::NotYet
            }
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
                return write_recovery(host, &state.recovery).await;
            }
            StepResult::Done => {
                if let Some(r) = state.recovery.as_mut() {
                    match r.step.next() {
                        Some(next) => r.step = next,
                        None => {
                            r.finished_at = Some(now);
                            tracing::info!("storage-heal: recovery finished");
                        }
                    }
                }
                if state.recovery.as_ref().is_some_and(|r| !r.running()) {
                    // The loss this recovery answered is consumed with it.
                    let finished = state.recovery.clone();
                    update_state(host, |s| {
                        s.recovery = finished;
                        s.loss = None;
                    })
                    .await?;
                    state.loss = None;
                } else {
                    write_recovery(host, &state.recovery).await?;
                }
            }
        }
    }
}

async fn run_step<H: Host>(host: &H, r: &Recovery) -> Result<StepResult> {
    use StepResult::*;
    let Some(mandate) = r.mandate() else {
        bail!("recovery is not running — no step may run");
    };
    match r.step {
        Step::PurgeOsds => {
            let dump = host.osd_dump().await?;
            let up = dump.up();
            if r.osds.iter().any(|id| up.contains(id)) {
                return Ok(Cancel);
            }
            let existing = host.osd_ids().await?;
            for id in r.osds.iter().filter(|id| existing.contains(id)) {
                destructive::purge_lost(host, &mandate, *id)
                    .await
                    .warn_on_err(format!("storage-heal: purge osd.{id}"));
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
            let exists = fs_exists(host).await?;
            let existing: Vec<String> = pools(host).await?.into_iter().collect();
            destructive::delete_app_filesystem(host, &mandate, exists, &existing).await?;
            Ok(Done)
        }
        Step::RecreateStorage => {
            let existing = pools(host).await?;
            for (pool, pgs) in FS_POOL_PGS {
                if !existing.contains(pool) {
                    let n = pgs.to_string();
                    host.ceph(&["osd", "pool", "create", pool, &n, &n]).await?;
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
            crate::csi::restart_plugins(host, crate::csi::Which::AllNodes).await?;
            Ok(Done)
        }
        Step::ReinstallApps => unreachable!("handled by reinstall_apps"),
    }
}

/// Tears every app down without waiting on anything the broken filesystem holds:
/// pods are forced off, and the finalizers that would wait for the CSI driver to
/// delete a volume from a filesystem that cannot answer are removed.
///
/// Every command here is best effort by design — each namespace is retried on
/// the next tick until none is left — but a failure is logged, never discarded.
async fn remove_apps<H: Host>(host: &H) -> Result<StepResult> {
    const NO_FINALIZERS: &str = r#"{"metadata":{"finalizers":null}}"#;
    let live = managed_namespaces(host).await?;
    for ns in &live {
        let ns = ns.as_str();
        host.kubectl(&[
            "scale",
            "deployment,statefulset",
            "--all",
            "-n",
            ns,
            "--replicas=0",
        ])
        .await
        .warn_on_err(format!("storage-heal: scale down {ns}"));
        host.kubectl(&[
            "delete",
            "pod",
            "--all",
            "-n",
            ns,
            "--force",
            "--grace-period=0",
            "--wait=false",
        ])
        .await
        .warn_on_err(format!("storage-heal: delete pods in {ns}"));
        let pvcs = match host
            .kubectl_json(&["get", "pvc", "-n", ns, "-o", "json"])
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("storage-heal: list PVCs in {ns}: {e}");
                continue;
            }
        };
        for p in pvcs["items"].as_array().into_iter().flatten() {
            let Some(name) = p["metadata"]["name"].as_str() else {
                continue;
            };
            host.kubectl(&[
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
            .await
            .debug_on_err(format!("storage-heal: clear finalizers on pvc {ns}/{name}"));
            if let Some(pv) = p["spec"]["volumeName"].as_str() {
                host.kubectl(&["patch", "pv", pv, "--type", "merge", "-p", NO_FINALIZERS])
                    .await
                    .debug_on_err(format!("storage-heal: clear finalizers on pv {pv}"));
                host.kubectl(&["delete", "pv", pv, "--wait=false", "--ignore-not-found"])
                    .await
                    .warn_on_err(format!("storage-heal: delete pv {pv}"));
            }
        }
        host.kubectl(&[
            "delete",
            "namespace",
            ns,
            "--wait=false",
            "--ignore-not-found",
        ])
        .await
        .warn_on_err(format!("storage-heal: delete namespace {ns}"));
    }
    Ok(if live.is_empty() {
        StepResult::Done
    } else {
        StepResult::NotYet
    })
}

/// `Ok(true)` once every app in the list has an outcome; `Ok(false)` when it
/// stopped early because this node is no longer the leader.
async fn reinstall_apps<H: Host, A: AppRecovery>(
    host: &H,
    apps: &A,
    state: &mut HealState,
    in_charge: InCharge<'_>,
) -> Result<bool> {
    let list = match state.recovery.as_ref().and_then(|r| r.apps.clone()) {
        Some(list) => list,
        None => {
            let list = apps.backed_up_apps().await?;
            if let Some(r) = state.recovery.as_mut() {
                r.apps = Some(list.clone());
            }
            write_recovery(host, &state.recovery).await?;
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
        if !in_charge() {
            return Ok(false);
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
        write_recovery(host, &state.recovery).await?;
    }
    Ok(true)
}

async fn fs_exists<H: Host>(host: &H) -> Result<bool> {
    let ls: Vec<model::FsEntry> = serde_json::from_value(host.ceph_json(&["fs", "ls"]).await?)
        .map_err(|e| anyhow::anyhow!("ceph fs ls: {e}"))?;
    Ok(ls.iter().any(|f| f.name == FS_NAME))
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

// ── Progress ──────────────────────────────────────────────────────────────────

/// How far the current step has got, as far as it can be counted.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct StepProgress {
    done: u32,
    total: u32,
    /// What is happening right now, in words, when there is something to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn count(done: usize, total: usize) -> StepProgress {
    StepProgress {
        done: done.min(total) as u32,
        total: total as u32,
        detail: None,
    }
}

/// Percent of the whole recovery, from the step it is on and that step's progress.
fn overall_percent(r: &Recovery, step: Option<&StepProgress>) -> u32 {
    if !r.running() {
        return 100;
    }
    let before: u32 = Step::ALL
        .iter()
        .take_while(|s| **s != r.step)
        .map(|s| s.weight())
        .sum();
    let within = step
        .filter(|p| p.total > 0)
        .map(|p| r.step.weight() * p.done / p.total)
        .unwrap_or(0);
    (before + within).min(99)
}

/// What one app being reinstalled is doing, from what exists in its namespace.
fn app_phase(namespace_exists: bool, destinations: &Value, deployments: &Value) -> &'static str {
    if !namespace_exists {
        return "Installing the app";
    }
    let restoring = destinations["items"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["status"]["latestMoverStatus"]["result"].as_str() != Some("Successful"));
    if restoring {
        return "Restoring its files";
    }
    let starting = deployments["items"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| {
            d["status"]["readyReplicas"].as_u64().unwrap_or(0)
                < d["spec"]["replicas"].as_u64().unwrap_or(1)
        });
    if starting {
        "Starting it up"
    } else {
        "Installing the app"
    }
}

fn fs_pgs_active(dump: &OsdDump, pgs: &[PgBrief]) -> (usize, usize) {
    let ids: HashSet<i64> = dump
        .pool_names()
        .into_iter()
        .filter(|(_, n)| n == FS_META_POOL || n == FS_DATA_POOL)
        .map(|(id, _)| id)
        .collect();
    active_in_pools(pgs, &ids)
}

/// Counts the current step against the cluster as it is now. None when it cannot be
/// read or has nothing to count — the page then shows the step without a bar.
async fn step_progress<H: Host>(host: &H, r: &Recovery) -> Option<StepProgress> {
    match r.step {
        Step::PurgeOsds => {
            let ids = host.osd_ids().await.ok()?;
            Some(count(
                r.osds.iter().filter(|id| !ids.contains(id)).count(),
                r.osds.len(),
            ))
        }
        Step::RemoveApps => {
            let live = managed_namespaces(host).await.ok()?;
            Some(count(
                r.removed.iter().filter(|ns| !live.contains(ns)).count(),
                r.removed.len(),
            ))
        }
        Step::DeleteStorage => {
            let pools = pools(host).await.ok()?;
            let fs = fs_exists(host).await.ok()?;
            let gone = usize::from(!fs)
                + [FS_META_POOL, FS_DATA_POOL]
                    .iter()
                    .filter(|p| !pools.contains(**p))
                    .count();
            Some(count(gone, 3))
        }
        Step::RecreateStorage => {
            let dump = host.osd_dump().await.ok()?;
            let pgs = host.pgs_brief().await.ok()?;
            let (active, present) = fs_pgs_active(&dump, &pgs);
            let expected: usize = FS_POOL_PGS.iter().map(|(_, n)| *n as usize).sum();
            Some(StepProgress {
                detail: Some(format!(
                    "{active} of {} storage groups ready",
                    present.max(expected)
                )),
                ..count(active, present.max(expected))
            })
        }
        Step::RestartCsi => None,
        Step::ReinstallApps => {
            let apps = r.apps.as_ref()?;
            let mut p = count(r.outcomes.len(), apps.len());
            if let Some(ns) = apps.iter().find(|ns| !r.outcomes.contains_key(*ns)) {
                let exists = host.kubectl(&["get", "namespace", ns]).await.is_ok();
                let rds = host
                    .kubectl_json(&["get", "replicationdestination", "-n", ns, "-o", "json"])
                    .await
                    .unwrap_or(Value::Null);
                let deploys = host
                    .kubectl_json(&["get", "deployment", "-n", ns, "-o", "json"])
                    .await
                    .unwrap_or(Value::Null);
                p.detail = Some(format!(
                    "{}: {}",
                    instance_name(ns),
                    app_phase(exists, &rds, &deploys)
                ));
            }
            Some(p)
        }
    }
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

fn instance_name(namespace: &str) -> &str {
    namespace.strip_prefix("yolab-").unwrap_or(namespace)
}

/// What the page renders: the loss, and the current or last recovery.
fn status_json(state: &HealState, progress: Option<&StepProgress>) -> Value {
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
        let steps: Vec<Value> = Step::ALL
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
            .collect();
        json!({
            "step": serde_json::to_value(r.step).unwrap_or(Value::Null),
            "steps": steps,
            "running": r.running(),
            "started_at": r.started_at,
            "finished_at": r.finished_at,
            "percent": overall_percent(r, progress),
            "step_progress": progress.filter(|_| r.running()),
            "apps": apps,
            "not_restored": not_restored,
        })
    });
    json!({ "loss": loss, "recovery": recovery })
}

/// What pressing the button would do: which apps come back, and which do not.
fn preview_json(
    installed: &[String],
    down_osds: &BTreeSet<i64>,
    contents: &crate::routers::restore::BackupContents,
) -> Value {
    json!({
        "backup_taken_at": contents.taken_at,
        "down_osds": down_osds,
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
    let host = RealHost;
    let state = match read_state(&host).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": e.to_string() })),
            )
        }
    };
    let progress = match state.recovery.as_ref().filter(|r| r.running()) {
        Some(r) => step_progress(&host, r).await,
        None => None,
    };
    (StatusCode::OK, Json(status_json(&state, progress.as_ref())))
}

/// Set on a request one node forwards to the leader. A node receiving it that
/// does not lead refuses rather than forwarding again, so a lease changing hands
/// mid-request can cost a retry but never a loop.
const FORWARDED_FROM: &str = "x-yolab-forwarded-from";

/// `POST /api/storage/recovery`
///
/// Runs on the leader, the heal state's only writer (see the module header). On
/// any other node the request is forwarded to the leader and its answer relayed.
pub async fn post_recover(
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, Json<Value>) {
    match recovery_route(
        crate::runtime::leader::this_process_leads(),
        headers.contains_key(FORWARDED_FROM),
    ) {
        Route::Here => match start_recovery(&RealHost, now_secs()).await {
            Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
            Err(e) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": e.to_string() })),
            ),
        },
        Route::Refuse => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "this machine stopped leading the cluster while the request was on its way — try again" }),
            ),
        ),
        Route::ToLeader => forward_to_leader(&s.config).await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Here,
    ToLeader,
    Refuse,
}

fn recovery_route(leads: bool, already_forwarded: bool) -> Route {
    match (leads, already_forwarded) {
        (true, _) => Route::Here,
        (false, false) => Route::ToLeader,
        (false, true) => Route::Refuse,
    }
}

async fn forward_to_leader(cfg: &crate::config::Config) -> (StatusCode, Json<Value>) {
    let unavailable = |why: String| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": why })),
        )
    };
    let holder = match crate::runtime::leader::holder().await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return unavailable("no machine leads the cluster right now — try again shortly".into())
        }
        Err(e) => return unavailable(format!("cannot tell which machine leads the cluster: {e}")),
    };
    let nodes = match crate::kubectl::get_nodes().await {
        Ok(n) => n,
        Err(e) => return unavailable(format!("cannot list the cluster's machines: {e}")),
    };
    let Some(addr) = crate::kubectl::node_ipv6(&nodes, &holder) else {
        return unavailable(format!(
            "{holder} leads the cluster but has no cluster address"
        ));
    };
    let url = format!("http://[{addr}]:{}/api/storage/recovery", cfg.port);
    let sent = reqwest::Client::new()
        .post(&url)
        .header(crate::auth::CLUSTER_AUTH_HEADER, cfg.cluster_token())
        .header(FORWARDED_FROM, crate::system::hostname())
        .timeout(Duration::from_secs(60))
        .send()
        .await;
    let response = match sent {
        Ok(r) => r,
        Err(e) => return unavailable(format!("{holder} (the leader) did not answer: {e}")),
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match response.json::<Value>().await {
        Ok(body) => (status, Json(body)),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(
                json!({ "error": format!("{holder} (the leader) sent an unreadable answer: {e}") }),
            ),
        ),
    }
}

/// `GET /api/storage/recovery/preview`
pub async fn get_preview(State(_s): State<AppState>) -> (StatusCode, Json<Value>) {
    let host = RealHost;
    let unavailable = |e: anyhow::Error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": e.to_string() })),
        )
    };
    let installed = match managed_namespaces(&host).await {
        Ok(n) => n,
        Err(e) => return unavailable(e),
    };
    let down = match host.osd_dump().await {
        Ok(d) => d.down(),
        Err(e) => return unavailable(e.into()),
    };
    match crate::routers::restore::backup_contents().await {
        Ok(contents) => (
            StatusCode::OK,
            Json(preview_json(&installed, &down, &contents)),
        ),
        Err(e) => unavailable(anyhow::anyhow!("could not read the backup: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const NOW: u64 = 1_000_000;
    const GRACE: Duration = Duration::from_secs(900);

    fn always() -> bool {
        true
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

    /// Test fixtures stay terse JSON; the code under test takes the typed dump.
    /// Fields real Ceph always sends (`in`, `pools`) are filled in when a
    /// fixture leaves them out.
    fn osd_dump(v: &Value) -> OsdDump {
        let mut v = v.clone();
        if v.get("pools").is_none() {
            v["pools"] = json!([]);
        }
        for o in v["osds"].as_array_mut().into_iter().flatten() {
            if o.get("in").is_none() {
                o["in"] = json!(1);
            }
        }
        serde_json::from_value(v).expect("fixture is a valid osd dump")
    }

    fn pg_briefs(v: &Value) -> Vec<PgBrief> {
        model::parse_pgs_brief("fixture", &v.to_string()).expect("fixture is a valid pgs_brief")
    }

    fn lost_pgs(dump: &Value, pgs: &Value) -> (PgsByPool, BTreeSet<i64>) {
        model::lost_pgs(&osd_dump(dump), &pg_briefs(pgs))
    }

    fn healthy_dump() -> Value {
        let mut d = dump();
        d["osds"][1]["up"] = json!(1);
        d
    }

    fn lost_pg_dump() -> Value {
        json!({"pg_stats": [
            {"pgid": "2.1", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "4.1", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "4.2", "state": "active+clean", "acting": [0]},
        ]})
    }

    // ── Which placement groups are lost ──────────────────────────────────────

    #[test]
    fn a_pg_is_lost_when_every_holder_is_down() {
        let pgs = json!({"pg_stats": [
            {"pgid": "2.1", "state": "stale+active+clean", "acting": [1], "up": [1]},
            {"pgid": "3.4", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "3.5", "state": "active+undersized", "acting": [0, 1]},
            {"pgid": "4.2", "state": "active+clean", "acting": [0]},
            {"pgid": "4.7", "state": "stale+down", "acting": [], "up": [1]},
            {"pgid": "1.0", "state": "stale+active+clean", "acting": [1]},
            {"pgid": "9.9", "state": "stale", "acting": [1]},
            {"pgid": "2.2", "state": "unknown", "acting": [0]},
            {"pgid": "2.3", "state": "unknown"},
        ]});
        let (lost, holders) = lost_pgs(&dump(), &pgs);
        assert_eq!(
            lost,
            pgs_by_pool(&[
                (".mgr", &["1.0"]),
                (FS_META_POOL, &["2.1"]),
                (FS_DATA_POOL, &["3.4"]),
                ("images", &["4.7"]),
            ])
        );
        assert_eq!(holders, set(&[1]));
    }

    #[test]
    fn nothing_is_lost_while_every_disk_is_up() {
        let (lost, holders) = lost_pgs(&healthy_dump(), &lost_pg_dump());
        assert!(lost.is_empty() && holders.is_empty());
    }

    #[test]
    fn a_pg_with_one_copy_still_up_is_not_lost() {
        let mut d = dump();
        d["osds"] = json!([
            {"osd": 0, "up": 1}, {"osd": 1, "up": 0}, {"osd": 2, "up": 0}
        ]);
        let pgs = json!([
            {"pgid": "3.1", "state": "stale", "acting": [1, 2]},
            {"pgid": "3.2", "state": "active+undersized+degraded", "acting": [0, 2]},
        ]);
        let (lost, holders) = lost_pgs(&d, &pgs);
        assert_eq!(lost, pgs_by_pool(&[(FS_DATA_POOL, &["3.1"])]));
        assert_eq!(holders, set(&[1, 2]));
    }

    #[test]
    fn the_record_only_grows_and_keeps_its_first_sighting() {
        let mut loss = None;
        merge_loss(&mut loss, PgsByPool::new(), set(&[1]), NOW);
        assert!(loss.is_none());
        merge_loss(
            &mut loss,
            pgs_by_pool(&[("images", &["4.1"])]),
            set(&[1]),
            NOW,
        );
        merge_loss(
            &mut loss,
            pgs_by_pool(&[("images", &["4.2"]), (FS_META_POOL, &["2.0"])]),
            set(&[2]),
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

    #[test]
    fn the_loss_clears_only_when_every_recorded_disk_is_up() {
        let l = Loss {
            osds: set(&[1, 2]),
            pgs: pgs_by_pool(&[(FS_META_POOL, &["2.1"])]),
            rebuilt: BTreeSet::new(),
            detected_at: NOW,
        };
        let partly = json!({"osds": [{"osd": 1, "up": 1}, {"osd": 2, "up": 0}]});
        assert!(!reconnected(&l, &osd_dump(&partly)));
        let all = json!({"osds": [{"osd": 1, "up": 1}, {"osd": 2, "up": 1}]});
        assert!(reconnected(&l, &osd_dump(&all)));
        let purged = json!({"osds": [{"osd": 1, "up": 1}]});
        assert!(
            !reconnected(&l, &osd_dump(&purged)),
            "a missing OSD is not a returned one"
        );
    }

    // ── State ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_unreadable_state_map_stops_everything() {
        let host = FakeHost::new().ok("ceph -s", "").fail(
            "ceph config-key get yolab/storage-heal",
            "connection refused",
        );
        assert!(tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .is_err());
        assert_eq!(host.calls().len(), 2, "{:?}", host.calls());
    }

    #[tokio::test]
    async fn a_missing_state_map_is_a_healthy_cluster_and_junk_is_unreadable() {
        let host = FakeHost::new().fail(
            "ceph config-key get yolab/storage-heal",
            "Error ENOENT: key 'yolab/storage-heal' doesn't exist",
        );
        assert_eq!(read_state(&host).await.unwrap(), HealState::default());
        let host = FakeHost::new().ok("ceph config-key get yolab/storage-heal", "{nope");
        assert!(read_state(&host).await.is_err());
    }

    #[test]
    fn state_round_trips_through_json() {
        let s = HealState {
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
    fn a_recovery_runs_on_the_leader_and_is_forwarded_at_most_once() {
        assert_eq!(recovery_route(true, false), Route::Here);
        assert_eq!(recovery_route(true, true), Route::Here);
        assert_eq!(recovery_route(false, false), Route::ToLeader);
        assert_eq!(
            recovery_route(false, true),
            Route::Refuse,
            "never forwarded twice"
        );
    }

    #[test]
    fn backups_are_blocked_while_app_data_is_unavailable_or_being_recovered() {
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

    fn cluster_host(state: &Value, dump: &Value, pgs: &Value) -> FakeHost {
        FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph config-key get yolab/storage-heal", &state.to_string())
            .ok("ceph osd dump", &dump.to_string())
            .ok("ceph pg dump pgs_brief", &pgs.to_string())
            .ok("ceph config-key set yolab/storage-heal", "")
    }

    const NO_STATE_MAP: &str = "Error ENOENT: key 'yolab/storage-heal' doesn't exist";

    /// A host with no state map yet, where every state write succeeds.
    fn fresh_state_host() -> FakeHost {
        FakeHost::new()
            .fail("ceph config-key get yolab/storage-heal", NO_STATE_MAP)
            .ok("ceph config-key set yolab/storage-heal", "")
    }

    fn wrote(host: &FakeHost) -> bool {
        host.ran("ceph config-key set yolab/storage-heal")
    }

    fn applied_states(host: &FakeHost) -> Vec<HealState> {
        host.calls()
            .iter()
            .filter_map(|c| c.strip_prefix("ceph config-key set yolab/storage-heal "))
            .filter_map(|raw| serde_json::from_str(raw).ok())
            .collect()
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_not_even_asked_about_its_state() {
        let host = FakeHost::new().fail("ceph -s", "timed out");
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert_eq!(host.calls(), vec!["ceph -s".to_string()]);
    }

    #[tokio::test]
    async fn a_healthy_cluster_writes_nothing() {
        let host = cluster_host(&json!({}), &healthy_dump(), &lost_pg_dump());
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert!(!wrote(&host));
    }

    #[tokio::test]
    async fn a_loss_is_recorded_on_the_very_first_tick_and_nothing_else_happens() {
        let host = cluster_host(&json!({}), &dump(), &lost_pg_dump());
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        let loss = applied_states(&host)[0]
            .loss
            .clone()
            .expect("recorded at once");
        assert_eq!(loss.osds, set(&[1]));
        assert_eq!(loss.detected_at, NOW);
        assert_eq!(
            loss.pgs,
            pgs_by_pool(&[(FS_META_POOL, &["2.1"]), ("images", &["4.1"])])
        );
        assert!(
            !host.ran("ceph osd out"),
            "nothing moves inside the grace period"
        );
        assert!(!host.ran("force-create-pg") && !host.ran("osd purge"));
    }

    #[tokio::test]
    async fn past_the_grace_the_lost_disks_go_out_first_and_nothing_is_rebuilt_yet() {
        let state = json!({"loss": {"osds": [1], "pgs": {"images": ["4.1"], FS_META_POOL: ["2.1"]},
                                    "detected_at": NOW - 900}});
        let host = cluster_host(&state, &dump(), &lost_pg_dump()).ok("ceph osd out", "");
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert!(host.ran("ceph osd out osd.1"));
        assert!(
            !host.ran("force-create-pg"),
            "not until the OSD is seen out"
        );
    }

    #[tokio::test]
    async fn once_out_the_disposable_pools_rebuild_and_app_data_is_left_alone() {
        let state = json!({"loss": {"osds": [1],
            "pgs": {FS_META_POOL: ["2.1"], "images": ["4.1"], ".mgr": ["1.0"]},
            "detected_at": NOW - 2000}});
        let mut d = dump();
        d["osds"][1]["in"] = json!(0);
        let pgs = json!({"pg_stats": [{"pgid": "2.1", "state": "down", "acting": [0]}]});
        let host = cluster_host(&state, &d, &pgs).ok("ceph osd force-create-pg", "");
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();

        assert!(host.ran("force-create-pg 4.1") && host.ran("force-create-pg 1.0"));
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
    async fn a_loss_of_only_app_data_never_marks_anything_out() {
        let state = json!({"loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"]},
                                    "detected_at": NOW - 100_000}});
        let only_app_data =
            json!({"pg_stats": [{"pgid": "2.1", "state": "stale+active+clean", "acting": [1]}]});
        let host = cluster_host(&state, &dump(), &only_app_data);
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert!(
            !host.ran("ceph osd out"),
            "reconnecting must stay a way back"
        );
    }

    #[tokio::test]
    async fn an_already_rebuilt_pg_is_not_rebuilt_again() {
        let state = json!({"loss": {"osds": [1], "pgs": {"images": ["4.1"]}, "rebuilt": ["4.1"],
                                    "detected_at": 1}});
        let mut d = dump();
        d["osds"][1]["in"] = json!(0);
        let host = cluster_host(&state, &d, &json!({"pg_stats": []}));
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert!(!host.ran("force-create-pg") && !host.ran("osd out"));
    }

    #[tokio::test]
    async fn the_disk_coming_back_clears_the_record() {
        let state =
            json!({"loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"]}, "detected_at": 1}});
        let host = cluster_host(&state, &healthy_dump(), &lost_pg_dump());
        tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .unwrap();
        assert!(applied_states(&host).pop().unwrap().loss.is_none());
    }

    #[tokio::test]
    async fn an_unreadable_pg_dump_is_an_error_and_records_nothing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph config-key get yolab/storage-heal", "{}")
            .ok("ceph osd dump", &dump().to_string())
            .fail("ceph pg dump pgs_brief", "timeout");
        assert!(tick(&host, &FakeApps::default(), GRACE, NOW, &always)
            .await
            .is_err());
        assert!(!wrote(&host));
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
                "ceph config-key get yolab/storage-heal",
                &serde_json::to_string(&state).unwrap(),
            )
            .ok("kubectl delete pod", "")
            .ok("ceph config-key set yolab/storage-heal", "");
        let err = tick(&host, &FakeApps::failing_list(), GRACE, NOW, &always)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("restic unreachable"), "{err}");
        assert!(!host.ran("pg dump"), "no detection while recovering");
        assert!(host.ran("app=csi-cephfsplugin"));
    }

    // ── Starting a recovery ──────────────────────────────────────────────────

    fn start_host(state: &Value, dump: &Value) -> FakeHost {
        FakeHost::new()
            .ok(
                "ceph config-key get yolab/storage-heal",
                &state.to_string(),
            )
            .ok("ceph osd dump", &dump.to_string())
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                &json!({"items": [{"metadata": {"name": "yolab-a"}}, {"metadata": {"name": "yolab-b"}}]})
                    .to_string(),
            )
            .ok("ceph config-key set yolab/storage-heal", "")
    }

    fn app_loss() -> Value {
        json!({"loss": {"osds": [1], "pgs": {FS_META_POOL: ["2.1"]}, "detected_at": 1}})
    }

    #[tokio::test]
    async fn the_button_works_immediately_after_a_loss_is_seen() {
        let host = start_host(&app_loss(), &dump());
        start_recovery(&host, 2).await.unwrap();
        let r = applied_states(&host).pop().unwrap().recovery.unwrap();
        assert_eq!(r.step, Step::PurgeOsds);
        assert_eq!(r.started_at, 2);
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

        let err = start_recovery(&start_host(&app_loss(), &healthy_dump()), NOW)
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

    #[tokio::test]
    async fn a_second_press_that_loses_the_race_starts_nothing_and_writes_nothing() {
        let state_cm = |state: &Value| state.to_string();
        let mut running = app_loss();
        running["recovery"] = serde_json::to_value(recovery_at(Step::PurgeOsds)).unwrap();
        let host = FakeHost::new()
            // What this press read first: a loss, nothing running yet…
            .ok(
                "ceph config-key get yolab/storage-heal",
                &state_cm(&app_loss()),
            )
            // …and what the swap reads: the other press already started one.
            .ok(
                "ceph config-key get yolab/storage-heal",
                &state_cm(&running),
            )
            .ok("ceph osd dump", &dump().to_string())
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": []}"#,
            )
            .ok("ceph config-key set yolab/storage-heal", "");
        let err = start_recovery(&host, NOW).await.unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(!wrote(&host), "{:?}", host.calls());
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
            .ok(
                "ceph osd dump",
                r#"{"osds": [{"osd": 0, "up": 1, "in": 1}], "pools": []}"#,
            )
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
        let host = FakeHost::new()
            .fail("ceph config-key get yolab/storage-heal", NO_STATE_MAP)
            .ok("ceph osd dump", &healthy_dump().to_string())
            .ok("ceph config-key set yolab/storage-heal", "");
        let mut state = HealState {
            loss: serde_json::from_value(app_loss()["loss"].clone()).unwrap(),
            recovery: Some(recovery_at(Step::PurgeOsds)),
        };
        continue_recovery(&host, &FakeApps::default(), &mut state, NOW, &always)
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
    fn the_steps_run_in_order_and_their_weights_make_a_whole() {
        let mut s = Step::PurgeOsds;
        let mut order = vec![s];
        while let Some(n) = s.next() {
            order.push(n);
            s = n;
        }
        assert_eq!(order, Step::ALL.to_vec());
        assert_eq!(Step::ALL.iter().map(|s| s.weight()).sum::<u32>(), 100);
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
            loss: serde_json::from_value(app_loss()["loss"].clone()).unwrap(),
            recovery: Some(recovery_at(Step::ReinstallApps)),
        }
    }

    #[tokio::test]
    async fn every_app_in_the_backup_is_reinstalled_and_the_recovery_finishes() {
        let apps = FakeApps::with(&["yolab-a", "yolab-c"]).fail("yolab-c", "helm timed out");
        let mut state = reinstalling();
        let host = fresh_state_host();
        continue_recovery(&host, &apps, &mut state, NOW, &always)
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
        assert_eq!(blocked_reason(&state), None, "backups may run again");
    }

    #[tokio::test]
    async fn the_backup_list_is_saved_before_the_first_reinstall() {
        let apps = FakeApps::with(&["yolab-a"]);
        let mut state = reinstalling();
        let host = fresh_state_host();
        continue_recovery(&host, &apps, &mut state, NOW, &always)
            .await
            .unwrap();
        let first = applied_states(&host).remove(0).recovery.unwrap();
        assert_eq!(first.apps, Some(strings(&["yolab-a"])));
        assert!(first.outcomes.is_empty());
    }

    #[tokio::test]
    async fn a_node_that_loses_the_lease_mid_recovery_stops_before_the_next_app() {
        let apps = FakeApps::with(&["yolab-a", "yolab-b"]);
        let mut state = reinstalling();
        state.recovery.as_mut().unwrap().apps = Some(strings(&["yolab-a", "yolab-b"]));
        let host = fresh_state_host();
        let asked = std::sync::atomic::AtomicU32::new(0);
        // In charge for the step check and the first app, then the lease is gone.
        let in_charge = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2;
        continue_recovery(&host, &apps, &mut state, NOW, &in_charge)
            .await
            .unwrap();
        assert_eq!(*apps.reinstalled.lock().unwrap(), strings(&["yolab-a"]));
        let r = state.recovery.unwrap();
        assert!(r.running(), "not finished: the new leader carries on");
        assert_eq!(r.step, Step::ReinstallApps);
        assert_eq!(r.outcomes.len(), 1, "yolab-a's outcome was saved");
    }

    #[tokio::test]
    async fn a_node_that_is_not_in_charge_starts_no_step_at_all() {
        let host = FakeHost::new();
        let mut state = HealState {
            loss: None,
            recovery: Some(recovery_at(Step::PurgeOsds)),
        };
        continue_recovery(&host, &FakeApps::default(), &mut state, NOW, &|| false)
            .await
            .unwrap();
        assert!(host.calls().is_empty(), "{:?}", host.calls());
        assert_eq!(state.recovery.unwrap().step, Step::PurgeOsds);
    }

    #[tokio::test]
    async fn a_resumed_run_keeps_its_saved_list_and_skips_finished_apps() {
        let apps = FakeApps::with(&["yolab-new-in-a-later-backup"]);
        let mut state = reinstalling();
        let r = state.recovery.as_mut().unwrap();
        r.apps = Some(strings(&["yolab-a", "yolab-b"]));
        r.outcomes.insert("yolab-a".into(), AppOutcome::Restored);
        let host = fresh_state_host();
        continue_recovery(&host, &apps, &mut state, NOW, &always)
            .await
            .unwrap();
        assert_eq!(*apps.reinstalled.lock().unwrap(), strings(&["yolab-b"]));
    }

    #[tokio::test]
    async fn an_unreadable_backup_stops_before_reinstalling_anything() {
        let mut state = reinstalling();
        let host = fresh_state_host();
        assert!(
            continue_recovery(&host, &FakeApps::failing_list(), &mut state, NOW, &always)
                .await
                .is_err()
        );
        assert!(state.recovery.unwrap().running());
    }

    #[tokio::test]
    async fn a_save_that_fails_stops_before_the_next_app() {
        let apps = FakeApps::with(&["yolab-a", "yolab-b"]);
        let mut state = reinstalling();
        state.recovery.as_mut().unwrap().apps = Some(strings(&["yolab-a", "yolab-b"]));
        let host = FakeHost::new()
            .fail("ceph config-key get yolab/storage-heal", NO_STATE_MAP)
            .fail(
                "ceph config-key set yolab/storage-heal",
                "error connecting to the cluster",
            );
        assert!(continue_recovery(&host, &apps, &mut state, NOW, &always)
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
            .fail("ceph config-key get yolab/storage-heal", NO_STATE_MAP)
            .ok("ceph config-key set yolab/storage-heal", "");
        let apps = FakeApps::with(&["yolab-a"]);
        let mut state = reinstalling();
        state.recovery.as_mut().unwrap().step = Step::PurgeOsds;

        continue_recovery(&host, &apps, &mut state, NOW, &always)
            .await
            .unwrap();
        assert_eq!(state.recovery.as_ref().unwrap().step, Step::RemoveApps);
        continue_recovery(&host, &apps, &mut state, NOW + 30, &always)
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
    }

    // ── Progress ─────────────────────────────────────────────────────────────

    fn progress(done: u32, total: u32) -> StepProgress {
        StepProgress {
            done,
            total,
            detail: None,
        }
    }

    #[test]
    fn the_overall_percent_adds_finished_steps_and_the_share_of_the_current_one() {
        let at = |step| recovery_at(step);
        assert_eq!(overall_percent(&at(Step::PurgeOsds), None), 0);
        assert_eq!(
            overall_percent(&at(Step::PurgeOsds), Some(&progress(1, 1))),
            5
        );
        assert_eq!(
            overall_percent(&at(Step::RemoveApps), Some(&progress(1, 2))),
            10
        );
        assert_eq!(
            overall_percent(&at(Step::RecreateStorage), Some(&progress(24, 48))),
            27
        );
        assert_eq!(
            overall_percent(&at(Step::ReinstallApps), Some(&progress(2, 4))),
            70
        );
        assert_eq!(
            overall_percent(&at(Step::ReinstallApps), Some(&progress(4, 4))),
            99,
            "a running recovery never claims 100"
        );
        assert_eq!(
            overall_percent(&at(Step::RemoveApps), Some(&progress(3, 0))),
            5
        );
        let mut done = at(Step::ReinstallApps);
        done.finished_at = Some(NOW);
        assert_eq!(overall_percent(&done, None), 100);
    }

    #[test]
    fn an_app_being_reinstalled_says_what_it_is_doing() {
        let none = json!({"items": []});
        assert_eq!(app_phase(false, &none, &none), "Installing the app");
        let restoring = json!({"items": [{"status": {"latestMoverStatus": {"result": "Failed"}}}, {"status": {}}]});
        assert_eq!(app_phase(true, &restoring, &none), "Restoring its files");
        let restored =
            json!({"items": [{"status": {"latestMoverStatus": {"result": "Successful"}}}]});
        let starting =
            json!({"items": [{"spec": {"replicas": 2}, "status": {"readyReplicas": 1}}]});
        assert_eq!(app_phase(true, &restored, &starting), "Starting it up");
        let ready = json!({"items": [{"spec": {"replicas": 1}, "status": {"readyReplicas": 1}}]});
        assert_eq!(app_phase(true, &restored, &ready), "Installing the app");
    }

    #[test]
    fn filesystem_pgs_are_counted_only_in_the_filesystem_pools() {
        let pgs = json!({"pg_stats": [
            {"pgid": "2.0", "state": "active+clean"},
            {"pgid": "2.1", "state": "creating"},
            {"pgid": "3.0", "state": "active+clean"},
            {"pgid": "4.0", "state": "active+clean"},
        ]});
        assert_eq!(fs_pgs_active(&osd_dump(&dump()), &pg_briefs(&pgs)), (2, 3));
    }

    #[tokio::test]
    async fn step_progress_counts_each_step_against_the_cluster() {
        let host = FakeHost::new().ok("ceph osd ls", "[0]");
        let mut r = recovery_at(Step::PurgeOsds);
        r.osds = set(&[1, 2]);
        assert_eq!(step_progress(&host, &r).await, Some(progress(2, 2)));

        let host = FakeHost::new().ok(
            "kubectl get namespaces -l yolab.io/managed=true",
            r#"{"items": [{"metadata": {"name": "yolab-b"}}]}"#,
        );
        assert_eq!(
            step_progress(&host, &recovery_at(Step::RemoveApps)).await,
            Some(progress(1, 2))
        );

        let host = FakeHost::new()
            .ok("ceph osd pool ls", "yolab-fs-data0")
            .ok("ceph fs ls", "[]");
        assert_eq!(
            step_progress(&host, &recovery_at(Step::DeleteStorage)).await,
            Some(progress(2, 3))
        );

        let pgs = json!({"pg_stats": [{"pgid": "2.0", "state": "active+clean"}]});
        let host = FakeHost::new()
            .ok("ceph osd dump", &dump().to_string())
            .ok("ceph pg dump pgs_brief", &pgs.to_string());
        assert_eq!(
            step_progress(&host, &recovery_at(Step::RecreateStorage)).await,
            Some(StepProgress {
                done: 1,
                total: 48,
                detail: Some("1 of 48 storage groups ready".into())
            }),
            "counts against the groups the new pools will have, not the few created so far"
        );

        assert_eq!(
            step_progress(&FakeHost::new(), &recovery_at(Step::RestartCsi)).await,
            None
        );
    }

    #[tokio::test]
    async fn reinstall_progress_names_the_app_in_progress_and_what_it_is_doing() {
        let mut r = recovery_at(Step::ReinstallApps);
        assert_eq!(
            step_progress(&FakeHost::new(), &r).await,
            None,
            "nothing to count before the backup is read"
        );
        r.apps = Some(strings(&["yolab-a", "yolab-b", "yolab-c"]));
        r.outcomes.insert("yolab-a".into(), AppOutcome::Restored);
        let host = FakeHost::new()
            .ok("kubectl get namespace yolab-b", "")
            .ok(
                "kubectl get replicationdestination -n yolab-b",
                r#"{"items": [{"status": {}}]}"#,
            )
            .ok("kubectl get deployment -n yolab-b", r#"{"items": []}"#);
        assert_eq!(
            step_progress(&host, &r).await,
            Some(StepProgress {
                done: 1,
                total: 3,
                detail: Some("b: Restoring its files".into())
            })
        );
    }

    #[tokio::test]
    async fn an_unreadable_cluster_gives_no_bar_rather_than_a_wrong_one() {
        let host = FakeHost::new().fail("ceph osd ls", "timeout");
        assert_eq!(
            step_progress(&host, &recovery_at(Step::PurgeOsds)).await,
            None
        );
    }

    // ── What the page is told ────────────────────────────────────────────────

    #[test]
    fn status_reports_the_loss_the_steps_the_progress_and_each_app() {
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
        };
        let p = StepProgress {
            done: 1,
            total: 2,
            detail: Some("c: Restoring its files".into()),
        };
        let v = status_json(&state, Some(&p));
        assert_eq!(v["loss"]["osds"], json!([1, 3]));
        assert_eq!(v["loss"]["pools"], json!(["images", FS_META_POOL]));
        assert_eq!(v["loss"]["placement_groups"], 3);
        assert_eq!(v["loss"]["needs_recovery"], true);
        assert_eq!(v["recovery"]["step"], "reinstall_apps");
        assert_eq!(
            v["recovery"]["steps"],
            json!([
                "purge_osds",
                "remove_apps",
                "delete_storage",
                "recreate_storage",
                "restart_csi",
                "reinstall_apps"
            ])
        );
        assert_eq!(v["recovery"]["running"], true);
        assert_eq!(v["recovery"]["percent"], 70);
        assert_eq!(
            v["recovery"]["step_progress"],
            json!({"done": 1, "total": 2, "detail": "c: Restoring its files"})
        );
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
    fn a_finished_recovery_is_at_100_percent_with_no_step_bar() {
        let mut r = recovery_at(Step::ReinstallApps);
        r.finished_at = Some(NOW);
        let state = HealState {
            recovery: Some(r),
            ..Default::default()
        };
        let v = status_json(&state, Some(&progress(1, 2)));
        assert_eq!(v["recovery"]["percent"], 100);
        assert_eq!(v["recovery"]["step_progress"], Value::Null);
    }

    #[test]
    fn before_the_backup_is_read_nobody_knows_what_will_not_come_back() {
        let state = HealState {
            recovery: Some(recovery_at(Step::RemoveApps)),
            ..Default::default()
        };
        let v = status_json(&state, None);
        assert_eq!(v["recovery"]["apps"], json!([]));
        assert_eq!(v["recovery"]["not_restored"], Value::Null);
        assert_eq!(v["recovery"]["step_progress"], Value::Null);
    }

    #[test]
    fn status_of_a_healthy_cluster_is_all_null() {
        assert_eq!(
            status_json(&HealState::default(), None),
            json!({"loss": null, "recovery": null})
        );
    }

    #[test]
    fn the_preview_names_the_down_disks_and_splits_the_apps() {
        let contents = crate::routers::restore::BackupContents {
            taken_at: Some("2026-09-14T12:10:02Z".into()),
            apps: strings(&["yolab-a", "yolab-gone-now"]),
        };
        assert_eq!(
            preview_json(&strings(&["yolab-a", "yolab-b"]), &set(&[1]), &contents),
            json!({
                "backup_taken_at": "2026-09-14T12:10:02Z",
                "down_osds": [1],
                "restored": ["a", "gone-now"],
                "not_restored": ["b"],
            })
        );
    }
}
