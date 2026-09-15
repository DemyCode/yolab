//! FORCE HEAL: rebuild the cluster from the machines that still answer.
//!
//! NOTHING HEALS ITSELF. An offline machine looks exactly like a departed one,
//! and a disk being moved looks like a dead one, so only a person decides. The
//! page shows what is wrong — machines that do not answer, Ceph without a
//! quorum, Kubernetes not answering, data with no reachable copy — and FORCE
//! HEAL is offered whenever any of it is true.
//!
//! KEEP THE MACHINES THAT ANSWER, THROW EVERYTHING ELSE AWAY. However the
//! cluster broke, a heal ends in the same place: the machines that still answer,
//! each with only its system disk in use, no pools, no apps — a fresh
//! installation of those machines. Apps come back through "Add from backup" on
//! the home page; other disks show up OFF on the Storage page, to be switched
//! on again.
//!
//!   1. `claim`: make sure no other machine drives a heal.
//!   2. `consensus`: get Ceph and Kubernetes to agree again without the gone
//!      machines — their mons removed (offline, from this machine's monmap, when
//!      there is no quorum to ask), and k3s reset to this machine when it is the
//!      only one left.
//!   3. `wipe`: purge every OSD that is not up, forget the gone machines, delete
//!      every pool and the app filesystem, and every disk switch and disk list.
//!   4. `restart`: restart every machine that answers, this one last. At boot
//!      each makes its system disk an OSD again if it has to, creates a fresh
//!      image store before k3s starts, and registers its other disks OFF; the mgr
//!      recreates its pool and the filesystem controller the app filesystem.
//!   5. `finish`: delete the gone machines from Kubernetes and every app.
//!
//! THE MACHINE YOU CLICK DRIVES IT. A heal must work exactly when the leader
//! election and the Ceph key-value store may not, so it is not a cluster-scoped
//! controller: the record lives in a file on the driving machine
//! (`/var/lib/yolab/heal.json`), survives its restart, and is copied into Ceph
//! (`settings::HEAL`) whenever Ceph answers, which is how other machines pause
//! their own storage controllers and show progress.
//!
//! SPLIT BRAIN. Two machines that are both running but cannot reach each other
//! would each remove the other and become two clusters. So a heal only removes
//! machines that answer on neither their Ceph nor their API port from here, the
//! owner confirms each by name, and a heal refuses to repair a lost Ceph quorum
//! while any other machine still answers — that is not a machine being gone.
//!
//! A REMOVED MACHINE DOES NOT COME BACK AS IT WAS. Its mon, OSDs, daemon keys and
//! Kubernetes node are gone; `settings::REMOVED_MACHINES` stops it re-adding its
//! mon (`storage::mon_member`). It rejoins by being installed again.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ceph::destructive::{self, HealMandate, RECOVERABLE_FS, RECOVERABLE_FS_POOLS};
use crate::ceph::model::{self, PgsByPool};
use crate::error::Outcome;
use crate::host::{Host, RealHost};
use crate::runtime::{Controller, Ctx, Scope, Tick};
use crate::storage::settings;
use crate::AppState;

const NAME: &str = "heal";
const TICK: Duration = Duration::from_secs(10);
const MANAGED_SELECTOR: &str = "yolab.io/managed=true";
const MONMAP_PATH: &str = "/var/lib/yolab/heal-monmap";
/// How long after writing its claim a driver waits before reading it back. Two
/// machines that claimed at the same moment both read the last write, and the
/// one that finds the other's id stops.
const CLAIM_SETTLE_SECS: u64 = 15;
/// Kubernetes not answering is normal for a few minutes after a boot — k3s waits
/// for the image store — so it is only called a problem after this much uptime.
const KUBERNETES_GRACE_SECS: u64 = 600;

// ── The record ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    /// Its cluster address: where its mon and its API listen.
    pub addr: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Step {
    Claim,
    Consensus,
    Wipe,
    Restart,
    Finish,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::Claim,
        Step::Consensus,
        Step::Wipe,
        Step::Restart,
        Step::Finish,
    ];

    fn next(self) -> Option<Step> {
        let i = Self::ALL.iter().position(|s| *s == self)?;
        Self::ALL.get(i + 1).copied()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Heal {
    id: String,
    /// The machine carrying it out.
    driver: String,
    started_at: u64,
    finished_at: Option<u64>,
    step: Step,
    /// Confirmed gone by the owner.
    gone: BTreeSet<String>,
    /// The other machines that answered when it started; they are restarted.
    peers: Vec<Machine>,
    /// Whether k3s is reset to the driver (it was the only machine left and k3s
    /// had no quorum).
    reset_kubernetes: bool,
    /// When the claim may be read back. None when Ceph could not be written at
    /// the start, in which case no other machine could have claimed either.
    claim_after: Option<u64>,
    /// The boot the driver restarted from, so the next boot knows it happened.
    restart_boot_id: Option<String>,
    /// What the current step is waiting for, or why it failed last.
    waiting: Option<String>,
}

impl Heal {
    fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    fn mandate(&self) -> Option<HealMandate> {
        self.running()
            .then(|| HealMandate::from_persisted_heal(&self.id, self.gone.clone()))
    }

    /// Whether Ceph can be expected to answer, so the record is worth copying
    /// there. Before consensus, trying would block each save on a timeout.
    fn publishable(&self) -> bool {
        self.claim_after.is_some() || self.step > Step::Consensus
    }
}

/// The driving machine's copy of the record.
#[derive(Clone)]
struct LocalRecord {
    path: PathBuf,
}

impl LocalRecord {
    fn under(root: &Path) -> Self {
        Self {
            path: root.join("var/lib/yolab/heal.json"),
        }
    }

    async fn load(&self) -> Result<Option<Heal>> {
        match tokio::fs::read(&self.path).await {
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .with_context(|| format!("{} is unreadable", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    /// Written whole to a temporary file and renamed over the record, so a crash
    /// mid-write leaves the previous record rather than half of the new one.
    async fn save(&self, heal: &Heal) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec(heal)?).await?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("replace {}", self.path.display()))
    }

    async fn remove(&self) -> Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", self.path.display())),
        }
    }
}

/// Saves locally — the save that counts — then copies to Ceph when it can.
async fn save<H: Host>(host: &H, local: &LocalRecord, heal: &Heal) -> Result<()> {
    local.save(heal).await?;
    if heal.publishable() {
        settings::set_json(host, settings::HEAL, heal)
            .await
            .warn_on_err("heal: copy the record into Ceph");
    }
    Ok(())
}

/// Whether a heal is running anywhere, as far as Ceph knows. `Err` when Ceph
/// cannot say — callers that gate on this treat that as "maybe", which the
/// runtime does.
pub(crate) async fn heal_running() -> Result<bool> {
    Ok(settings::get_json::<_, Heal>(&RealHost, settings::HEAL)
        .await?
        .is_some_and(|h| h.running()))
}

/// Why a backup must not run right now, if it must not: a heal is deleting
/// everything, or app data has no reachable copy and the backup would upload
/// the damage as the newest copy. Not being able to tell is a reason too.
pub(crate) async fn backups_blocked() -> Option<String> {
    match backup_block(&RealHost).await {
        Ok(reason) => reason.map(str::to_string),
        Err(e) => Some(format!("cannot tell whether storage is healthy ({e:#})")),
    }
}

async fn backup_block<H: Host>(host: &H) -> Result<Option<&'static str>> {
    if settings::get_json::<_, Heal>(host, settings::HEAL)
        .await?
        .is_some_and(|h| h.running())
    {
        return Ok(Some("the cluster is being healed"));
    }
    let dump = host.osd_dump().await?;
    let pgs = host.pgs_brief().await?;
    Ok(blocked_by_loss(&model::lost_pgs(&dump, &pgs).0))
}

fn blocked_by_loss(lost: &PgsByPool) -> Option<&'static str> {
    lost.keys()
        .any(|pool| RECOVERABLE_FS_POOLS.contains(&pool.as_str()))
        .then_some("a disk holding app data does not answer — reconnect it, or use FORCE HEAL")
}

// ── Looking at the cluster ────────────────────────────────────────────────────

/// Reaching other machines. A seam so the survey and the restart can be tested.
pub(crate) trait Network: Send + Sync {
    /// Whether `machine` answers at all, on its Ceph or its API port.
    fn answers<'a>(&'a self, machine: &'a Machine) -> impl Future<Output = bool> + Send + 'a;
    /// Asks `machine` to restart.
    fn restart<'a>(&'a self, machine: &'a Machine) -> impl Future<Output = Result<()>> + Send + 'a;
}

pub(crate) struct RealNetwork {
    port: u16,
    token: String,
}

impl RealNetwork {
    fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            port: cfg.port,
            token: cfg.cluster_token(),
        }
    }
}

#[allow(clippy::manual_async_fn)]
impl Network for RealNetwork {
    fn answers<'a>(&'a self, machine: &'a Machine) -> impl Future<Output = bool> + Send + 'a {
        async move {
            // Both ports at once: a machine that is gone costs one timeout, not
            // two, on every page load.
            let reach = |port: u16| async move {
                let connect = tokio::net::TcpStream::connect((machine.addr.as_str(), port));
                matches!(
                    tokio::time::timeout(Duration::from_secs(3), connect).await,
                    Ok(Ok(_))
                )
            };
            let (mon, api) = tokio::join!(reach(3300), reach(self.port));
            mon || api
        }
    }

    fn restart<'a>(&'a self, machine: &'a Machine) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            let url = format!("http://[{}]:{}/api/system/reboot", machine.addr, self.port);
            reqwest::Client::new()
                .post(&url)
                .header(crate::auth::CLUSTER_AUTH_HEADER, &self.token)
                .timeout(Duration::from_secs(10))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
    }
}

/// This machine's mon, asked over its admin socket — which answers with or
/// without a quorum, and carries the monmap it has.
#[derive(Debug, Clone, PartialEq)]
struct MonStatus {
    in_quorum: bool,
    machines: Vec<Machine>,
}

fn parse_mon_status(raw: &str) -> Result<MonStatus> {
    let v: Value = serde_json::from_str(raw).context("mon_status is not JSON")?;
    let state = v["state"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("mon_status has no state"))?;
    let mons = v["monmap"]["mons"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("mon_status has no monmap"))?;
    let machines = mons
        .iter()
        .map(|m| {
            let name = m["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("a mon without a name"))?;
            let addr = m["public_addrs"]["addrvec"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["addr"].as_str())
                .and_then(host_of)
                .ok_or_else(|| anyhow::anyhow!("mon {name} has no address"))?;
            Ok(Machine {
                name: name.to_string(),
                addr,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MonStatus {
        in_quorum: matches!(state, "leader" | "peon"),
        machines,
    })
}

/// `[fd00::1]:3300` → `fd00::1`; `10.0.0.1:3300` → `10.0.0.1`.
fn host_of(addr: &str) -> Option<String> {
    let (host, _port) = addr.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| host.to_string())
}

async fn mon_status<H: Host>(host: &H, me: &str) -> Result<MonStatus> {
    let raw = host
        .ceph(&["daemon", &format!("mon.{me}"), "mon_status"])
        .await
        .context("this machine's Ceph monitor does not answer")?;
    parse_mon_status(&raw)
}

async fn kubernetes_answers<H: Host>(host: &H) -> bool {
    host.kubectl(&["get", "--raw", "/readyz", "--request-timeout=5s"])
        .await
        .is_ok()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MachineState {
    name: String,
    addr: String,
    this_machine: bool,
    answers: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Survey {
    me: String,
    machines: Vec<MachineState>,
    ceph_quorum: bool,
    kubernetes: bool,
    uptime_secs: u64,
    /// Known only while Ceph has a quorum.
    down_osds: Option<BTreeSet<i64>>,
    lost_groups: Option<usize>,
}

impl Survey {
    fn gone(&self) -> BTreeSet<String> {
        self.machines
            .iter()
            .filter(|m| !m.this_machine && !m.answers)
            .map(|m| m.name.clone())
            .collect()
    }

    fn answering_peers(&self) -> Vec<Machine> {
        self.machines
            .iter()
            .filter(|m| !m.this_machine && m.answers)
            .map(|m| Machine {
                name: m.name.clone(),
                addr: m.addr.clone(),
            })
            .collect()
    }

    fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.gone().is_empty() {
            out.push("machines_gone");
        }
        if !self.ceph_quorum {
            out.push("ceph_no_quorum");
        }
        if !self.kubernetes && self.uptime_secs >= KUBERNETES_GRACE_SECS {
            out.push("kubernetes_down");
        }
        if self.lost_groups.is_some_and(|n| n > 0) {
            out.push("data_unreachable");
        }
        out
    }

    fn refusal(&self) -> Option<String> {
        if !self.machines.iter().any(|m| m.this_machine) {
            return Some(format!(
                "{} is not in its own monmap — it cannot drive a heal",
                self.me
            ));
        }
        if self.problems().is_empty() {
            return Some("nothing is wrong — there is nothing to heal".into());
        }
        let answering: Vec<String> = self.answering_peers().into_iter().map(|m| m.name).collect();
        if !self.ceph_quorum && !answering.is_empty() {
            return Some(format!(
                "Ceph has no quorum while {} still answers — that is not a machine being gone. \
                 Make sure every machine is on and connected, or power off the ones that are gone for good",
                answering.join(", ")
            ));
        }
        None
    }

    fn reset_kubernetes(&self) -> bool {
        !self.kubernetes && !self.gone().is_empty() && self.answering_peers().is_empty()
    }
}

async fn survey<H: Host, N: Network>(
    host: &H,
    net: &N,
    me: &str,
    uptime_secs: u64,
) -> Result<Survey> {
    let mon = mon_status(host, me).await?;
    let mut machines = Vec::with_capacity(mon.machines.len());
    for m in &mon.machines {
        let this_machine = m.name == me;
        machines.push(MachineState {
            name: m.name.clone(),
            addr: m.addr.clone(),
            this_machine,
            answers: this_machine || net.answers(m).await,
        });
    }
    let kubernetes = kubernetes_answers(host).await;
    let (down_osds, lost_groups) = if mon.in_quorum {
        match (host.osd_dump().await, host.pgs_brief().await) {
            (Ok(dump), Ok(pgs)) => {
                let lost = model::lost_pgs(&dump, &pgs).0;
                (
                    Some(dump.down()),
                    Some(lost.values().map(BTreeSet::len).sum()),
                )
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    };
    Ok(Survey {
        me: me.to_string(),
        machines,
        ceph_quorum: mon.in_quorum,
        kubernetes,
        uptime_secs,
        down_osds,
        lost_groups,
    })
}

// ── Starting ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct HealRequest {
    /// The machines the owner confirmed gone, by name. Must be exactly the ones
    /// that do not answer right now: anything else means the page is out of date.
    pub remove_machines: BTreeSet<String>,
}

#[allow(clippy::too_many_arguments)]
async fn start_heal<H: Host, N: Network>(
    host: &H,
    net: &N,
    local: &LocalRecord,
    me: &str,
    request: &HealRequest,
    uptime_secs: u64,
    id: String,
    now: u64,
) -> Result<Heal> {
    if let Some(running) = local.load().await?.filter(Heal::running) {
        // Past the restart, what is left only talks to Kubernetes and is safe to
        // start over — and starting over is the way out when it cannot finish (a
        // machine that answered at the start died before k3s came back).
        if running.step < Step::Finish {
            bail!("this machine is already healing the cluster");
        }
        tracing::warn!(
            "heal {}: replaced by a new heal while at {:?}",
            running.id,
            running.step
        );
    }
    let s = survey(host, net, me, uptime_secs).await?;
    if let Some(why) = s.refusal() {
        bail!("{why}");
    }
    let gone = s.gone();
    if request.remove_machines != gone {
        bail!(
            "the machines that do not answer changed since the page was loaded (now: {}) — review and confirm again",
            if gone.is_empty() {
                "none".to_string()
            } else {
                gone.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        );
    }
    if s.ceph_quorum {
        // A record whose driver is this machine (checked above) or is itself gone
        // will never finish: refusing on it would leave the cluster paused for
        // good, with no way to heal it.
        if let Some(other) = settings::get_json::<_, Heal>(host, settings::HEAL)
            .await?
            .filter(|h| h.running() && h.driver != me && !gone.contains(&h.driver))
        {
            bail!("{} is already healing the cluster", other.driver);
        }
    }
    let heal = Heal {
        id,
        driver: me.to_string(),
        started_at: now,
        finished_at: None,
        step: Step::Claim,
        gone,
        peers: s.answering_peers(),
        reset_kubernetes: s.reset_kubernetes(),
        claim_after: s.ceph_quorum.then_some(now + CLAIM_SETTLE_SECS),
        restart_boot_id: None,
        waiting: None,
    };
    if s.ceph_quorum {
        settings::set_json(host, settings::HEAL, &heal).await?;
    }
    local.save(&heal).await?;
    tracing::warn!(
        "heal {} started by {me}: removing {:?}, restarting {:?}, k3s reset: {}",
        heal.id,
        heal.gone,
        heal.peers.iter().map(|p| &p.name).collect::<Vec<_>>(),
        heal.reset_kubernetes
    );
    crate::runtime::wake(NAME);
    Ok(heal)
}

// ── Running ───────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum StepResult {
    Done,
    /// Look again next tick, for this reason.
    NotYet(String),
    /// Another machine's claim won: this one does nothing.
    Abandon,
    /// This machine is restarting.
    Restarting,
}

pub struct HealController {
    pub config: crate::config::Config,
}

impl Controller for HealController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        // The machine the owner clicked drives the heal, whatever leads.
        Scope::Node
    }
    fn interval(&self) -> Duration {
        TICK
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let boot_id = tokio::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .await
            .context("read this boot's id")?;
        tick(
            &RealHost,
            &RealNetwork::from_config(&self.config),
            &LocalRecord::under(Path::new("/")),
            &ctx.node,
            boot_id.trim(),
            now_secs(),
        )
        .await
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn tick<H: Host, N: Network>(
    host: &H,
    net: &N,
    local: &LocalRecord,
    me: &str,
    boot_id: &str,
    now: u64,
) -> Result<Tick> {
    let Some(mut heal) = local.load().await? else {
        return Ok(Tick::Idle("no heal is driven from this machine".into()));
    };
    if !heal.running() {
        publish_finished(host, &heal).await;
        return Ok(Tick::Idle(format!("heal {} finished", heal.id)));
    }
    if boot_id.is_empty() {
        bail!("this boot has no id — a heal cannot tell whether its restart happened");
    }
    while heal.running() {
        tracing::info!("heal {}: {:?}", heal.id, heal.step);
        match run_step(host, net, local, &mut heal, me, boot_id, now).await {
            Err(e) => {
                heal.waiting = Some(format!("{e:#}"));
                save(host, local, &heal).await?;
                return Err(e);
            }
            Ok(StepResult::NotYet(why)) => {
                if heal.waiting.as_deref() != Some(why.as_str()) {
                    heal.waiting = Some(why);
                    save(host, local, &heal).await?;
                }
                return Ok(Tick::Done);
            }
            Ok(StepResult::Abandon) => {
                tracing::warn!(
                    "heal {}: another machine's heal won the claim — stopping",
                    heal.id
                );
                local.remove().await?;
                return Ok(Tick::Idle("another machine drives the heal".into()));
            }
            Ok(StepResult::Restarting) => return Ok(Tick::Done),
            Ok(StepResult::Done) => {
                heal.waiting = None;
                match heal.step.next() {
                    Some(next) => heal.step = next,
                    None => {
                        heal.finished_at = Some(now);
                        tracing::warn!("heal {}: finished", heal.id);
                    }
                }
                save(host, local, &heal).await?;
            }
        }
    }
    Ok(Tick::Done)
}

/// The last copy into Ceph may have failed; other machines stay paused until it
/// lands, so it is retried while the record is here.
///
/// Only over this same heal: a machine keeps its record of a heal it drove long
/// after, and must never write it over a later one another machine started.
async fn publish_finished<H: Host>(host: &H, heal: &Heal) {
    let needed = match settings::get_json::<_, Heal>(host, settings::HEAL).await {
        Ok(None) => true,
        Ok(Some(stored)) => stored.id == heal.id && stored != *heal,
        Err(_) => false,
    };
    if needed {
        settings::set_json(host, settings::HEAL, heal)
            .await
            .debug_on_err("heal: copy the finished record into Ceph");
    }
}

/// The heal to show: this machine's own record or Ceph's, whichever started
/// last. A machine that drove a heal weeks ago still has that record, and it
/// must not hide the one running now.
fn newest(local: Option<Heal>, stored: Option<Heal>) -> Option<Heal> {
    match (local, stored) {
        (Some(l), Some(s)) => Some(if s.started_at > l.started_at { s } else { l }),
        (l, s) => l.or(s),
    }
}

async fn run_step<H: Host, N: Network>(
    host: &H,
    net: &N,
    local: &LocalRecord,
    heal: &mut Heal,
    me: &str,
    boot_id: &str,
    now: u64,
) -> Result<StepResult> {
    use StepResult::*;
    let Some(mandate) = heal.mandate() else {
        bail!("heal {} is not running — no step may run", heal.id);
    };
    match heal.step {
        Step::Claim => {
            let Some(after) = heal.claim_after else {
                return Ok(Done);
            };
            if now < after {
                return Ok(NotYet("making sure no other machine is healing".into()));
            }
            let stored = settings::get_json::<_, Heal>(host, settings::HEAL).await?;
            Ok(match stored {
                Some(h) if h.id == heal.id => Done,
                _ => Abandon,
            })
        }
        Step::Consensus => {
            if let Some(waiting) = ceph_consensus(host, &mandate, heal, me).await? {
                return Ok(NotYet(waiting));
            }
            if heal.reset_kubernetes && !kubernetes_answers(host).await {
                let out = host.systemctl(&["stop", "k3s.service"]).await?;
                if !out.success {
                    bail!("systemctl stop k3s: {}", out.stderr.trim());
                }
                destructive::reset_kubernetes_membership(host, &mandate).await?;
            }
            Ok(Done)
        }
        Step::Wipe => wipe(host, &mandate, heal).await,
        Step::Restart => {
            // Every pool is gone, so a machine left running keeps an image store
            // on a pool that no longer exists. One that did not take the request
            // but still answers is asked again next tick; one that no longer
            // answers is down, and gets a fresh store whenever it boots.
            let mut refused = Vec::new();
            for peer in &heal.peers {
                if let Err(e) = net.restart(peer).await {
                    tracing::warn!("heal: restart {}: {e:#}", peer.name);
                    if net.answers(peer).await {
                        refused.push(peer.name.clone());
                    }
                }
            }
            if !refused.is_empty() {
                return Ok(NotYet(format!(
                    "{} did not accept the restart — trying again",
                    refused.join(", ")
                )));
            }
            heal.step = Step::Finish;
            heal.restart_boot_id = Some(boot_id.to_string());
            heal.waiting = Some("restarting this machine".into());
            save(host, local, heal).await?;
            restart_this_machine(host).await?;
            Ok(Restarting)
        }
        Step::Finish => {
            if heal.restart_boot_id.as_deref() == Some(boot_id) {
                // Saved, but the restart never happened.
                restart_this_machine(host).await?;
                return Ok(Restarting);
            }
            if !kubernetes_answers(host).await {
                return Ok(NotYet("waiting for Kubernetes to start".into()));
            }
            for machine in &heal.gone {
                host.kubectl(&["delete", "node", machine, "--ignore-not-found"])
                    .await?;
            }
            remove_apps(host).await
        }
    }
}

/// Removes the gone machines' mons. `Some(reason)` while Ceph has no quorum yet.
async fn ceph_consensus<H: Host>(
    host: &H,
    mandate: &HealMandate,
    heal: &Heal,
    me: &str,
) -> Result<Option<String>> {
    let status = mon_status(host, me).await?;
    let listed: Vec<String> = status
        .machines
        .iter()
        .filter(|m| heal.gone.contains(&m.name))
        .map(|m| m.name.clone())
        .collect();
    let quorum = host
        .ceph_json(&["--connect-timeout", "10", "mon", "dump"])
        .await
        .is_ok();
    if quorum {
        for machine in &listed {
            destructive::remove_mon(host, mandate, machine).await?;
        }
        return Ok(None);
    }
    if !listed.is_empty() {
        destructive::remove_mons_offline(host, mandate, me, &listed, MONMAP_PATH).await?;
    }
    Ok(Some("waiting for Ceph to form a quorum".into()))
}

async fn restart_this_machine<H: Host>(host: &H) -> Result<()> {
    tracing::warn!("heal: restarting this machine");
    let out = host.systemctl(&["reboot"]).await?;
    if !out.success {
        bail!("systemctl reboot: {}", out.stderr.trim());
    }
    Ok(())
}

/// OSD ids per CRUSH host, from `ceph osd tree`.
fn osds_on_hosts(tree: &Value, hosts: &BTreeSet<String>) -> BTreeSet<i64> {
    tree["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["type"] == "host" && n["name"].as_str().is_some_and(|h| hosts.contains(h)))
        .flat_map(|n| n["children"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c.as_i64())
        .filter(|id| *id >= 0)
        .collect()
}

/// Everything but the machines: every OSD that is not up, the gone machines'
/// traces, every pool and the app filesystem, and every disk switch and list.
///
/// No bookkeeping of which disk was which. OSDs that still run keep running on
/// empty storage; after the restart each machine's disk controller finds no
/// switch for its disks and registers them OFF, which empties and purges any
/// such OSD through its ordinary, safe-to-destroy path. A system disk whose OSD
/// was purged here is made an OSD again at boot (`disks_reconciler`).
///
/// Idempotent, so a crash anywhere repeats it from the top.
async fn wipe<H: Host>(host: &H, mandate: &HealMandate, heal: &Heal) -> Result<StepResult> {
    let tree = host.ceph_json(&["osd", "tree"]).await?;
    let on_gone = osds_on_hosts(&tree, &heal.gone);
    let dump = host.osd_dump().await?;
    let up = dump.up();
    for id in on_gone.iter().filter(|id| up.contains(id)) {
        // Its machine is gone; Ceph has not noticed yet.
        host.ceph(&["osd", "down", &format!("osd.{id}")]).await?;
    }
    let targets: BTreeSet<i64> = dump.down().union(&on_gone).copied().collect();
    for id in &targets {
        destructive::purge_down(host, mandate, *id)
            .await
            .warn_on_err(format!("heal: purge osd.{id}"));
    }

    for machine in &heal.gone {
        host.ceph(&["osd", "crush", "rm", machine])
            .await
            .warn_on_err(format!("heal: remove host {machine} from the CRUSH map"));
        destructive::forget_daemons(host, mandate, machine).await?;
        settings::set(
            host,
            &format!("{}{machine}", settings::REMOVED_MACHINES),
            &heal.id,
        )
        .await?;
    }

    let fs_exists = {
        let ls: Vec<model::FsEntry> = serde_json::from_value(host.ceph_json(&["fs", "ls"]).await?)
            .context("ceph fs ls")?;
        ls.iter().any(|f| f.name == RECOVERABLE_FS)
    };
    let pools: Vec<String> = host
        .ceph(&["osd", "pool", "ls"])
        .await?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    destructive::delete_all_storage(host, mandate, fs_exists, &pools).await?;

    for prefix in [settings::DISKS, settings::DISK_STATUS] {
        for key in settings::dump(host, prefix).await?.into_keys() {
            settings::remove(host, &format!("{prefix}{key}")).await?;
        }
    }

    let left: Vec<i64> = host
        .osd_ids()
        .await?
        .into_iter()
        .filter(|id| targets.contains(id))
        .collect();
    Ok(if left.is_empty() {
        StepResult::Done
    } else {
        StepResult::NotYet(format!("waiting for {left:?} to be purged"))
    })
}

/// Tears every app down without waiting on anything the deleted filesystem held:
/// pods are forced off, and finalizers that wait for the CSI driver to delete a
/// volume that no longer exists are removed.
///
/// Every command is best effort — each namespace is retried next tick until none
/// is left — but a failure is logged, never discarded.
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
        .warn_on_err(format!("heal: scale down {ns}"));
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
        .warn_on_err(format!("heal: delete pods in {ns}"));
        let pvcs = match host
            .kubectl_json(&["get", "pvc", "-n", ns, "-o", "json"])
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("heal: list PVCs in {ns}: {e}");
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
            .debug_on_err(format!("heal: clear finalizers on pvc {ns}/{name}"));
            if let Some(pv) = p["spec"]["volumeName"].as_str() {
                host.kubectl(&["patch", "pv", pv, "--type", "merge", "-p", NO_FINALIZERS])
                    .await
                    .debug_on_err(format!("heal: clear finalizers on pv {pv}"));
                host.kubectl(&["delete", "pv", pv, "--wait=false", "--ignore-not-found"])
                    .await
                    .warn_on_err(format!("heal: delete pv {pv}"));
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
        .warn_on_err(format!("heal: delete namespace {ns}"));
    }
    Ok(if live.is_empty() {
        StepResult::Done
    } else {
        StepResult::NotYet(format!("waiting for {} app(s) to be removed", live.len()))
    })
}

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

// ── HTTP ──────────────────────────────────────────────────────────────────────

fn heal_json(heal: &Heal) -> Value {
    json!({
        "id": heal.id,
        "driver": heal.driver,
        "running": heal.running(),
        "started_at": heal.started_at,
        "finished_at": heal.finished_at,
        "step": heal.step,
        "steps": Step::ALL,
        "removed_machines": heal.gone,
        "restarted_machines": heal.peers.iter().map(|p| &p.name).collect::<Vec<_>>(),
        "reset_kubernetes": heal.reset_kubernetes,
        "waiting": heal.waiting,
    })
}

fn status_json(survey: &Survey, heal: Option<&Heal>) -> Value {
    json!({
        "survey": survey,
        "problems": survey.problems(),
        "refusal": survey.refusal(),
        "plan": {
            "remove_machines": survey.gone(),
            "restart_machines": survey.answering_peers().into_iter().map(|m| m.name).collect::<Vec<_>>(),
            "reset_kubernetes": survey.reset_kubernetes(),
        },
        "heal": heal.map(heal_json),
    })
}

fn unavailable(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": e.to_string() })),
    )
}

async fn uptime_secs() -> u64 {
    tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// `GET /api/heal` — what is wrong, what a heal would do, and the current or
/// last heal. Served by any machine, whether or not Ceph or Kubernetes answer.
pub async fn get_status(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let host = RealHost;
    let me = crate::system::hostname();
    let net = RealNetwork::from_config(&s.config);
    let survey = match survey(&host, &net, &me, uptime_secs().await).await {
        Ok(v) => v,
        Err(e) => return unavailable(format!("{e:#}")),
    };
    let local = match LocalRecord::under(Path::new("/")).load().await {
        Ok(h) => h,
        Err(e) => return unavailable(format!("{e:#}")),
    };
    let stored = if survey.ceph_quorum {
        settings::get_json::<_, Heal>(&host, settings::HEAL)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let heal = newest(local, stored);
    (StatusCode::OK, Json(status_json(&survey, heal.as_ref())))
}

/// `POST /api/heal` with a `HealRequest`. This machine drives the heal.
pub async fn post_heal(
    State(s): State<AppState>,
    Json(request): Json<HealRequest>,
) -> (StatusCode, Json<Value>) {
    let me = crate::system::hostname();
    let net = RealNetwork::from_config(&s.config);
    let id = crate::routers::backup_common::random_hex(8);
    match start_heal(
        &RealHost,
        &net,
        &LocalRecord::under(Path::new("/")),
        &me,
        &request,
        uptime_secs().await,
        id,
        now_secs(),
    )
    .await
    {
        Ok(heal) => (StatusCode::OK, Json(heal_json(&heal))),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use std::sync::Mutex;

    const NOW: u64 = 1_000_000;
    const HEAL_GET: &str = "ceph config-key get yolab/heal";
    const HEAL_SET: &str = "ceph config-key set yolab/heal";
    const NO_KEY: &str = "Error ENOENT: key doesn't exist";
    const MON_STATUS: &str = "ceph daemon mon.node1 mon_status";

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn machine(name: &str, addr: &str) -> Machine {
        Machine {
            name: name.into(),
            addr: addr.into(),
        }
    }

    #[derive(Default)]
    struct FakeNetwork {
        answering: BTreeSet<String>,
        /// Answer, but reject the restart request.
        refusing: BTreeSet<String>,
        restarted: Mutex<Vec<String>>,
    }

    impl FakeNetwork {
        fn answering(names: &[&str]) -> Self {
            Self {
                answering: names.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Network for FakeNetwork {
        fn answers<'a>(&'a self, m: &'a Machine) -> impl Future<Output = bool> + Send + 'a {
            async move { self.answering.contains(&m.name) }
        }
        fn restart<'a>(&'a self, m: &'a Machine) -> impl Future<Output = Result<()>> + Send + 'a {
            async move {
                self.restarted.lock().unwrap().push(m.name.clone());
                if self.answering.contains(&m.name) && !self.refusing.contains(&m.name) {
                    Ok(())
                } else {
                    bail!("no answer")
                }
            }
        }
    }

    fn mon_status_json(state: &str, mons: &[&str]) -> String {
        json!({
            "name": "node1",
            "state": state,
            "monmap": {"mons": mons.iter().enumerate().map(|(i, n)| json!({
                "name": n,
                "public_addrs": {"addrvec": [
                    {"type": "v2", "addr": format!("[fd00::{}]:3300", i + 1), "nonce": 0},
                    {"type": "v1", "addr": format!("[fd00::{}]:6789", i + 1), "nonce": 0},
                ]},
            })).collect::<Vec<_>>()},
        })
        .to_string()
    }

    fn dump(up: &[(i64, bool)]) -> String {
        json!({
            "osds": up.iter().map(|(id, u)| json!({"osd": id, "up": i32::from(*u), "in": 1})).collect::<Vec<_>>(),
            "pools": [{"pool": 3, "pool_name": "yolab-fs-data0"}],
        })
        .to_string()
    }

    fn heal_at(step: Step) -> Heal {
        Heal {
            id: "h1".into(),
            driver: "node1".into(),
            started_at: NOW - 100,
            finished_at: None,
            step,
            gone: names(&["node2"]),
            peers: vec![machine("node3", "fd00::3")],
            reset_kubernetes: false,
            claim_after: Some(NOW - 50),
            restart_boot_id: None,
            waiting: None,
        }
    }

    fn local() -> (tempfile::TempDir, LocalRecord) {
        let dir = tempfile::tempdir().unwrap();
        let record = LocalRecord::under(dir.path());
        (dir, record)
    }

    // ── Reading the cluster ──────────────────────────────────────────────────

    #[test]
    fn mon_status_gives_quorum_and_every_machine_with_its_address() {
        let s = parse_mon_status(&mon_status_json("peon", &["node1", "node2"])).unwrap();
        assert!(s.in_quorum);
        assert_eq!(
            s.machines,
            vec![machine("node1", "fd00::1"), machine("node2", "fd00::2")]
        );
        assert!(
            !parse_mon_status(&mon_status_json("probing", &["node1"]))
                .unwrap()
                .in_quorum
        );
        assert!(
            !parse_mon_status(&mon_status_json("electing", &["node1"]))
                .unwrap()
                .in_quorum
        );
        assert!(parse_mon_status("{}").is_err());
        assert!(
            parse_mon_status(r#"{"state":"leader","monmap":{"mons":[{"name":"x"}]}}"#).is_err()
        );
    }

    #[test]
    fn an_address_is_read_without_its_port() {
        assert_eq!(host_of("[fd00::1]:3300").as_deref(), Some("fd00::1"));
        assert_eq!(host_of("10.0.0.1:6789").as_deref(), Some("10.0.0.1"));
        assert_eq!(host_of("nope"), None);
    }

    fn survey_host(state: &str, mons: &[&str], kubernetes: bool) -> FakeHost {
        let host = FakeHost::new()
            .ok(MON_STATUS, &mon_status_json(state, mons))
            .ok("ceph osd dump", &dump(&[(0, true), (1, false)]))
            .ok(
                "ceph pg dump pgs_brief",
                r#"[{"pgid": "3.1", "state": "stale+active+clean", "acting": [1]}]"#,
            );
        if kubernetes {
            host.ok("kubectl get --raw /readyz", "ok")
        } else {
            host.fail("kubectl get --raw /readyz", "connection refused")
        }
    }

    #[tokio::test]
    async fn a_survey_sees_who_answers_and_what_is_lost() {
        let host = survey_host("leader", &["node1", "node2", "node3"], true);
        let s = survey(&host, &FakeNetwork::answering(&["node3"]), "node1", 3600)
            .await
            .unwrap();
        assert_eq!(s.gone(), names(&["node2"]));
        assert_eq!(s.answering_peers(), vec![machine("node3", "fd00::3")]);
        assert!(s.ceph_quorum && s.kubernetes);
        assert_eq!(s.down_osds, Some(BTreeSet::from([1])));
        assert_eq!(s.lost_groups, Some(1));
        assert_eq!(s.problems(), vec!["machines_gone", "data_unreachable"]);
        assert_eq!(s.refusal(), None);
        assert!(!s.reset_kubernetes());
    }

    #[tokio::test]
    async fn without_quorum_the_survey_does_not_ask_ceph_about_disks() {
        let host = survey_host("probing", &["node1", "node2"], false);
        let s = survey(&host, &FakeNetwork::default(), "node1", 3600)
            .await
            .unwrap();
        assert!(!s.ceph_quorum && !s.kubernetes);
        assert_eq!((&s.down_osds, s.lost_groups), (&None, None));
        assert!(!host.ran("osd dump"));
        assert_eq!(
            s.problems(),
            vec!["machines_gone", "ceph_no_quorum", "kubernetes_down"]
        );
        assert!(
            s.reset_kubernetes(),
            "the only machine left, and k3s has no quorum"
        );
    }

    #[tokio::test]
    async fn a_machine_whose_mon_does_not_answer_cannot_survey() {
        let host = FakeHost::new().fail(MON_STATUS, "admin socket not found");
        assert!(survey(&host, &FakeNetwork::default(), "node1", 0)
            .await
            .is_err());
    }

    fn survey_of(
        machines: &[(&str, bool)],
        ceph_quorum: bool,
        kubernetes: bool,
        lost: usize,
    ) -> Survey {
        Survey {
            me: "node1".into(),
            machines: machines
                .iter()
                .enumerate()
                .map(|(i, (name, answers))| MachineState {
                    name: name.to_string(),
                    addr: format!("fd00::{}", i + 1),
                    this_machine: *name == "node1",
                    answers: *answers,
                })
                .collect(),
            ceph_quorum,
            kubernetes,
            uptime_secs: 3600,
            down_osds: None,
            lost_groups: ceph_quorum.then_some(lost),
        }
    }

    #[test]
    fn a_healthy_cluster_has_nothing_to_heal() {
        let s = survey_of(&[("node1", true), ("node2", true)], true, true, 0);
        assert!(s.problems().is_empty());
        assert!(s.refusal().unwrap().contains("nothing to heal"));
    }

    #[test]
    fn a_lost_quorum_with_another_machine_answering_is_refused() {
        let s = survey_of(&[("node1", true), ("node2", true)], false, true, 0);
        assert!(s.refusal().unwrap().contains("node2 still answers"));
    }

    #[test]
    fn a_lost_disk_between_two_answering_machines_is_healed_without_resetting_k3s() {
        let s = survey_of(&[("node1", true), ("node2", true)], true, false, 3);
        assert_eq!(s.refusal(), None);
        assert!(s.gone().is_empty());
        assert!(
            !s.reset_kubernetes(),
            "the other member answers: k3s comes back after restarts"
        );
    }

    #[test]
    fn kubernetes_down_right_after_boot_is_not_yet_a_problem() {
        let mut s = survey_of(&[("node1", true)], true, false, 0);
        s.uptime_secs = 60;
        assert!(s.problems().is_empty());
        s.uptime_secs = KUBERNETES_GRACE_SECS;
        assert_eq!(s.problems(), vec!["kubernetes_down"]);
    }

    #[test]
    fn a_machine_missing_from_its_own_monmap_cannot_drive() {
        let s = survey_of(&[("node2", false)], false, false, 0);
        assert!(s.refusal().unwrap().contains("not in its own monmap"));
    }

    // ── Starting ─────────────────────────────────────────────────────────────

    fn request(machines: &[&str]) -> HealRequest {
        HealRequest {
            remove_machines: names(machines),
        }
    }

    #[tokio::test]
    async fn starting_with_quorum_claims_in_ceph_and_saves_locally() {
        let host = survey_host("leader", &["node1", "node2", "node3"], true)
            .fail(HEAL_GET, NO_KEY)
            .ok(HEAL_SET, "");
        let (_d, rec) = local();
        let net = FakeNetwork::answering(&["node3"]);
        let heal = start_heal(
            &host,
            &net,
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h1".into(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(heal.step, Step::Claim);
        assert_eq!(heal.gone, names(&["node2"]));
        assert_eq!(heal.peers, vec![machine("node3", "fd00::3")]);
        assert_eq!(heal.claim_after, Some(NOW + CLAIM_SETTLE_SECS));
        assert!(!heal.reset_kubernetes);
        assert!(host.ran(HEAL_SET));
        assert_eq!(rec.load().await.unwrap(), Some(heal));
        assert!(
            !host.ran("mon remove") && !host.ran("osd purge"),
            "the controller does the work"
        );
    }

    #[tokio::test]
    async fn starting_without_quorum_saves_only_locally() {
        let host = survey_host("probing", &["node1", "node2"], false);
        let (_d, rec) = local();
        let heal = start_heal(
            &host,
            &FakeNetwork::default(),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h1".into(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(heal.claim_after, None);
        assert!(heal.reset_kubernetes);
        assert!(!host.ran("config-key"));
    }

    #[tokio::test]
    async fn a_confirmation_that_no_longer_matches_is_refused() {
        for confirmed in [vec![], vec!["node2", "node3"]] {
            let host = survey_host("leader", &["node1", "node2", "node3"], true);
            let (_d, rec) = local();
            let err = start_heal(
                &host,
                &FakeNetwork::answering(&["node3"]),
                &rec,
                "node1",
                &request(&confirmed),
                3600,
                "h1".into(),
                NOW,
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("changed"), "{err}");
            assert_eq!(rec.load().await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn a_heal_already_running_here_or_elsewhere_is_refused() {
        let (_d, rec) = local();
        rec.save(&heal_at(Step::Wipe)).await.unwrap();
        let host = survey_host("leader", &["node1", "node2"], true);
        let err = start_heal(
            &host,
            &FakeNetwork::default(),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h2".into(),
            NOW,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already healing"));

        let (_d, rec) = local();
        let mut elsewhere = heal_at(Step::Wipe);
        elsewhere.driver = "node3".into();
        let host = survey_host("leader", &["node1", "node2", "node3"], true)
            .ok(HEAL_GET, &serde_json::to_string(&elsewhere).unwrap());
        let err = start_heal(
            &host,
            &FakeNetwork::answering(&["node3"]),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h2".into(),
            NOW,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("node3 is already healing"),
            "{err}"
        );
        assert!(!host.ran(HEAL_SET));
    }

    // ── The record ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_local_record_round_trips_and_junk_is_an_error() {
        let (dir, rec) = local();
        assert_eq!(rec.load().await.unwrap(), None);
        let heal = heal_at(Step::Finish);
        rec.save(&heal).await.unwrap();
        assert_eq!(rec.load().await.unwrap(), Some(heal));
        std::fs::write(dir.path().join("var/lib/yolab/heal.json"), "{nope").unwrap();
        assert!(rec.load().await.is_err());
        rec.remove().await.unwrap();
        rec.remove().await.unwrap();
        assert_eq!(rec.load().await.unwrap(), None);
    }

    #[test]
    fn the_steps_are_in_order_and_the_record_is_published_once_ceph_can_answer() {
        let mut s = Step::Claim;
        let mut order = vec![s];
        while let Some(n) = s.next() {
            order.push(n);
            s = n;
        }
        assert_eq!(order, Step::ALL.to_vec());

        let mut h = heal_at(Step::Consensus);
        assert!(h.publishable(), "Ceph answered at the start");
        h.claim_after = None;
        assert!(!h.publishable());
        h.step = Step::Wipe;
        assert!(h.publishable());
    }

    #[tokio::test]
    async fn heal_state_gates_backups() {
        let host = FakeHost::new().ok(
            HEAL_GET,
            &serde_json::to_string(&heal_at(Step::Wipe)).unwrap(),
        );
        assert!(backup_block(&host).await.unwrap().is_some());

        let host = FakeHost::new()
            .fail(HEAL_GET, NO_KEY)
            .ok("ceph osd dump", &dump(&[(0, true), (1, false)]))
            .ok(
                "ceph pg dump pgs_brief",
                r#"[{"pgid": "3.1", "state": "stale", "acting": [1]}]"#,
            );
        assert!(backup_block(&host)
            .await
            .unwrap()
            .unwrap()
            .contains("FORCE HEAL"));

        let host = FakeHost::new().fail(HEAL_GET, "timed out");
        assert!(backup_block(&host).await.is_err());
    }

    // ── Steps ────────────────────────────────────────────────────────────────

    async fn step(host: &FakeHost, heal: &mut Heal) -> Result<StepResult> {
        let (_d, rec) = local();
        run_step(
            host,
            &FakeNetwork::answering(&["node3"]),
            &rec,
            heal,
            "node1",
            "boot-a",
            NOW,
        )
        .await
    }

    #[tokio::test]
    async fn the_claim_waits_and_then_yields_to_a_later_claim() {
        let mut h = heal_at(Step::Claim);
        h.claim_after = Some(NOW + 1);
        assert!(matches!(
            step(&FakeHost::new(), &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));

        h.claim_after = Some(NOW);
        let mine = FakeHost::new().ok(HEAL_GET, &serde_json::to_string(&h).unwrap());
        assert_eq!(step(&mine, &mut h).await.unwrap(), StepResult::Done);

        let mut theirs = h.clone();
        theirs.id = "h2".into();
        let lost = FakeHost::new().ok(HEAL_GET, &serde_json::to_string(&theirs).unwrap());
        assert_eq!(step(&lost, &mut h).await.unwrap(), StepResult::Abandon);

        h.claim_after = None;
        assert_eq!(
            step(&FakeHost::new(), &mut h).await.unwrap(),
            StepResult::Done
        );
    }

    #[tokio::test]
    async fn with_quorum_the_gone_machines_mons_are_removed_online() {
        let host = FakeHost::new()
            .ok("ceph --connect-timeout 10 mon dump", "{}")
            .ok(
                MON_STATUS,
                &mon_status_json("leader", &["node1", "node2", "node3"]),
            )
            .ok("ceph mon remove", "");
        let mut h = heal_at(Step::Consensus);
        assert_eq!(step(&host, &mut h).await.unwrap(), StepResult::Done);
        assert!(host.ran("ceph mon remove node2"));
        assert!(!host.ran("mon remove node3") && !host.ran("monmaptool"));
        assert!(!host.ran("cluster-reset"), "k3s was not to be reset");
    }

    #[tokio::test]
    async fn without_quorum_the_monmap_is_edited_offline_once_then_waited_on() {
        let host = FakeHost::new()
            .fail("ceph --connect-timeout 10 mon dump", "timed out")
            .ok(MON_STATUS, &mon_status_json("probing", &["node1", "node2"]))
            .ok("systemctl", "")
            .ok("ceph-mon", "")
            .ok("monmaptool", "");
        let mut h = heal_at(Step::Consensus);
        assert!(matches!(
            step(&host, &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));
        assert!(host.ran(&format!("monmaptool {MONMAP_PATH} --rm node2")));

        let edited = FakeHost::new()
            .fail("ceph --connect-timeout 10 mon dump", "timed out")
            .ok(MON_STATUS, &mon_status_json("electing", &["node1"]));
        assert!(matches!(
            step(&edited, &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));
        assert!(!edited.ran("monmaptool") && !edited.ran("systemctl"));
    }

    #[tokio::test]
    async fn k3s_is_reset_only_when_the_heal_decided_so_and_it_still_does_not_answer() {
        let quorum = || {
            FakeHost::new()
                .ok("ceph --connect-timeout 10 mon dump", "{}")
                .ok(MON_STATUS, &mon_status_json("leader", &["node1"]))
        };
        let mut h = heal_at(Step::Consensus);
        h.reset_kubernetes = true;
        let answering = quorum().ok("kubectl get --raw /readyz", "ok");
        assert_eq!(step(&answering, &mut h).await.unwrap(), StepResult::Done);
        assert!(!answering.ran("cluster-reset"));

        let down = quorum()
            .fail("kubectl get --raw /readyz", "refused")
            .ok("systemctl stop k3s.service", "")
            .ok("k3s server --cluster-reset", "");
        assert_eq!(step(&down, &mut h).await.unwrap(), StepResult::Done);
        assert!(
            down.position("systemctl stop k3s.service")
                < down.position("k3s server --cluster-reset")
        );

        let no_quorum = FakeHost::new()
            .fail("ceph --connect-timeout 10 mon dump", "timed out")
            .ok(MON_STATUS, &mon_status_json("electing", &["node1"]));
        assert!(matches!(
            step(&no_quorum, &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));
        assert!(
            !no_quorum.ran("cluster-reset"),
            "Ceph first, Kubernetes after"
        );
    }

    fn tree() -> String {
        json!({"nodes": [
            {"id": -2, "name": "node1", "type": "host", "children": [0, 1]},
            {"id": -3, "name": "node2", "type": "host", "children": [2]},
            {"id": 0, "name": "osd.0", "type": "osd"},
        ]})
        .to_string()
    }

    #[test]
    fn osds_are_found_under_their_host() {
        let t: Value = serde_json::from_str(&tree()).unwrap();
        assert_eq!(osds_on_hosts(&t, &names(&["node2"])), BTreeSet::from([2]));
        assert!(osds_on_hosts(&t, &names(&["node9"])).is_empty());
    }

    fn wipe_host(osd_ls_after: &str) -> FakeHost {
        FakeHost::new()
            .ok("ceph osd tree", &tree())
            .ok("ceph osd dump", &dump(&[(0, true), (1, false), (2, true)]))
            // After `osd down`, the gone machine's OSD reads down.
            .ok("ceph osd dump", &dump(&[(0, true), (1, false), (2, false)]))
            .ok("ceph osd down", "")
            .ok("ceph osd purge", "")
            .ok("ceph osd ls", osd_ls_after)
            .ok("ceph osd crush rm", "")
            .ok("ceph auth del", "")
            .ok("ceph config-key set yolab/removed-machines/node2", "")
            .ok("ceph fs ls", r#"[{"name": "yolab-fs"}]"#)
            .ok("ceph fs", "")
            .ok(
                "ceph osd pool ls",
                ".mgr\nimages\nyolab-fs-metadata\nyolab-fs-data0\n",
            )
            .ok("ceph config set mon", "")
            .ok("ceph osd pool delete", "")
            .ok(
                "ceph config-key dump yolab/disks/",
                r#"{"yolab/disks/serial-wwn-9": "ON", "yolab/disks/node1--dev-sdb": "OFF"}"#,
            )
            .ok(
                "ceph config-key dump yolab/disk-status/",
                r#"{"yolab/disk-status/node1": "{}", "yolab/disk-status/node2": "{}"}"#,
            )
            .ok("ceph config-key rm", "")
    }

    #[tokio::test]
    async fn wipe_keeps_only_what_runs_and_throws_everything_else_away() {
        let host = wipe_host("[0]");
        let mut h = heal_at(Step::Wipe);
        assert_eq!(step(&host, &mut h).await.unwrap(), StepResult::Done);

        assert!(
            host.ran("ceph osd down osd.2"),
            "node2 is gone even if Ceph thinks osd.2 is up"
        );
        assert!(host.ran("ceph osd purge osd.1") && host.ran("ceph osd purge osd.2"));
        assert!(!host.ran("purge osd.0"), "osd.0 is up");
        assert!(host.ran("ceph osd crush rm node2"));
        assert!(host.ran("ceph auth del mgr.node2"));
        assert!(host.ran("ceph config-key set yolab/removed-machines/node2 h1"));
        for pool in [".mgr", "images", "yolab-fs-metadata", "yolab-fs-data0"] {
            assert!(host.ran(&format!("pool delete {pool} {pool}")), "{pool}");
        }
        for key in [
            "yolab/disks/serial-wwn-9",
            "yolab/disks/node1--dev-sdb",
            "yolab/disk-status/node1",
            "yolab/disk-status/node2",
        ] {
            assert!(host.ran(&format!("ceph config-key rm {key}")), "{key}");
        }
    }

    #[tokio::test]
    async fn wipe_waits_while_a_purged_osd_is_still_listed() {
        let host = wipe_host("[0, 2]").fail("ceph osd purge osd.2", "EBUSY");
        let mut h = heal_at(Step::Wipe);
        assert!(matches!(
            step(&host, &mut h).await.unwrap(),
            StepResult::NotYet(ref why) if why.contains('2')
        ));
    }

    #[tokio::test]
    async fn restart_asks_every_peer_then_saves_and_restarts_this_machine() {
        let (_d, rec) = local();
        let host = FakeHost::new().ok(HEAL_SET, "").ok("systemctl reboot", "");
        let net = FakeNetwork::default();
        let mut h = heal_at(Step::Restart);
        let r = run_step(&host, &net, &rec, &mut h, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert_eq!(r, StepResult::Restarting);
        assert_eq!(
            *net.restarted.lock().unwrap(),
            vec!["node3".to_string()],
            "a machine that no longer answers is not waited on"
        );
        let saved = rec.load().await.unwrap().unwrap();
        assert_eq!(saved.step, Step::Finish);
        assert_eq!(saved.restart_boot_id.as_deref(), Some("boot-a"));
        assert!(host.position(HEAL_SET) < host.position("systemctl reboot"));
    }

    #[tokio::test]
    async fn a_machine_that_answers_but_refuses_the_restart_is_asked_again() {
        let (_d, rec) = local();
        let host = FakeHost::new().ok(HEAL_SET, "").ok("systemctl reboot", "");
        let net = FakeNetwork {
            answering: names(&["node3"]),
            refusing: names(&["node3"]),
            ..Default::default()
        };
        let mut h = heal_at(Step::Restart);
        let r = run_step(&host, &net, &rec, &mut h, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert!(
            matches!(r, StepResult::NotYet(ref why) if why.contains("node3")),
            "{r:?}"
        );
        assert!(!host.ran("systemctl reboot"), "this machine waits for it");
        assert_eq!(h.step, Step::Restart);
    }

    #[tokio::test]
    async fn finish_restarts_again_if_the_restart_never_happened() {
        let mut h = heal_at(Step::Finish);
        h.restart_boot_id = Some("boot-a".into());
        let host = FakeHost::new().ok("systemctl reboot", "");
        assert_eq!(step(&host, &mut h).await.unwrap(), StepResult::Restarting);

        h.restart_boot_id = Some("boot-z".into());
        let starting = FakeHost::new().fail("kubectl get --raw /readyz", "refused");
        assert!(matches!(
            step(&starting, &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));
        assert!(!starting.ran("systemctl"));
    }

    #[tokio::test]
    async fn finish_forgets_the_gone_machines_and_removes_every_app() {
        let host = FakeHost::new()
            .ok("kubectl get --raw /readyz", "ok")
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": [{"metadata": {"name": "yolab-a"}}]}"#,
            )
            .ok(
                "kubectl get namespaces -l yolab.io/managed=true",
                r#"{"items": []}"#,
            )
            .ok("kubectl scale", "")
            .ok("kubectl delete", "")
            .ok("kubectl patch", "")
            .ok(
                "kubectl get pvc -n yolab-a",
                r#"{"items": [{"metadata": {"name": "data"}, "spec": {"volumeName": "pv1"}}]}"#,
            );
        let mut h = heal_at(Step::Finish);
        h.restart_boot_id = Some("boot-z".into());
        assert!(matches!(
            step(&host, &mut h).await.unwrap(),
            StepResult::NotYet(_)
        ));
        assert!(host.ran("kubectl delete node node2 --ignore-not-found"));
        assert!(
            host.ran("kubectl delete pod --all -n yolab-a --force --grace-period=0 --wait=false")
        );
        assert!(host.ran("kubectl patch pv pv1 --type merge"));
        assert!(host.ran("kubectl delete namespace yolab-a --wait=false"));
        assert_eq!(step(&host, &mut h).await.unwrap(), StepResult::Done);
    }

    #[tokio::test]
    async fn no_step_runs_for_a_finished_heal() {
        let mut h = heal_at(Step::Wipe);
        h.finished_at = Some(NOW);
        let host = FakeHost::new();
        assert!(step(&host, &mut h).await.is_err());
        assert!(host.calls().is_empty());
    }

    // ── The tick ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn without_a_local_record_the_tick_does_nothing() {
        let (_d, rec) = local();
        let host = FakeHost::new();
        let t = tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert!(matches!(t, Tick::Idle(_)));
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn a_finished_heal_is_copied_into_ceph_until_it_lands() {
        let (_d, rec) = local();
        let mut h = heal_at(Step::Finish);
        h.finished_at = Some(NOW);
        rec.save(&h).await.unwrap();
        let host = FakeHost::new().fail(HEAL_GET, NO_KEY).ok(HEAL_SET, "");
        tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert!(host.ran(HEAL_SET));

        let landed = FakeHost::new().ok(HEAL_GET, &serde_json::to_string(&h).unwrap());
        tick(
            &landed,
            &FakeNetwork::default(),
            &rec,
            "node1",
            "boot-a",
            NOW,
        )
        .await
        .unwrap();
        assert!(!landed.ran(HEAL_SET));
    }

    #[tokio::test]
    async fn a_failing_step_is_recorded_and_retried() {
        let (_d, rec) = local();
        rec.save(&heal_at(Step::Wipe)).await.unwrap();
        let host = FakeHost::new()
            .fail("ceph osd tree", "timed out")
            .ok(HEAL_SET, "");
        assert!(
            tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
                .await
                .is_err()
        );
        let saved = rec.load().await.unwrap().unwrap();
        assert_eq!(saved.step, Step::Wipe);
        assert!(saved.waiting.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_heal_that_loses_its_claim_removes_its_record() {
        let (_d, rec) = local();
        rec.save(&heal_at(Step::Claim)).await.unwrap();
        let mut theirs = heal_at(Step::Claim);
        theirs.id = "h2".into();
        let host = FakeHost::new().ok(HEAL_GET, &serde_json::to_string(&theirs).unwrap());
        tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert_eq!(rec.load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_tick_runs_steps_until_one_must_wait() {
        let (_d, rec) = local();
        let mut h = heal_at(Step::Claim);
        h.claim_after = None;
        rec.save(&h).await.unwrap();
        let host = FakeHost::new()
            .ok(HEAL_SET, "")
            .ok("ceph --connect-timeout 10 mon dump", "{}")
            .ok(
                MON_STATUS,
                &mon_status_json("leader", &["node1", "node2", "node3"]),
            )
            .ok("ceph mon remove", "")
            .fail("ceph osd tree", "timed out");
        assert!(
            tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
                .await
                .is_err()
        );
        let saved = rec.load().await.unwrap().unwrap();
        assert_eq!(saved.step, Step::Wipe, "claim and consensus were saved");
    }

    #[tokio::test]
    async fn a_tick_refuses_to_run_without_a_boot_id() {
        let (_d, rec) = local();
        rec.save(&heal_at(Step::Restart)).await.unwrap();
        assert!(tick(
            &FakeHost::new(),
            &FakeNetwork::default(),
            &rec,
            "node1",
            "",
            NOW
        )
        .await
        .is_err());
    }

    // ── What the page is told ────────────────────────────────────────────────

    #[test]
    fn status_names_the_problems_the_plan_and_the_heal() {
        let s = survey_of(
            &[("node1", true), ("node2", false), ("node3", true)],
            true,
            true,
            2,
        );
        let v = status_json(&s, Some(&heal_at(Step::Wipe)));
        assert_eq!(v["problems"], json!(["machines_gone", "data_unreachable"]));
        assert_eq!(v["refusal"], Value::Null);
        assert_eq!(
            v["plan"],
            json!({"remove_machines": ["node2"], "restart_machines": ["node3"], "reset_kubernetes": false})
        );
        assert_eq!(
            v["survey"]["machines"][1],
            json!({"name": "node2", "addr": "fd00::2", "this_machine": false, "answers": false})
        );
        assert_eq!(v["heal"]["step"], "wipe");
        assert_eq!(
            v["heal"]["steps"],
            json!(["claim", "consensus", "wipe", "restart", "finish"])
        );
        assert_eq!(v["heal"]["running"], true);
        assert_eq!(status_json(&s, None)["heal"], Value::Null);
    }

    #[test]
    fn a_request_is_read_from_the_json_the_page_sends() {
        let r: HealRequest = serde_json::from_str(r#"{"remove_machines": ["node2"]}"#).unwrap();
        assert_eq!(r.remove_machines, names(&["node2"]));
        assert!(
            serde_json::from_str::<HealRequest>("{}").is_err(),
            "the confirmation is explicit"
        );
    }

    // ── Starting again, and old records ──────────────────────────────────────

    #[tokio::test]
    async fn a_heal_stuck_after_the_restart_can_be_started_again() {
        let (_d, rec) = local();
        rec.save(&heal_at(Step::Finish)).await.unwrap();
        let host = survey_host("leader", &["node1", "node2"], true)
            .fail(HEAL_GET, NO_KEY)
            .ok(HEAL_SET, "");
        let heal = start_heal(
            &host,
            &FakeNetwork::default(),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h2".into(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(heal.id, "h2");
        assert_eq!(rec.load().await.unwrap().unwrap().id, "h2");
    }

    #[tokio::test]
    async fn a_heal_whose_driver_is_gone_does_not_block_the_next_one() {
        let (_d, rec) = local();
        let mut orphaned = heal_at(Step::Wipe);
        orphaned.driver = "node2".into();
        let host = survey_host("leader", &["node1", "node2", "node3"], true)
            .ok(HEAL_GET, &serde_json::to_string(&orphaned).unwrap())
            .ok(HEAL_SET, "");
        start_heal(
            &host,
            &FakeNetwork::answering(&["node3"]),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h2".into(),
            NOW,
        )
        .await
        .unwrap();
        assert!(host.ran(HEAL_SET));

        let (_d, rec) = local();
        let mut mine_but_lost = heal_at(Step::Wipe);
        mine_but_lost.driver = "node1".into();
        let host = survey_host("leader", &["node1", "node2", "node3"], true)
            .ok(HEAL_GET, &serde_json::to_string(&mine_but_lost).unwrap())
            .ok(HEAL_SET, "");
        start_heal(
            &host,
            &FakeNetwork::answering(&["node3"]),
            &rec,
            "node1",
            &request(&["node2"]),
            3600,
            "h2".into(),
            NOW,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_old_record_never_overwrites_a_newer_heal_in_ceph() {
        let (_d, rec) = local();
        let mut old = heal_at(Step::Finish);
        old.finished_at = Some(NOW);
        rec.save(&old).await.unwrap();
        let mut newer = heal_at(Step::Wipe);
        newer.id = "h9".into();
        newer.started_at = NOW + 100;
        let host = FakeHost::new()
            .ok(HEAL_GET, &serde_json::to_string(&newer).unwrap())
            .ok(HEAL_SET, "");
        tick(&host, &FakeNetwork::default(), &rec, "node1", "boot-a", NOW)
            .await
            .unwrap();
        assert!(!host.ran(HEAL_SET));
    }

    #[test]
    fn the_page_shows_whichever_heal_started_last() {
        let mut old = heal_at(Step::Finish);
        old.finished_at = Some(NOW);
        let mut newer = heal_at(Step::Wipe);
        newer.id = "h9".into();
        newer.started_at = NOW + 100;
        assert_eq!(
            newest(Some(old.clone()), Some(newer.clone())).unwrap().id,
            "h9"
        );
        assert_eq!(
            newest(Some(newer.clone()), Some(old.clone())).unwrap().id,
            "h9"
        );
        assert_eq!(newest(None, Some(old.clone())).unwrap().id, "h1");
        assert_eq!(newest(Some(old), None).unwrap().id, "h1");
        assert_eq!(newest(None, None), None);
    }
}
