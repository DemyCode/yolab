//! FORCE HEAL: a fresh cluster from the machines that still answer.
//!
//! NOTHING HEALS ITSELF. An offline machine looks exactly like a departed one,
//! and a disk being moved looks like a dead one, so only a person decides. The
//! page shows what is wrong — machines that do not answer, Ceph without a
//! quorum, Kubernetes not answering, data with no reachable copy — and FORCE
//! HEAL is offered whenever any of it is true.
//!
//! NO REPAIR, A REINSTALL. However the cluster broke, it is not taken apart
//! piece by piece (monmaps edited, etcd members removed, OSDs purged): every
//! machine that answers is installed again as a new cluster, and everything
//! else is left behind. The machine the owner clicked creates it; the others
//! join it, exactly as they would have joined at install time. Afterwards:
//!
//!   - every machine's disks are empty; only its system disk is in use, the
//!     others show up OFF on the Storage page, to be switched on again,
//!   - there are no apps: they come back through "Add from backup",
//!   - the backup credentials are put back (`credentials`),
//!   - the storage policy is the default one again.
//!
//! WHICH MACHINES. The YoLab platform's list of this account's machines, this
//! machine's Ceph monmap and Kubernetes' nodes, together — any one of them may
//! be unreadable in the situation a heal is for. Every machine listed is asked
//! directly, on its API, whether it is there. One that answers is kept; one that
//! does not is left out of the new cluster and removed from the platform.
//!
//!   1. `prepare`: every machine that is kept, the creator included, writes
//!      the new cluster into its config.toml and runs `nixos-rebuild boot`
//!      (`member`). Nothing is wiped.
//!   2. `arm`: once every machine is prepared, each sets `[node]
//!      wipe_condition = true`, the creator last.
//!   3. `restart`: every machine restarts at the same time. At boot each wipes
//!      its Ceph and Kubernetes state (`storage::reset_wipe`), clears the flag,
//!      and creates or joins the cluster — a machine that joins retries until
//!      the creator is up, as at install time.
//!   4. `rebuild`: wait for every machine to be in the new Kubernetes cluster,
//!      then remove the ones left behind from the platform.
//!
//! A heal that fails before the restart is undone on every machine (`undo`),
//! which is then exactly as it was.
//!
//! THE MACHINE YOU CLICK DRIVES IT, from a record in a file on its own disk
//! (`/var/lib/yolab/heal.json`) that survives its restart. Nothing about a heal
//! is kept in Kubernetes or Ceph: the heal replaces both.

pub(crate) mod credentials;
pub(crate) mod member;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ceph::destructive::APP_DATA_POOLS;
use crate::ceph::model::{self, PgsByPool};
use crate::error::Outcome;
use crate::host::{Host, RealHost};
use crate::runtime::{Controller, Ctx, Scope, Tick};
use crate::AppState;
use member::{Begin, Layout, PhaseView, PrepareRequest, Preparing, ResetView};

const NAME: &str = "heal";
const TICK: Duration = Duration::from_secs(10);
/// Kubernetes not answering is normal for a few minutes after a boot — k3s waits
/// for the image store — so it is only called a problem after this much uptime.
const KUBERNETES_GRACE_SECS: u64 = 600;
/// How long the machines may take to prepare, all together. Past it the heal is
/// undone rather than left waiting on a machine that will never finish.
const PREPARE_WAIT_SECS: u64 = 3 * 3600;

// ── The record ────────────────────────────────────────────────────────────────

/// A machine in the new cluster.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// Its cluster address: where its API, mon and k3s listen.
    pub addr: String,
}

/// A machine left behind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Gone {
    /// Its name when any list knew it, its address otherwise.
    pub label: String,
    pub addr: String,
    /// Its registration on the YoLab platform, removed at the end.
    pub platform_id: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Step {
    Prepare,
    Arm,
    Restart,
    Rebuild,
    /// Only after a failure: every machine back as it was.
    Undo,
}

impl Step {
    /// The steps of a heal that succeeds, in order.
    const PATH: [Step; 4] = [Step::Prepare, Step::Arm, Step::Restart, Step::Rebuild];

    fn next(self) -> Option<Step> {
        let i = Self::PATH.iter().position(|s| *s == self)?;
        Self::PATH.get(i + 1).copied()
    }

    /// Whether a new heal may replace one at this step. Nothing is half-done on
    /// any machine: the restart is behind it, or everything is being put back.
    fn replaceable(self) -> bool {
        matches!(self, Step::Rebuild | Step::Undo)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Heal {
    id: String,
    /// The machine carrying it out, which creates the new cluster.
    driver: String,
    started_at: u64,
    finished_at: Option<u64>,
    step: Step,
    /// The new cluster's Ceph fsid.
    fsid: String,
    /// Every machine of the new cluster, the driver last.
    members: Vec<Member>,
    gone: Vec<Gone>,
    /// The boot the driver restarted from, so the next boot knows it happened.
    restart_boot_id: Option<String>,
    /// What the current step is waiting for, or why it failed last.
    waiting: Option<String>,
    /// Why the heal was abandoned, when it was.
    failed: Option<String>,
}

impl Heal {
    fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    fn driver_member(&self) -> Result<&Member> {
        self.members
            .iter()
            .find(|m| m.name == self.driver)
            .context("the heal's record does not list the machine driving it")
    }

    fn request_for(&self, member: &Member) -> Result<PrepareRequest> {
        let server_addr = if member.name == self.driver {
            String::new()
        } else {
            format!("https://[{}]:6443", self.driver_member()?.addr)
        };
        Ok(PrepareRequest {
            heal_id: self.id.clone(),
            driver: self.driver.clone(),
            fsid: self.fsid.clone(),
            server_addr,
        })
    }
}

/// The driving machine's record.
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

    fn load(&self) -> Result<Option<Heal>> {
        match std::fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .with_context(|| format!("{} is unreadable", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    fn save(&self, heal: &Heal) -> Result<()> {
        crate::config::write_private_file(&self.path, &serde_json::to_vec_pretty(heal)?)
    }
}

/// Why a backup must not run right now, if it must not: app data has no
/// reachable copy, and the backup would upload the damage as the newest copy.
/// Not being able to tell is a reason too.
pub(crate) async fn backups_blocked() -> Option<String> {
    let host = RealHost;
    let lost = async {
        let dump = host.osd_dump().await?;
        let pgs = host.pgs_brief().await?;
        anyhow::Ok(model::lost_pgs(&dump, &pgs).0)
    };
    match lost.await {
        Ok(lost) => blocked_by_loss(&lost).map(str::to_string),
        Err(e) => Some(format!("cannot tell whether storage is healthy ({e:#})")),
    }
}

fn blocked_by_loss(lost: &PgsByPool) -> Option<&'static str> {
    lost.keys()
        .any(|pool| APP_DATA_POOLS.contains(&pool.as_str()))
        .then_some("a disk holding app data does not answer — reconnect it, or use FORCE HEAL")
}

// ── Reaching other machines ───────────────────────────────────────────────────

/// What a machine says about itself on `GET /api/heal/peer`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct PeerInfo {
    pub name: String,
    pub addr: String,
    pub reset: Option<ResetView>,
}

/// A machine's registration on the YoLab platform.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct PlatformNode {
    pub node_id: i64,
    pub sub_ipv6: String,
}

/// Everything a heal says to other machines and to the platform. A seam, so the
/// survey and the steps can be tested.
pub(crate) trait Network: Send + Sync {
    fn peer<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<PeerInfo>> + Send + 'a;
    fn prepare<'a>(
        &'a self,
        addr: &'a str,
        request: &'a PrepareRequest,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
    fn arm<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
    fn undo<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
    fn reboot<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<()>> + Send + 'a;
    /// `None` when this machine is not connected to the platform.
    fn platform_nodes(&self)
        -> impl Future<Output = Result<Option<Vec<PlatformNode>>>> + Send + '_;
    fn delete_platform_node(&self, id: i64) -> impl Future<Output = Result<()>> + Send + '_;
}

pub(crate) struct RealNetwork {
    client: reqwest::Client,
    port: u16,
    token: String,
    platform_url: String,
}

impl RealNetwork {
    fn from_config(cfg: &crate::config::Config) -> Self {
        let platform_url = std::fs::read_to_string(&cfg.config_path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|t| {
                t.get("tunnel")?
                    .get("platform_api_url")?
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_default();
        Self {
            client: reqwest::Client::new(),
            port: cfg.port,
            token: cfg.cluster_token(),
            platform_url: platform_url.trim_end_matches('/').to_string(),
        }
    }

    fn url(&self, addr: &str, path: &str) -> String {
        format!("http://[{addr}]:{}{path}", self.port)
    }

    /// Sends a node-to-node request; a refusal comes back as its `error`.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        let response = request
            .header(crate::auth::CLUSTER_AUTH_HEADER, &self.token)
            .timeout(timeout)
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let reason = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or(body);
        bail!("{status}: {reason}")
    }
}

#[allow(clippy::manual_async_fn)]
impl Network for RealNetwork {
    fn peer<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<PeerInfo>> + Send + 'a {
        async move {
            let request = self.client.get(self.url(addr, "/api/heal/peer"));
            Ok(self
                .send(request, Duration::from_secs(5))
                .await?
                .json()
                .await?)
        }
    }

    fn prepare<'a>(
        &'a self,
        addr: &'a str,
        request: &'a PrepareRequest,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            let r = self
                .client
                .post(self.url(addr, "/api/heal/peer/prepare"))
                .json(request);
            self.send(r, Duration::from_secs(30)).await.map(|_| ())
        }
    }

    fn arm<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            let r = self
                .client
                .post(self.url(addr, "/api/heal/peer/arm"))
                .json(&json!({ "heal_id": heal_id }));
            self.send(r, Duration::from_secs(30)).await.map(|_| ())
        }
    }

    fn undo<'a>(
        &'a self,
        addr: &'a str,
        heal_id: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            let r = self
                .client
                .post(self.url(addr, "/api/heal/peer/undo"))
                .json(&json!({ "heal_id": heal_id }));
            // Undoing rebuilds the boot entry.
            self.send(r, Duration::from_secs(3600)).await.map(|_| ())
        }
    }

    fn reboot<'a>(&'a self, addr: &'a str) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            let r = self.client.post(self.url(addr, "/api/system/reboot"));
            self.send(r, Duration::from_secs(10)).await.map(|_| ())
        }
    }

    fn platform_nodes(
        &self,
    ) -> impl Future<Output = Result<Option<Vec<PlatformNode>>>> + Send + '_ {
        async move {
            if self.platform_url.is_empty() || self.token.is_empty() {
                return Ok(None);
            }
            let nodes = self
                .client
                .get(format!("{}/nodes", self.platform_url))
                .bearer_auth(&self.token)
                .timeout(Duration::from_secs(10))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(Some(nodes))
        }
    }

    fn delete_platform_node(&self, id: i64) -> impl Future<Output = Result<()>> + Send + '_ {
        async move {
            let response = self
                .client
                .delete(format!("{}/nodes/{id}", self.platform_url))
                .bearer_auth(&self.token)
                .timeout(Duration::from_secs(10))
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(());
            }
            response.error_for_status()?;
            Ok(())
        }
    }
}

// ── Looking at the cluster ────────────────────────────────────────────────────

/// This machine's mon, asked over its admin socket — which answers with or
/// without a quorum, and carries the monmap it has.
#[derive(Debug, Clone, PartialEq)]
struct MonStatus {
    in_quorum: bool,
    /// Name and address of every mon.
    mons: Vec<(String, String)>,
}

fn parse_mon_status(raw: &str) -> Result<MonStatus> {
    let v: Value = serde_json::from_str(raw).context("mon_status is not JSON")?;
    let state = v["state"].as_str().context("mon_status has no state")?;
    let mons = v["monmap"]["mons"]
        .as_array()
        .context("mon_status has no monmap")?
        .iter()
        .map(|m| {
            let name = m["name"].as_str().context("a mon without a name")?;
            let addr = m["public_addrs"]["addrvec"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["addr"].as_str())
                .and_then(host_of)
                .with_context(|| format!("mon {name} has no address"))?;
            Ok((name.to_string(), addr))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MonStatus {
        in_quorum: matches!(state, "leader" | "peon"),
        mons,
    })
}

/// `[fd00::1]:3300` → `fd00::1`; `10.0.0.1:3300` → `10.0.0.1`.
fn host_of(addr: &str) -> Option<String> {
    let (host, _port) = addr.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| host.to_string())
}

/// One spelling per address, so the same machine from two lists is one machine:
/// `fd00:0::1/128` and `fd00::1` are the same.
fn normalize(addr: &str) -> String {
    let bare = addr.split('/').next().unwrap_or(addr).trim();
    bare.parse::<Ipv6Addr>()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bare.to_string())
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

/// Name and cluster address of every Kubernetes node.
async fn kubernetes_nodes<H: Host>(host: &H) -> Result<Vec<(String, Option<String>)>> {
    let v = host
        .kubectl_json(&["get", "nodes", "-o", "json", "--request-timeout=10s"])
        .await?;
    let items = v["items"]
        .as_array()
        .context("kubectl get nodes: no items list")?;
    Ok(items
        .iter()
        .filter_map(|n| {
            let name = n["metadata"]["name"].as_str()?.to_string();
            let addr = n["status"]["addresses"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| {
                    a["type"] == "InternalIP"
                        && a["address"].as_str().is_some_and(|s| s.contains(':'))
                })
                .and_then(|a| a["address"].as_str())
                .map(str::to_string);
            Some((name, addr))
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MachineState {
    /// What the machine calls itself when it answers; otherwise what a list
    /// called it, if any did.
    name: Option<String>,
    addr: String,
    platform_id: Option<i64>,
    this_machine: bool,
    answers: bool,
    /// Its part in a heal, when it answers and has one.
    reset: Option<ResetView>,
}

impl MachineState {
    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.addr.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Survey {
    me: String,
    machines: Vec<MachineState>,
    /// The lists of machines that could not be read, and why.
    unreadable: Vec<String>,
    /// Whether any list of machines could be read.
    listed: bool,
    ceph_quorum: bool,
    kubernetes: bool,
    uptime_secs: u64,
    /// Known only while Ceph has a quorum.
    lost_groups: Option<usize>,
}

impl Survey {
    fn kept(&self) -> impl Iterator<Item = &MachineState> {
        self.machines.iter().filter(|m| m.answers)
    }

    fn gone(&self) -> impl Iterator<Item = &MachineState> {
        self.machines.iter().filter(|m| !m.answers)
    }

    fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.gone().next().is_some() {
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

    fn refusal(&self, local: Option<&Heal>) -> Option<String> {
        if !self.listed {
            return Some(format!(
                "no list of this cluster's machines can be read ({}) — a heal cannot tell which machines to keep",
                self.unreadable.join("; ")
            ));
        }
        let running = local.filter(|h| h.running());
        if let Some(heal) = running.filter(|h| !h.step.replaceable()) {
            return Some(format!(
                "this machine is already healing the cluster (heal {})",
                heal.id
            ));
        }
        let answering: BTreeSet<String> = self.kept().filter_map(|m| m.name.clone()).collect();
        for m in self.kept() {
            let Some(reset) = m.reset.as_ref().filter(|r| r.phase.active()) else {
                continue;
            };
            let ours = running.is_some_and(|h| h.id == reset.heal_id);
            if !ours && reset.driver != self.me && answering.contains(&reset.driver) {
                return Some(format!("{} is already healing the cluster", reset.driver));
            }
        }
        if self.problems().is_empty() {
            return Some("nothing is wrong — there is nothing to heal".into());
        }
        None
    }
}

async fn survey<H: Host, N: Network>(
    host: &H,
    net: &N,
    me: &str,
    my_addr: &str,
    uptime_secs: u64,
) -> Survey {
    #[derive(Default)]
    struct Listed {
        name: Option<String>,
        platform_id: Option<i64>,
    }
    let my_addr = normalize(my_addr);
    let mut listed: BTreeMap<String, Listed> = BTreeMap::new();
    listed.entry(my_addr.clone()).or_default().name = Some(me.to_string());
    let mut unreadable = Vec::new();
    let mut any_list = false;

    match net.platform_nodes().await {
        Ok(Some(nodes)) => {
            any_list = true;
            for n in nodes {
                listed
                    .entry(normalize(&n.sub_ipv6))
                    .or_default()
                    .platform_id = Some(n.node_id);
            }
        }
        Ok(None) => {}
        Err(e) => unreadable.push(format!("the YoLab platform: {e:#}")),
    }
    let mon = mon_status(host, me).await;
    match &mon {
        Ok(status) => {
            any_list = true;
            for (name, addr) in &status.mons {
                let entry = listed.entry(normalize(addr)).or_default();
                if entry.name.is_none() {
                    entry.name = Some(name.clone());
                }
            }
        }
        Err(e) => unreadable.push(format!("Ceph's monmap: {e:#}")),
    }
    let kubernetes = kubernetes_answers(host).await;
    if kubernetes {
        match kubernetes_nodes(host).await {
            Ok(nodes) => {
                any_list = true;
                for (name, addr) in nodes {
                    if let Some(addr) = addr {
                        let entry = listed.entry(normalize(&addr)).or_default();
                        if entry.name.is_none() {
                            entry.name = Some(name);
                        }
                    }
                }
            }
            Err(e) => unreadable.push(format!("Kubernetes' nodes: {e:#}")),
        }
    }

    let probes = listed.keys().map(|addr| net.peer(addr));
    let answers = futures::future::join_all(probes).await;
    let machines = listed
        .into_iter()
        .zip(answers)
        .map(|((addr, l), answer)| {
            let this_machine = addr == my_addr;
            match answer {
                Ok(info) => MachineState {
                    name: Some(info.name),
                    addr,
                    platform_id: l.platform_id,
                    this_machine,
                    answers: true,
                    reset: info.reset,
                },
                Err(e) => {
                    if !this_machine {
                        tracing::debug!("heal: {addr} does not answer: {e:#}");
                    }
                    MachineState {
                        name: l.name,
                        addr,
                        platform_id: l.platform_id,
                        this_machine,
                        // This machine is serving the request that asks.
                        answers: this_machine,
                        reset: None,
                    }
                }
            }
        })
        .collect();

    let ceph_quorum = mon.as_ref().is_ok_and(|m| m.in_quorum);
    let lost_groups = if ceph_quorum {
        match (host.osd_dump().await, host.pgs_brief().await) {
            (Ok(dump), Ok(pgs)) => Some(
                model::lost_pgs(&dump, &pgs)
                    .0
                    .values()
                    .map(BTreeSet::len)
                    .sum(),
            ),
            _ => None,
        }
    } else {
        None
    };
    Survey {
        me: me.to_string(),
        machines,
        unreadable,
        listed: any_list,
        ceph_quorum,
        kubernetes,
        uptime_secs,
        lost_groups,
    }
}

// ── Starting ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct HealRequest {
    /// The machines of the new cluster, as the owner saw them.
    pub keep_machines: BTreeSet<String>,
    /// The machines left behind, confirmed by the owner. Both must be exactly
    /// the survey's now: anything else means the page is out of date.
    pub remove_machines: BTreeSet<String>,
}

#[allow(clippy::too_many_arguments)]
async fn start_heal<H: Host, N: Network>(
    host: &H,
    net: &N,
    local: &LocalRecord,
    me: &str,
    my_addr: &str,
    request: &HealRequest,
    uptime_secs: u64,
    id: String,
    now: u64,
) -> Result<Heal> {
    let previous = local.load()?;
    let s = survey(host, net, me, my_addr, uptime_secs).await;
    if let Some(why) = s.refusal(previous.as_ref()) {
        bail!("{why}");
    }
    let keep: BTreeSet<String> = s.kept().map(MachineState::label).collect();
    let remove: BTreeSet<String> = s.gone().map(MachineState::label).collect();
    if request.keep_machines != keep || request.remove_machines != remove {
        bail!(
            "the machines changed since the page was loaded (answering: {}; not answering: {}) — review and confirm again",
            list(&keep),
            list(&remove)
        );
    }
    let driver_addr: Ipv6Addr = my_addr
        .parse()
        .with_context(|| format!("this machine's cluster address {my_addr:?} is not IPv6"))?;
    let mut members: Vec<Member> = s
        .kept()
        .filter(|m| !m.this_machine)
        .map(|m| Member {
            name: m.label(),
            addr: m.addr.clone(),
        })
        .collect();
    members.push(Member {
        name: me.to_string(),
        addr: driver_addr.to_string(),
    });
    if let Some(prev) = previous.filter(Heal::running) {
        tracing::warn!(
            "heal {}: replaced by a new heal while at {:?}",
            prev.id,
            prev.step
        );
    }
    let heal = Heal {
        id,
        driver: me.to_string(),
        started_at: now,
        finished_at: None,
        step: Step::Prepare,
        fsid: new_fsid(),
        members,
        gone: s
            .gone()
            .map(|m| Gone {
                label: m.label(),
                addr: m.addr.clone(),
                platform_id: m.platform_id,
            })
            .collect(),
        restart_boot_id: None,
        waiting: None,
        failed: None,
    };
    local.save(&heal)?;
    tracing::warn!(
        "heal {} started by {me}: new cluster of {:?}, leaving out {:?}",
        heal.id,
        heal.members.iter().map(|m| &m.name).collect::<Vec<_>>(),
        heal.gone.iter().map(|g| &g.label).collect::<Vec<_>>(),
    );
    crate::runtime::wake(NAME);
    Ok(heal)
}

fn list(items: &BTreeSet<String>) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// A random (version 4) UUID.
fn new_fsid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ── Running ───────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum StepResult {
    Done,
    /// Look again next tick, for this reason.
    NotYet(String),
    /// Abandon the heal, for this reason, and undo it.
    Fail(String),
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
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let boot_id = boot_id().await?;
        tick(
            &RealHost,
            &RealNetwork::from_config(&self.config),
            &LocalRecord::under(Path::new("/")),
            &boot_id,
            now_secs(),
        )
        .await
    }
}

async fn boot_id() -> Result<String> {
    let id = tokio::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .await
        .context("read this boot's id")?;
    let id = id.trim();
    if id.is_empty() {
        bail!("this boot has no id — a heal cannot tell whether a restart happened");
    }
    Ok(id.to_string())
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
    boot_id: &str,
    now: u64,
) -> Result<Tick> {
    let Some(mut heal) = local.load()? else {
        return Ok(Tick::Idle("no heal is driven from this machine".into()));
    };
    while heal.running() {
        tracing::info!("heal {}: {:?}", heal.id, heal.step);
        match run_step(host, net, local, &mut heal, boot_id, now).await {
            Err(e) => {
                heal.waiting = Some(format!("{e:#}"));
                local.save(&heal)?;
                return Err(e);
            }
            Ok(StepResult::NotYet(why)) => {
                if heal.waiting.as_deref() != Some(why.as_str()) {
                    heal.waiting = Some(why);
                    local.save(&heal)?;
                }
                return Ok(Tick::Done);
            }
            Ok(StepResult::Fail(why)) => {
                tracing::error!("heal {}: {why} — undoing it on every machine", heal.id);
                heal.failed = Some(why);
                heal.step = Step::Undo;
                heal.waiting = None;
                local.save(&heal)?;
            }
            Ok(StepResult::Restarting) => return Ok(Tick::Done),
            Ok(StepResult::Done) => {
                heal.waiting = None;
                match heal.step.next() {
                    Some(next) => heal.step = next,
                    None => {
                        heal.finished_at = Some(now);
                        tracing::warn!(
                            "heal {}: {}",
                            heal.id,
                            if heal.failed.is_some() {
                                "undone"
                            } else {
                                "finished"
                            }
                        );
                    }
                }
                local.save(&heal)?;
            }
        }
    }
    Ok(Tick::Idle(format!("heal {} finished", heal.id)))
}

async fn run_step<H: Host, N: Network>(
    host: &H,
    net: &N,
    local: &LocalRecord,
    heal: &mut Heal,
    boot_id: &str,
    now: u64,
) -> Result<StepResult> {
    use StepResult::*;
    match heal.step {
        Step::Prepare => prepare_step(net, heal, now).await,
        Step::Arm => {
            // The driver last: if another machine cannot be armed, the machine
            // that would create the cluster is not either.
            for m in &heal.members {
                let info = match net.peer(&m.addr).await {
                    Ok(info) => info,
                    Err(e) => return Ok(Fail(format!("{} stopped answering: {e:#}", m.name))),
                };
                match info.reset.filter(|r| r.heal_id == heal.id).map(|r| r.phase) {
                    Some(PhaseView::Armed) => continue,
                    Some(PhaseView::Prepared) => {}
                    other => {
                        return Ok(Fail(format!(
                            "{} is no longer prepared ({})",
                            m.name,
                            other.map_or("it has nothing for this heal".to_string(), |p| {
                                format!("{p:?}").to_lowercase()
                            })
                        )))
                    }
                }
                if let Err(e) = net.arm(&m.addr, &heal.id).await {
                    return Ok(Fail(format!("{} could not be armed: {e:#}", m.name)));
                }
            }
            Ok(Done)
        }
        Step::Restart => {
            // All at once: each machine restarts a few seconds after it answers
            // (routers/reboot.rs), this one straight after asking them.
            for m in heal.members.iter().filter(|m| m.name != heal.driver) {
                // Not fatal: one that does not restart now is asked again after
                // this machine is back, and wipes itself whenever it restarts.
                net.reboot(&m.addr)
                    .await
                    .warn_on_err(format!("heal: restart {}", m.name));
            }
            heal.step = Step::Rebuild;
            heal.restart_boot_id = Some(boot_id.to_string());
            heal.waiting = Some("restarting this machine".into());
            local.save(heal)?;
            restart_this_machine(host).await?;
            Ok(Restarting)
        }
        Step::Rebuild => {
            if heal.restart_boot_id.as_deref() == Some(boot_id) {
                // Saved, but the restart never happened.
                restart_this_machine(host).await?;
                return Ok(Restarting);
            }
            rebuild_step(host, net, heal).await
        }
        Step::Undo => {
            let mut stuck = Vec::new();
            for m in &heal.members {
                if let Err(e) = net.undo(&m.addr, &heal.id).await {
                    stuck.push(format!("{} ({e:#})", m.name));
                }
            }
            Ok(if stuck.is_empty() {
                Done
            } else {
                NotYet(format!(
                    "putting back {} — a machine left armed wipes itself at its next restart",
                    stuck.join(", ")
                ))
            })
        }
    }
}

async fn prepare_step<N: Network>(net: &N, heal: &Heal, now: u64) -> Result<StepResult> {
    let mut waiting = Vec::new();
    for m in &heal.members {
        let info = match net.peer(&m.addr).await {
            Ok(info) => info,
            Err(e) => {
                waiting.push(format!("{} does not answer ({e:#})", m.name));
                continue;
            }
        };
        if info.name != m.name {
            return Ok(StepResult::Fail(format!(
                "{} now answers as {}, not {}",
                m.addr, info.name, m.name
            )));
        }
        let Some(reset) = info.reset.filter(|r| r.heal_id == heal.id) else {
            match net.prepare(&m.addr, &heal.request_for(m)?).await {
                Ok(()) => waiting.push(format!("{} is preparing", m.name)),
                Err(e) => waiting.push(format!("{} did not start preparing ({e:#})", m.name)),
            }
            continue;
        };
        match reset.phase {
            PhaseView::Preparing => waiting.push(format!("{} is preparing", m.name)),
            PhaseView::Prepared | PhaseView::Armed => {}
            PhaseView::Failed => {
                return Ok(StepResult::Fail(format!(
                    "{} could not prepare: {}",
                    m.name,
                    reset.error.unwrap_or_default()
                )))
            }
            PhaseView::Undone | PhaseView::Restarted => {
                return Ok(StepResult::Fail(format!(
                    "{} abandoned its part in the heal",
                    m.name
                )))
            }
        }
    }
    if waiting.is_empty() {
        return Ok(StepResult::Done);
    }
    if now >= heal.started_at + PREPARE_WAIT_SECS {
        return Ok(StepResult::Fail(format!(
            "the machines did not finish preparing in {} hours: {}",
            PREPARE_WAIT_SECS / 3600,
            waiting.join("; ")
        )));
    }
    Ok(StepResult::NotYet(waiting.join("; ")))
}

async fn rebuild_step<H: Host, N: Network>(host: &H, net: &N, heal: &Heal) -> Result<StepResult> {
    let mut waiting = Vec::new();
    for m in heal.members.iter().filter(|m| m.name != heal.driver) {
        let Ok(info) = net.peer(&m.addr).await else {
            continue;
        };
        let pending = info
            .reset
            .is_some_and(|r| r.heal_id == heal.id && r.phase == PhaseView::Armed);
        if pending {
            net.reboot(&m.addr)
                .await
                .warn_on_err(format!("heal: restart {} again", m.name));
            waiting.push(format!("{} has not restarted yet", m.name));
        }
    }
    if !kubernetes_answers(host).await {
        waiting.push("Kubernetes is starting".into());
        return Ok(StepResult::NotYet(waiting.join("; ")));
    }
    let nodes: BTreeSet<String> = kubernetes_nodes(host)
        .await?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for m in heal.members.iter().filter(|m| !nodes.contains(&m.name)) {
        waiting.push(format!("{} has not joined the new cluster yet", m.name));
    }
    if !waiting.is_empty() {
        return Ok(StepResult::NotYet(waiting.join("; ")));
    }
    for g in &heal.gone {
        if let Some(id) = g.platform_id {
            // Best effort: a registration left behind costs nothing but a line
            // in the platform's list.
            net.delete_platform_node(id)
                .await
                .warn_on_err(format!("heal: remove {} from the YoLab platform", g.label));
        }
    }
    Ok(StepResult::Done)
}

async fn restart_this_machine<H: Host>(host: &H) -> Result<()> {
    tracing::warn!("heal: restarting this machine");
    let out = host.systemctl(&["reboot"]).await?;
    if !out.success {
        bail!("systemctl reboot: {}", out.stderr.trim());
    }
    Ok(())
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

fn heal_json(heal: &Heal) -> Value {
    json!({
        "id": heal.id,
        "driver": heal.driver,
        "running": heal.running(),
        "failed": heal.failed,
        "started_at": heal.started_at,
        "finished_at": heal.finished_at,
        "step": heal.step,
        "steps": Step::PATH,
        "members": heal.members.iter().map(|m| &m.name).collect::<Vec<_>>(),
        "removed_machines": heal.gone.iter().map(|g| &g.label).collect::<Vec<_>>(),
        "waiting": heal.waiting,
    })
}

fn status_json(survey: &Survey, heal: Option<&Heal>) -> Value {
    json!({
        "survey": {
            "me": survey.me,
            "machines": survey.machines.iter().map(|m| json!({
                "label": m.label(),
                "name": m.name,
                "addr": m.addr,
                "this_machine": m.this_machine,
                "answers": m.answers,
                "reset": m.reset,
            })).collect::<Vec<_>>(),
            "unreadable": survey.unreadable,
            "ceph_quorum": survey.ceph_quorum,
            "kubernetes": survey.kubernetes,
            "uptime_secs": survey.uptime_secs,
            "lost_groups": survey.lost_groups,
        },
        "problems": survey.problems(),
        "refusal": survey.refusal(heal),
        "plan": {
            "keep_machines": survey.kept().map(MachineState::label).collect::<Vec<_>>(),
            "remove_machines": survey.gone().map(MachineState::label).collect::<Vec<_>>(),
        },
        "heal": heal.map(heal_json),
    })
}

fn error(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": e.to_string() })))
}

async fn uptime_secs() -> u64 {
    tokio::fs::read_to_string("/proc/uptime")
        .await
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// What `GET /api/heal` calls wrong on this machine right now, for
/// notifications (`notify::alerts`).
pub(crate) async fn current_problems(cfg: &crate::config::Config) -> Vec<&'static str> {
    let net = RealNetwork::from_config(cfg);
    let me = crate::system::hostname();
    survey(&RealHost, &net, &me, &cfg.node_ipv6, uptime_secs().await)
        .await
        .problems()
}

/// `GET /api/heal` — what is wrong, what a heal would do, and the heal this
/// machine drives or drove last. Served whether or not Ceph or Kubernetes answer.
pub async fn get_status(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let net = RealNetwork::from_config(&s.config);
    let local = match LocalRecord::under(Path::new("/")).load() {
        Ok(h) => h,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    let me = crate::system::hostname();
    let survey = survey(
        &RealHost,
        &net,
        &me,
        &s.config.node_ipv6,
        uptime_secs().await,
    )
    .await;
    (StatusCode::OK, Json(status_json(&survey, local.as_ref())))
}

/// `POST /api/heal` with a `HealRequest`. This machine drives the heal.
pub async fn post_heal(
    State(s): State<AppState>,
    Json(request): Json<HealRequest>,
) -> (StatusCode, Json<Value>) {
    let net = RealNetwork::from_config(&s.config);
    let me = crate::system::hostname();
    let id = crate::routers::backup_common::random_hex(8);
    match start_heal(
        &RealHost,
        &net,
        &LocalRecord::under(Path::new("/")),
        &me,
        &s.config.node_ipv6,
        &request,
        uptime_secs().await,
        id,
        now_secs(),
    )
    .await
    {
        Ok(heal) => (StatusCode::OK, Json(heal_json(&heal))),
        Err(e) => error(StatusCode::CONFLICT, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct HealIdRequest {
    pub heal_id: String,
}

/// `GET /api/heal/peer` — this machine, as a heal driver sees it.
pub async fn get_peer(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    let boot = match boot_id().await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let layout = Layout::from_config(&s.config);
    match member::current(&layout, &boot, Preparing::global()) {
        Ok(reset) => (
            StatusCode::OK,
            Json(json!(PeerInfo {
                name: crate::system::hostname(),
                addr: normalize(&s.config.node_ipv6),
                reset,
            })),
        ),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `POST /api/heal/peer/prepare` — prepare this machine for a heal's new
/// cluster. Returns as soon as preparing has started.
pub async fn post_peer_prepare(
    State(s): State<AppState>,
    Json(request): Json<PrepareRequest>,
) -> (StatusCode, Json<Value>) {
    let boot = match boot_id().await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let layout = Layout::from_config(&s.config);
    let _held = member::lock().lock().await;
    if let Ok(Some(v)) = member::current(&layout, &boot, Preparing::global()) {
        if v.heal_id == request.heal_id && v.phase != PhaseView::Failed {
            return (StatusCode::OK, Json(json!(v)));
        }
    }
    // Held while preparing: an update rebuilding this machine meanwhile would race it.
    let Some(update) = crate::routers::update::exclusive() else {
        return error(
            StatusCode::CONFLICT,
            "an update is rebuilding this machine's system",
        );
    };
    match member::begin_prepare(&RealHost, &layout, &request, &boot, Preparing::global()).await {
        Ok(Begin::Already(v)) => (StatusCode::OK, Json(json!(v))),
        Ok(Begin::Start) => {
            tokio::spawn(async move {
                let _update = update;
                member::prepare(&RealHost, &layout, &request, Preparing::global()).await;
            });
            (StatusCode::ACCEPTED, Json(json!({ "status": "preparing" })))
        }
        Err(e) => error(StatusCode::CONFLICT, format!("{e:#}")),
    }
}

/// `POST /api/heal/peer/arm` — have this machine's next boot wipe it.
pub async fn post_peer_arm(
    State(s): State<AppState>,
    Json(request): Json<HealIdRequest>,
) -> (StatusCode, Json<Value>) {
    peer_change(
        &s,
        &request.heal_id,
        |layout, boot, id, _may_rebuild| async move { member::arm(&layout, &id, &boot) },
    )
    .await
}

/// `POST /api/heal/peer/undo` — put this machine back as it was.
pub async fn post_peer_undo(
    State(s): State<AppState>,
    Json(request): Json<HealIdRequest>,
) -> (StatusCode, Json<Value>) {
    peer_change(
        &s,
        &request.heal_id,
        |layout, boot, id, may_rebuild| async move {
            member::undo(
                &RealHost,
                &layout,
                &id,
                &boot,
                Preparing::global(),
                may_rebuild,
            )
            .await
        },
    )
    .await
}

async fn peer_change<F, Fut>(s: &AppState, heal_id: &str, change: F) -> (StatusCode, Json<Value>)
where
    F: FnOnce(Layout, String, String, bool) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let boot = match boot_id().await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let layout = Layout::from_config(&s.config);
    let _held = member::lock().lock().await;
    // Held while the boot entry is rebuilt: an update doing the same would race it.
    let update = crate::routers::update::exclusive();
    let may_rebuild = update.is_some();
    match change(layout, boot, heal_id.to_string(), may_rebuild).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(e) => error(StatusCode::CONFLICT, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests;
