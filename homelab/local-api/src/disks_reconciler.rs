use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::{collections::HashMap, io::Read, path::Path, sync::Mutex};
use tokio::time::{sleep, Duration};

use crate::ceph::destructive;
use crate::error::Outcome;
use crate::host::{Host, RealHost};
use crate::storage::settings;

const INTERVAL_SECS: u64 = 60;

const BLUESTORE_MAGIC: &[u8] = b"bluestore block device\n";
const CEPH_FSID_KEY: &[u8] = b"\x09\x00\x00\x00ceph_fsid";

const SYSTEM_OSD_DEV: &str = "/dev/mapper/pool-ceph";
pub(crate) const SYSTEM_OSD_ID: &str = "system";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Unset,
    Active,
    Creating,
    Retrying,
    Blocked,
    Draining,
    Removing,
    Removable,
    Unknown,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Unset => "",
            Phase::Active => "active",
            Phase::Creating => "creating",
            Phase::Retrying => "retrying",
            Phase::Blocked => "blocked",
            Phase::Draining => "draining",
            Phase::Removing => "removing",
            Phase::Removable => "removable",
            Phase::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DiskProgress {
    pub phase: Phase,
    pub message: String,
    pub attempts: u32,
    pub orphan_osd_id: Option<i64>,
    pub last_attempt: Option<std::time::Instant>,
}

static PROGRESS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, DiskProgress>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

static CREATING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    Ours,
    Foreign,
    Blank,
    Unknown,
}

impl Ownership {
    fn read(device_fsid: Option<&str>, our_fsid: &str) -> Self {
        match device_fsid {
            None => Ownership::Blank,
            Some(_) if our_fsid.is_empty() => Ownership::Unknown,
            Some(f) if f == our_fsid => Ownership::Ours,
            Some(_) => Ownership::Foreign,
        }
    }

    fn is_ours(self) -> bool {
        self == Ownership::Ours
    }

    fn is_foreign(self) -> bool {
        matches!(self, Ownership::Foreign | Ownership::Unknown)
    }

    fn as_str(self) -> &'static str {
        match self {
            Ownership::Ours => "ours",
            Ownership::Foreign => "foreign",
            Ownership::Blank => "blank",
            Ownership::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Disk {
    pub device: String,
    pub model: String,
    pub size_bytes: u64,
    pub is_loop: bool,
    pub ownership: Ownership,
    pub has_partitions: bool,
    pub mounted: bool,
    pub osd_id: Option<i64>,
    pub progress: Option<DiskProgress>,
}

impl Disk {
    fn to_value(&self) -> Value {
        let mut v = json!({
            "device": self.device,
            "model": self.model,
            "size_bytes": self.size_bytes,
            "is_our_osd": self.ownership.is_ours(),
            "foreign_ceph": self.ownership.is_foreign(),
            "ownership": self.ownership.as_str(),
            "has_partitions": self.has_partitions,
            "mounted": self.mounted,
            "osd_id": self.osd_id,
        });
        if self.is_loop {
            v["is_loop"] = json!(true);
        }
        if let Some(p) = self.progress.as_ref().filter(|p| p.phase != Phase::Unset) {
            v["phase"] = json!(p.phase.as_str());
            v["message"] = json!(p.message);
            v["attempts"] = json!(p.attempts);
        }
        v
    }

    fn dev_path(&self) -> Option<String> {
        let d = self.device.trim();
        if d.is_empty() {
            return None;
        }
        Some(if d.starts_with('/') {
            d.to_string()
        } else {
            format!("/dev/{d}")
        })
    }
}

fn set_phase(disk_id: &str, phase: Phase, message: impl Into<String>) {
    let Ok(mut p) = PROGRESS.lock() else { return };
    let e = p.entry(disk_id.to_string()).or_default();
    e.phase = phase;
    e.message = message.into();
}

pub(crate) fn stuck_disks() -> Vec<(String, String)> {
    let Ok(progress) = PROGRESS.lock() else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = progress
        .iter()
        .filter(|(_, p)| {
            p.phase == Phase::Blocked || (p.phase == Phase::Retrying && p.attempts >= 3)
        })
        .map(|(disk, p)| (disk.clone(), p.message.clone()))
        .collect();
    out.sort();
    out
}

fn progress_of(disk_id: &str) -> DiskProgress {
    PROGRESS
        .lock()
        .ok()
        .and_then(|p| p.get(disk_id).cloned())
        .unwrap_or_default()
}

fn mark_progress(meta: &mut HashMap<String, Disk>) {
    for (disk_id, d) in meta.iter_mut() {
        d.progress = Some(progress_of(disk_id));
    }
}

fn system_osd_present() -> bool {
    Path::new(SYSTEM_OSD_DEV).exists()
}

fn lv_size_bytes(dev: &str) -> u64 {
    std::fs::read_link(dev)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .and_then(|dm| std::fs::read_to_string(format!("/sys/block/{dm}/size")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|sectors| sectors * 512)
        .unwrap_or(0)
}

fn system_osd_meta(our_fsid: &str) -> Disk {
    lv_osd_meta(SYSTEM_OSD_DEV, our_fsid)
}

fn lv_osd_meta(dev: &str, our_fsid: &str) -> Disk {
    Disk {
        device: dev.to_string(),
        model: "System disk".to_string(),
        size_bytes: lv_size_bytes(dev),
        is_loop: true,
        ownership: Ownership::read(bluestore_fsid(dev).as_deref(), our_fsid),
        has_partitions: false,
        mounted: false,
        osd_id: None,
        progress: None,
    }
}

pub(crate) async fn system_osd_attempt<H: Host>(
    host: &H,
) -> Result<crate::storage::wait::Attempt<()>> {
    lv_osd_attempt(host, SYSTEM_OSD_DEV).await
}

async fn lv_osd_attempt<H: Host>(host: &H, dev: &str) -> Result<crate::storage::wait::Attempt<()>> {
    use crate::storage::wait::Attempt;

    if !Path::new(dev).exists() {
        anyhow::bail!(
            "{dev} does not exist — this machine was not installed with the \
             YoLab disk layout, so it has nowhere to keep its image store"
        );
    }
    let our_fsid = match host.cluster_fsid().await {
        Ok(fsid) => fsid,
        Err(e) => {
            return Ok(Attempt::NotYet(format!(
                "cannot read this cluster's id yet ({e})"
            )))
        }
    };
    let system = canonical_device(dev);
    let find = |local: &[(String, i64)]| {
        local
            .iter()
            .find(|(path, _)| canonical_device(path) == system)
            .map(|(_, id)| *id)
    };

    if let Some(id) = find(&local_osds(host).await?) {
        match host.osd_ids().await {
            Err(e) => {
                return Ok(Attempt::NotYet(format!(
                    "cannot tell whether osd.{id} on {dev} still exists ({e})"
                )))
            }
            Ok(ids) if ids.contains(&id) => {
                start_osd_unit(host, id).await;
                return Ok(Attempt::Ready(()));
            }
            Ok(_) => {
                tracing::warn!(
                    "{dev}: carries osd.{id}, which this cluster no longer has — erasing it for a new one"
                );
                destructive::zap(
                    host,
                    dev,
                    destructive::ZapWarrant::ForgottenByCluster {
                        osd: id,
                        whole_disk: false,
                    },
                )
                .await?;
            }
        }
    }
    if let Some(reason) = refuse_osd_creation(&lv_osd_meta(dev, &our_fsid)) {
        return Ok(Attempt::NotYet(format!("{dev}: {reason}")));
    }
    create_osd(host, SYSTEM_OSD_ID, dev).await;
    Ok(match find(&local_osds(host).await?) {
        Some(_) => Attempt::Ready(()),
        None => Attempt::NotYet(format!(
            "creating the OSD on {dev} did not succeed (the reason is logged above)"
        )),
    })
}

pub struct DisksController;

impl crate::runtime::Controller for DisksController {
    fn name(&self) -> &'static str {
        "disks"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(INTERVAL_SECS)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::Ceph]
    }
    fn pauses_during(&self) -> &'static [crate::runtime::Activity] {
        &[crate::runtime::Activity::Restore]
    }
    async fn reconcile(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        if ctx.node.is_empty() {
            anyhow::bail!("cannot determine this node's name");
        }
        publish_local(&RealHost, &ctx.node).await?;
        Ok(crate::runtime::Tick::Done)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OsdMapSource {
    CephVolume,
    Mon,
}

async fn fetch_disk_to_osd<H: Host>(
    host: &H,
    _node: &str,
    meta: &HashMap<String, Disk>,
) -> Option<(HashMap<String, i64>, OsdMapSource)> {
    let mut device_to_disk_id: HashMap<String, String> = HashMap::new();
    for (disk_id, m) in meta {
        let dev = m.device.as_str();
        if dev.is_empty() {
            continue;
        }
        device_to_disk_id.insert(canonical_device(dev), disk_id.clone());
    }

    let (local, source) = match local_osds(host).await {
        Ok(v) => (v, OsdMapSource::CephVolume),
        Err(e) => {
            tracing::warn!("fetch_disk_to_osd: ceph-volume failed ({e}) — asking the mon instead");
            match host.ceph_json(&["osd", "metadata"]).await {
                Ok(v) => {
                    let from_mon = parse_osd_metadata(&v, _node);
                    if from_mon.is_empty() {
                        tracing::warn!(
                            "fetch_disk_to_osd: the mon reported no OSDs for this host either — \
                             treating the map as UNKNOWN, not empty"
                        );
                        return None;
                    }
                    tracing::info!(
                        "fetch_disk_to_osd: recovered {} OSD(s) from the mon",
                        from_mon.len()
                    );
                    (from_mon, OsdMapSource::Mon)
                }
                Err(e2) => {
                    tracing::warn!(
                        "fetch_disk_to_osd: the mon could not answer either ({e2}) — treating the \
                         OSD map as UNKNOWN, not empty"
                    );
                    return None;
                }
            }
        }
    };

    let mut result = HashMap::new();
    for (dev_path, osd_id) in local {
        let key = canonical_device(&dev_path);
        if let Some(disk_id) = device_to_disk_id.get(&key) {
            result.insert(disk_id.clone(), osd_id);
        }
    }
    Some((result, source))
}

pub(crate) fn parse_osd_metadata(raw: &Value, host: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Some(items) = raw.as_array() else {
        return out;
    };
    for m in items {
        if m["hostname"].as_str() != Some(host) {
            continue;
        }
        let Some(id) = m["id"].as_i64() else { continue };
        if let Some(node) = m["bluestore_bdev_dev_node"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "unknown")
        {
            out.push((node.to_string(), id));
        }
        if let Some(devs) = m["devices"].as_str() {
            for d in devs.split(',').map(str::trim).filter(|d| !d.is_empty()) {
                out.push((d.to_string(), id));
            }
        }
    }
    out
}

fn canonical_device(dev: &str) -> String {
    let full = if dev.starts_with('/') {
        dev.to_string()
    } else {
        format!("/dev/{dev}")
    };
    std::fs::canonicalize(&full)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(full)
}

async fn local_osds<H: Host>(host: &H) -> Result<Vec<(String, i64)>> {
    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await?;
    let our_fsid = host.cluster_fsid().await.unwrap_or_default();
    parse_lvm_list(&raw, &our_fsid)
}

async fn publish_local<H: Host + 'static>(host: &H, node: &str) -> Result<()> {
    let Some(scanned) = scan_devices(host).await else {
        anyhow::bail!("could not read the disk list from lsblk — skipping this tick");
    };
    let our_fsid = host.cluster_fsid().await.unwrap_or_default();

    let mut meta: HashMap<String, Disk> = scanned
        .iter()
        .map(|(d, flags)| (disk_id(d), disk_meta(d, &our_fsid, *flags)))
        .collect();
    if system_osd_present() {
        meta.insert(SYSTEM_OSD_ID.to_string(), system_osd_meta(&our_fsid));
    }

    let fetched = if our_fsid.is_empty() {
        None
    } else {
        fetch_disk_to_osd(host, node, &meta).await
    };
    let can_create = matches!(fetched, Some((_, OsdMapSource::CephVolume)));
    let disk_to_osd: Option<HashMap<String, i64>> = fetched.map(|(map, _)| map);
    if let Some(map) = &disk_to_osd {
        mark_known_osds(&mut meta, map);
    }

    let desired = read_desired(host).await;

    match &desired {
        Some(d) => {
            let registered = auto_register_all_disks(host, node, &meta, d).await;
            let d = if registered > 0 {
                read_desired(host).await.unwrap_or_else(|| d.clone())
            } else {
                d.clone()
            };
            reconcile_local_osds(host, node, &meta, &d, disk_to_osd.as_ref(), can_create).await;
        }
        None => {
            tracing::warn!(
                "disk reconciler: cannot read which disks are switched on — making no changes"
            );
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    Phase::Unknown,
                    "Cannot reach this machine's settings right now. Nothing will be changed until it can.",
                );
            }
        }
    }

    mark_progress(&mut meta);
    write_status(host, node, &meta).await;
    Ok(())
}

pub(crate) fn drain_targets_remaining(
    crush_nodes: &[Value],
    leaving: i64,
    failure_domain: &str,
) -> usize {
    let usable: Vec<i64> = crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd"))
        .filter(|n| n["status"].as_str() == Some("up"))
        .filter(|n| n["reweight"].as_f64().unwrap_or(0.0) > 0.5)
        .filter_map(|n| n["id"].as_i64())
        .filter(|id| *id != leaving)
        .collect();

    if failure_domain != "host" {
        return usable.len();
    }

    crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("host"))
        .filter(|h| {
            h["children"].as_array().is_some_and(|c| {
                c.iter()
                    .filter_map(|x| x.as_i64())
                    .any(|id| usable.contains(&id))
            })
        })
        .count()
}

fn drain_message(targets: usize, size: Option<u32>) -> String {
    let Some(size) = size else {
        return "Moving this disk's files onto the others. Do not unplug it while this \
                is happening."
            .to_string();
    };

    if targets == 0 {
        return "Waiting to move this disk's files somewhere else — but there is no other \
                disk running to move them to. Switch on another disk, or add one, and this \
                will finish on its own."
            .to_string();
    }

    if targets < size as usize {
        return format!(
            "This disk cannot be emptied yet. You have asked for {size} copies of \
             everything, and taking this disk out leaves only {targets} other \
             place{plural} to keep them — so there is nowhere for its files to go. \
             Lower the number of copies to {targets}, or add another disk, and this \
             finishes on its own.",
            plural = if targets == 1 { "" } else { "s" }
        );
    }

    "Moving this disk's files onto the others. Do not unplug it until this finishes.".to_string()
}

#[derive(Debug, PartialEq)]
struct Steer {
    set_weight: Option<f64>,
    mark_in: bool,
    mark_out: bool,
    needs_purge: bool,
    phase: Option<Phase>,
    message: Option<String>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
struct OsdState {
    crush_weight: f64,
    reweight: f64,
    kb: u64,
    up: bool,
}

fn decide_steer(
    want_on: bool,
    size_bytes: u64,
    state: OsdState,
    osd_id: i64,
    crush_nodes: &[Value],
    failure_domain: &str,
    want_copies: Option<u32>,
) -> Steer {
    if want_on {
        let set_weight = if state.crush_weight == 0.0 {
            let weight = weight_tib_from(state.kb, size_bytes);
            (weight > 0.0).then_some(weight)
        } else {
            None
        };
        let (phase, message) = if state.up {
            (Phase::Active, "In use, storing your files.".to_string())
        } else {
            (
                Phase::Retrying,
                "This disk is switched on but is not currently serving data. \
                 YoLab keeps trying to bring it back."
                    .to_string(),
            )
        };
        return Steer {
            set_weight,
            mark_in: state.reweight < 0.5,
            mark_out: false,
            needs_purge: false,
            phase: Some(phase),
            message: Some(message),
        };
    }

    if state.reweight > 0.5 {
        let targets = drain_targets_remaining(crush_nodes, osd_id, failure_domain);
        return Steer {
            set_weight: None,
            mark_in: false,
            mark_out: true,
            needs_purge: false,
            phase: Some(Phase::Draining),
            message: Some(drain_message(targets, want_copies)),
        };
    }

    Steer {
        set_weight: None,
        mark_in: false,
        mark_out: false,
        needs_purge: true,
        phase: None,
        message: None,
    }
}

fn steer_report(steer: &Steer, applied: bool) -> (Phase, String) {
    if let (true, Some(phase), Some(message)) = (applied, steer.phase, steer.message.as_ref()) {
        (phase, message.clone())
    } else {
        (
            Phase::Retrying,
            "YoLab could not finish this step and will keep trying.".to_string(),
        )
    }
}

async fn reconcile_local_osds<H: Host + 'static>(
    host: &H,
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
    disk_to_osd: Option<&HashMap<String, i64>>,
    can_create: bool,
) {
    match plan_tick(host.reachable().await, disk_to_osd.is_some()) {
        TickPlan::Unreachable => {
            tracing::debug!("reconcile_local_osds: ceph unreachable, skipping this tick");
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    Phase::Unknown,
                    "Waiting for the storage cluster to answer.",
                );
            }
            return;
        }
        TickPlan::UnknownOsdMap => {
            tracing::warn!(
                "reconcile_local_osds: the local OSD map is unknown this tick — making no changes"
            );
            for disk_id in meta.keys() {
                set_phase(
                    disk_id,
                    Phase::Unknown,
                    "Cannot read this machine's disk setup right now. Nothing will be changed until it can.",
                );
            }
            return;
        }
        TickPlan::Proceed => {}
    }
    let Some(disk_to_osd) = disk_to_osd else {
        return;
    };
    let forgotten = erase_forgotten_osds(host, node, meta, desired, disk_to_osd).await;
    let live: HashMap<String, i64> = disk_to_osd
        .iter()
        .filter(|(disk_id, _)| !forgotten.contains(*disk_id))
        .map(|(d, id)| (d.clone(), *id))
        .collect();
    let disk_to_osd = &live;

    if !can_create {
        tracing::info!(
            "reconcile_local_osds: this tick's OSD list is the mon's, which omits OSDs that never \
             booted — creating nothing until ceph-volume answers"
        );
    }
    for (disk_id, m) in meta.iter().filter(|_| can_create) {
        if forgotten.contains(disk_id) {
            continue;
        }
        let creating = CREATING
            .lock()
            .map(|c| c.contains(disk_id))
            .unwrap_or(false);
        let (attempts, since_last) = PROGRESS
            .lock()
            .ok()
            .and_then(|p| p.get(disk_id).cloned())
            .map(|p| (p.attempts, p.last_attempt.map(|t| t.elapsed())))
            .unwrap_or((0, None));
        match plan_create(
            node,
            disk_id,
            m,
            desired,
            disk_to_osd,
            creating,
            attempts,
            since_last,
        ) {
            CreatePlan::Skip => continue,
            CreatePlan::Waiting => continue,
            CreatePlan::Blocked(reason) => {
                tracing::warn!("{disk_id}: desired ON but not creating an OSD — {reason}");
                set_phase(disk_id, Phase::Blocked, reason);
            }
            CreatePlan::Create { dev_path } => {
                spawn_create(host.clone(), disk_id.clone(), dev_path)
            }
        }
    }

    for disk_id in meta.keys() {
        let want_on = wants_on(desired, node, disk_id);
        if !want_on {
            continue;
        }
        if let Some(&osd_id) = disk_to_osd.get(disk_id) {
            ensure_osd_unit_running(host, osd_id).await;
        }
    }

    stop_foreign_osd_units(host).await;

    let crush_nodes: Vec<Value> = match host.ceph_json(&["osd", "df", "tree"]).await {
        Ok(v) => v["nodes"].as_array().cloned().unwrap_or_default(),
        Err(e) => {
            tracing::warn!(
                "reconcile: `ceph osd df tree` did not answer ({e}) — daemons were started, but \
                 nothing else can be decided this tick"
            );
            return;
        }
    };

    let (want_copies, failure_domain) = match crate::topology::read_policy().await {
        Some(crate::topology::PolicyState::Chosen(p)) => (Some(p.size), p.failure_domain),
        _ => (None, "osd".to_string()),
    };

    let osd_state_up: std::collections::HashSet<i64> = crush_nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd") && n["status"].as_str() == Some("up"))
        .filter_map(|n| n["id"].as_i64())
        .collect();

    let mut osd_state: HashMap<i64, (f64, f64, u64)> = HashMap::new();
    for n in &crush_nodes {
        if n["type"].as_str() != Some("osd") {
            continue;
        }
        if let Some(id) = n["id"].as_i64() {
            osd_state.insert(
                id,
                (
                    n["crush_weight"].as_f64().unwrap_or(0.0),
                    n["reweight"].as_f64().unwrap_or(1.0),
                    n["kb"].as_u64().unwrap_or(0),
                ),
            );
        }
    }

    for (disk_id, m) in meta {
        let want_on = wants_on(desired, node, disk_id);
        let Some(&osd_id) = disk_to_osd.get(disk_id) else {
            if !want_on {
                set_phase(disk_id, Phase::Removable, "Not in use. Safe to unplug.");
            }
            continue;
        };
        let (crush_weight, reweight, kb) = osd_state.get(&osd_id).copied().unwrap_or((0.0, 1.0, 0));
        let osd = format!("osd.{osd_id}");

        let state = OsdState {
            crush_weight,
            reweight,
            kb,
            up: osd_state_up.contains(&osd_id),
        };
        let steer = decide_steer(
            want_on,
            m.size_bytes,
            state,
            osd_id,
            &crush_nodes,
            &failure_domain,
            want_copies,
        );

        let mut applied = true;
        if let Some(weight) = steer.set_weight {
            tracing::info!("{osd} ({disk_id}): crush_weight=0 — setting weight={weight:.5}");
            if host
                .ceph(&["osd", "crush", "reweight", &osd, &format!("{weight:.5}")])
                .await
                .is_err()
            {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not set its weight");
            }
        }
        if steer.mark_in {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=ON — marking in");
            if host.ceph(&["osd", "in", &osd]).await.is_err() {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not mark it in");
            }
        }
        if steer.mark_out {
            tracing::info!("{osd} ({disk_id}): reweight={reweight:.2}, desired=OFF — marking out");
            if host.ceph(&["osd", "out", &osd]).await.is_err() {
                applied = false;
                tracing::warn!("{osd} ({disk_id}): could not mark it out");
            }
        }

        if steer.needs_purge {
            let before = destructive::safe_to_destroy(host, osd_id)
                .await
                .ok_or_warn(format!("{osd} ({disk_id}): safe-to-destroy did not answer"))
                .flatten();
            let safe_before = before.is_some();
            if safe_before {
                set_phase(
                    disk_id,
                    Phase::Removing,
                    "Finishing up — do not unplug yet.",
                );
                disable_osd_unit(host, osd_id).await;
            }
            let after = if safe_before {
                destructive::safe_to_destroy(host, osd_id)
                    .await
                    .ok_or_warn(format!("{osd} ({disk_id}): safe-to-destroy did not answer"))
                    .flatten()
            } else {
                None
            };
            let safe_after = after.is_some();
            let loss = if safe_after {
                crate::routers::ceph::assess_pg_loss_via(host).await
            } else {
                None
            };

            match plan_purge(safe_before, safe_after, loss.as_ref()) {
                PurgeVerdict::Wait => {
                    tracing::debug!("{osd} ({disk_id}): out but not yet safe-to-destroy — waiting");
                    let targets = drain_targets_remaining(&crush_nodes, osd_id, &failure_domain);
                    set_phase(
                        disk_id,
                        Phase::Draining,
                        drain_message(targets, want_copies),
                    );
                    continue;
                }
                PurgeVerdict::Recheck => {
                    tracing::warn!("{osd} ({disk_id}): stopped, but no longer reports safe-to-destroy — leaving it alone");
                    continue;
                }
                PurgeVerdict::RefuseDataAtRisk => {
                    if let Some(l) = loss.as_ref() {
                        tracing::warn!(
                            "{osd} ({disk_id}): NOT purging — {} of {} placement groups are unreadable \
                             and cannot be rebuilt, and this disk may hold the only copy",
                            l.stuck,
                            l.total
                        );
                    }
                    set_phase(
                        disk_id,
                        Phase::Removing,
                        "Not removing this disk: data elsewhere in the cluster is unreadable and \
                         cannot be rebuilt, and this disk may hold the only copy of it.",
                    );
                    continue;
                }
                PurgeVerdict::Purge => {}
            }
            let Some(proof) = after else {
                continue;
            };

            match destructive::purge_safe(host, proof).await {
                Ok(Some(receipt)) => {
                    tracing::info!("{osd} ({disk_id}): purged");
                    if !m.device.is_empty() {
                        tracing::info!("{osd} ({disk_id}): returning {} to blank", m.device);
                        wipe_device(host, &m.device, receipt).await;
                    }
                    set_phase(
                        disk_id,
                        Phase::Removable,
                        "Removed from the pool. Safe to unplug.",
                    );
                }
                Ok(None) => {
                    tracing::warn!(
                        "{osd} ({disk_id}): purge reported success but {osd} is still listed — leaving the disk alone"
                    );
                    set_phase(
                        disk_id,
                        Phase::Removing,
                        "Still finishing up — do not unplug this disk yet.",
                    );
                }
                Err(e) => {
                    tracing::warn!("{osd} ({disk_id}): purge failed: {e}");
                    set_phase(
                        disk_id,
                        Phase::Removing,
                        "Still finishing up — do not unplug this disk yet.",
                    );
                }
            }
        } else {
            let (phase, message) = steer_report(&steer, applied);
            set_phase(disk_id, phase, message);
        }
    }

    let unplugged_but_wanted = any_unplugged_but_wanted(desired, node, meta);

    purge_drained_osds(host, node, &crush_nodes, disk_to_osd, unplugged_but_wanted).await;
}

fn any_unplugged_but_wanted(
    desired: &HashMap<String, String>,
    node: &str,
    meta: &HashMap<String, Disk>,
) -> bool {
    let prefix = format!("{node}--");
    desired.iter().any(|(k, v)| {
        if v != "ON" {
            return false;
        }
        match k.strip_prefix(&prefix) {
            Some(d) => !meta.contains_key(d),
            None if is_globally_unique_id(k) => !meta.contains_key(k.as_str()),
            None => false,
        }
    })
}

fn spawn_create<H: Host + 'static>(host: H, disk_id: String, dev_path: String) {
    {
        let Ok(mut running) = CREATING.lock() else {
            return;
        };
        if !running.insert(disk_id.clone()) {
            return;
        }
    }

    let attempt = {
        let mut n = 1;
        if let Ok(mut p) = PROGRESS.lock() {
            let e = p.entry(disk_id.clone()).or_default();
            e.attempts += 1;
            e.last_attempt = Some(std::time::Instant::now());
            n = e.attempts;
        }
        n
    };
    set_phase(
        &disk_id,
        Phase::Creating,
        if attempt == 1 {
            "Setting this disk up for storage…".to_string()
        } else {
            format!("Still setting this disk up… (attempt {attempt})")
        },
    );
    tracing::info!(
        "{disk_id} ({dev_path}): switched ON with no OSD — creating (attempt {attempt})"
    );

    tokio::spawn(async move {
        create_osd(&host, &disk_id, &dev_path).await;
        if let Ok(mut c) = CREATING.lock() {
            c.remove(&disk_id);
        }
    });
}

async fn foreign_osd_on<H: Host>(host: &H, dev_path: &str) -> Option<i64> {
    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await
        .ok()?;
    let our_fsid = host.cluster_fsid().await.ok().filter(|f| !f.is_empty())?;
    foreign_osd_in_list(&raw, &our_fsid, dev_path)
}

async fn create_osd<H: Host>(host: &H, disk_id: &str, dev_path: &str) {
    reclaim_orphan(host, disk_id).await;

    let before = host.osd_ids().await.ok();

    if let Some(id) = foreign_osd_on(host, dev_path).await {
        tracing::warn!(
            "{disk_id}: {dev_path} still holds osd.{id} from another cluster — erasing it first"
        );
        if let Err(e) = destructive::zap(
            host,
            dev_path,
            destructive::ZapWarrant::ForeignCluster { osd: id },
        )
        .await
        {
            tracing::warn!("{disk_id}: zap failed, leaving the disk alone: {e}");
            set_phase(
                disk_id,
                Phase::Retrying,
                "Could not erase this disk yet. YoLab will keep trying.",
            );
            return;
        }
    }

    let is_dm = dev_path.starts_with("/dev/mapper/") || dev_path.starts_with("/dev/dm-");
    if !is_dm {
        if let Ok(out) = host.run_cmd("wipefs", &["--all", dev_path]).await {
            if !out.success {
                tracing::warn!(
                    "{disk_id}: wipefs failed on {dev_path}: {} — ceph-volume may retry it",
                    out.stderr.trim()
                );
            }
        }
    }

    let mut result = host
        .ceph_volume(&[
            "lvm",
            "create",
            "--bluestore",
            "--data",
            dev_path,
            "--no-systemd",
        ])
        .await;

    let warrant = result
        .as_ref()
        .err()
        .and_then(destructive::ZapWarrant::stale_signature);
    if let Some(warrant) = warrant {
        tracing::warn!("{disk_id}: stale BlueStore signature on {dev_path} — zapping and retrying");
        if let Err(e) = destructive::zap(host, dev_path, warrant).await {
            tracing::warn!("{disk_id}: zap failed: {e}");
        } else {
            result = host
                .ceph_volume(&[
                    "lvm",
                    "create",
                    "--bluestore",
                    "--data",
                    dev_path,
                    "--no-systemd",
                ])
                .await;
        }
    }

    match result {
        Ok(_) => match local_osds(host).await {
            Ok(local) => {
                let want = canonical_device(dev_path);
                if let Some((_, osd_id)) = local.iter().find(|(d, _)| canonical_device(d) == want) {
                    start_osd_unit(host, *osd_id).await;
                    set_phase(disk_id, Phase::Active, "Added to the storage pool.");
                    if let Ok(mut p) = PROGRESS.lock() {
                        let e = p.entry(disk_id.to_string()).or_default();
                        e.attempts = 0;
                        e.last_attempt = None;
                        e.orphan_osd_id = None;
                    }
                } else {
                    tracing::warn!(
                            "{disk_id}: ceph-volume reported success but {dev_path} is in no OSD map — will retry"
                        );
                    set_phase(
                        disk_id,
                        Phase::Creating,
                        "Added to the storage pool; waiting for it to come online.",
                    );
                }
            }
            Err(e) => {
                tracing::warn!("{disk_id}: created, but could not confirm it: {e}");
                set_phase(disk_id, Phase::Creating, "Added. Checking it is working…")
            }
        },
        Err(e) => {
            let leaked: Vec<i64> = match (&before, host.osd_ids().await.ok()) {
                (Some(before), Some(after)) => after
                    .iter()
                    .copied()
                    .filter(|id| !before.contains(id))
                    .collect(),
                _ => Vec::new(),
            };
            if let Some(&id) = leaked.first() {
                tracing::warn!(
                    "{disk_id}: create failed after allocating osd.{id} — reclaiming it"
                );
                if let Ok(mut p) = PROGRESS.lock() {
                    p.entry(disk_id.to_string()).or_default().orphan_osd_id = Some(id);
                }
                reclaim_orphan(host, disk_id).await;
            }
            tracing::warn!("{disk_id}: ceph-volume create failed: {e}");
            set_phase(
                disk_id,
                Phase::Retrying,
                "Could not add this disk yet. YoLab will keep trying.",
            );
        }
    }
}

async fn reclaim_orphan<H: Host>(host: &H, disk_id: &str) {
    let Some(id) = PROGRESS
        .lock()
        .ok()
        .and_then(|p| p.get(disk_id).and_then(|d| d.orphan_osd_id))
    else {
        return;
    };

    let Ok(existing) = host.osd_ids().await else {
        return;
    };
    if !existing.contains(&id) {
        if let Ok(mut p) = PROGRESS.lock() {
            p.entry(disk_id.to_string()).or_default().orphan_osd_id = None;
        }
        return;
    }

    let proof = match destructive::safe_to_destroy(host, id).await {
        Ok(Some(proof)) => proof,
        Ok(None) => {
            tracing::warn!(
                "{disk_id}: osd.{id} was left by a failed setup but Ceph will not confirm it is empty — leaving it"
            );
            return;
        }
        Err(e) => {
            tracing::warn!("{disk_id}: could not ask whether osd.{id} is safe to remove: {e}");
            return;
        }
    };

    match destructive::purge_safe(host, proof).await {
        Ok(Some(_)) => {
            tracing::info!("{disk_id}: removed osd.{id}, left behind by a failed setup");
            if let Ok(mut p) = PROGRESS.lock() {
                p.entry(disk_id.to_string()).or_default().orphan_osd_id = None;
            }
        }
        Ok(None) => tracing::warn!(
            "{disk_id}: purge of osd.{id} reported success but it is still listed — keeping the record"
        ),
        Err(e) => tracing::warn!("{disk_id}: could not remove leftover osd.{id}: {e}"),
    }
}

fn mark_known_osds(meta: &mut HashMap<String, Disk>, disk_to_osd: &HashMap<String, i64>) {
    for (disk_id, &osd_id) in disk_to_osd {
        if let Some(d) = meta.get_mut(disk_id) {
            d.osd_id = Some(osd_id);
            d.ownership = Ownership::Ours;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PurgeVerdict {
    Wait,
    Recheck,
    RefuseDataAtRisk,
    Purge,
}

fn plan_purge(
    safe_before_stop: bool,
    safe_after_stop: bool,
    loss: Option<&crate::routers::ceph::PgLoss>,
) -> PurgeVerdict {
    if !safe_before_stop {
        return PurgeVerdict::Wait;
    }
    if !safe_after_stop {
        return PurgeVerdict::Recheck;
    }
    if loss.is_some_and(|l| l.unrecoverable && l.stuck > 0) {
        return PurgeVerdict::RefuseDataAtRisk;
    }
    PurgeVerdict::Purge
}

#[derive(Debug, PartialEq, Eq)]
enum TickPlan {
    Unreachable,
    UnknownOsdMap,
    Proceed,
}

fn plan_tick(reachable: bool, disk_to_osd_known: bool) -> TickPlan {
    if !reachable {
        TickPlan::Unreachable
    } else if !disk_to_osd_known {
        TickPlan::UnknownOsdMap
    } else {
        TickPlan::Proceed
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CreatePlan {
    Create { dev_path: String },
    Blocked(&'static str),
    Waiting,
    Skip,
}

fn retry_backoff(attempts: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 30;
    const CAP_SECS: u64 = 600;
    let shift = attempts.saturating_sub(1).min(6);
    std::time::Duration::from_secs((BASE_SECS.saturating_mul(1 << shift)).min(CAP_SECS))
}

fn retry_due(attempts: u32, since_last: Option<std::time::Duration>) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => elapsed >= retry_backoff(attempts),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_create(
    node: &str,
    disk_id: &str,
    d: &Disk,
    desired: &HashMap<String, String>,
    disk_to_osd: &HashMap<String, i64>,
    already_creating: bool,
    attempts: u32,
    since_last_attempt: Option<std::time::Duration>,
) -> CreatePlan {
    if !wants_on(desired, node, disk_id) || disk_to_osd.contains_key(disk_id) {
        return CreatePlan::Skip;
    }
    if already_creating {
        return CreatePlan::Skip;
    }
    if let Some(reason) = refuse_osd_creation(d) {
        return CreatePlan::Blocked(reason);
    }
    let Some(dev_path) = d.dev_path() else {
        return CreatePlan::Skip;
    };
    if !retry_due(attempts, since_last_attempt) {
        return CreatePlan::Waiting;
    }
    CreatePlan::Create { dev_path }
}

fn wants_on(desired: &HashMap<String, String>, node: &str, disk_id: &str) -> bool {
    disk_id == SYSTEM_OSD_ID
        || desired
            .get(&record_key(node, disk_id))
            .is_some_and(|v| v == "ON")
}

fn refuse_osd_creation(d: &Disk) -> Option<&'static str> {
    match d.ownership {
        Ownership::Ours => {
            return Some(
                "This disk already holds your files, but YoLab has lost track of it. It is \
                 being left alone rather than risk erasing it.",
            )
        }
        Ownership::Unknown => {
            return Some(
                "YoLab can't tell whether this disk holds your files right now, so it is \
                 being left alone. It will be rechecked shortly.",
            )
        }
        Ownership::Foreign | Ownership::Blank => {}
    }
    if d.dev_path().is_none() {
        return Some("This disk disappeared before it could be set up.");
    }
    if d.mounted {
        return Some("This machine is using this disk for something else.");
    }
    None
}

pub(crate) fn parse_lvm_list(raw: &str, our_fsid: &str) -> Result<Vec<(String, i64)>> {
    let json_start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object in ceph-volume output"))?;
    let v: Value =
        serde_json::from_str(&raw[json_start..]).context("parse ceph-volume lvm list")?;

    let mut out = Vec::new();
    let Some(map) = v.as_object() else {
        return Ok(out);
    };
    for (osd_id, entries) in map {
        let Ok(id) = osd_id.parse::<i64>() else {
            continue;
        };
        let Some(list) = entries.as_array() else {
            continue;
        };
        for e in list {
            let tag_fsid = e["tags"]["ceph.cluster_fsid"].as_str().unwrap_or_default();
            if !tag_fsid.is_empty() && !our_fsid.is_empty() && tag_fsid != our_fsid {
                tracing::info!(
                    "parse_lvm_list: skipping osd.{id} — belongs to cluster {tag_fsid}, not {our_fsid}"
                );
                continue;
            }
            if let Some(lv) = e["lv_path"].as_str().filter(|p| !p.is_empty()) {
                out.push((lv.to_string(), id));
            }
            if let Some(devs) = e["devices"].as_array() {
                for d in devs {
                    if let Some(path) = d.as_str().filter(|p| !p.is_empty()) {
                        out.push((path.to_string(), id));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn foreign_osd_ids(raw: &str, our_fsid: &str) -> Vec<i64> {
    let (Ok(all), Ok(ours)) = (parse_lvm_list(raw, ""), parse_lvm_list(raw, our_fsid)) else {
        return Vec::new();
    };
    let our_ids: Vec<i64> = ours.iter().map(|(_, id)| *id).collect();
    all.iter()
        .map(|(_, id)| *id)
        .filter(|id| !our_ids.contains(id))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn foreign_osd_in_list(raw: &str, our_fsid: &str, dev_path: &str) -> Option<i64> {
    let foreign = foreign_osd_ids(raw, our_fsid);
    let want = canonical_device(dev_path);
    parse_lvm_list(raw, "")
        .ok()?
        .into_iter()
        .find(|(d, id)| foreign.contains(id) && canonical_device(d) == want)
        .map(|(_, id)| id)
}

fn is_running(state: Option<&str>) -> bool {
    matches!(state, Some("active" | "activating" | "reloading"))
}

fn is_stopped(state: Option<&str>) -> bool {
    matches!(state, Some("inactive" | "failed"))
}

async fn osd_unit_state<H: Host>(host: &H, osd_id: i64) -> Option<String> {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    host.systemctl(&["show", "-p", "ActiveState", "--value", &unit])
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn stop_foreign_osd_units<H: Host>(host: &H) {
    let Ok(raw) = host.ceph_volume(&["lvm", "list", "--format", "json"]).await else {
        return;
    };
    let Some(our_fsid) = host.cluster_fsid().await.ok().filter(|f| !f.is_empty()) else {
        return;
    };
    for id in foreign_osd_ids(&raw, &our_fsid) {
        let unit = format!("yolab-ceph-osd@{id}.service");
        let state = host
            .systemctl(&["show", "-p", "ActiveState", "--value", &unit])
            .await
            .ok()
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        if state.is_empty() || state == "inactive" {
            continue;
        }
        tracing::warn!("osd.{id}: belongs to another cluster — stopping {unit}");
        host.systemctl(&["stop", &unit])
            .await
            .warn_on_err(format!("stop {unit}"));
    }
}

async fn erase_forgotten_osds<H: Host>(
    host: &H,
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
    disk_to_osd: &HashMap<String, i64>,
) -> std::collections::HashSet<String> {
    let mut forgotten = std::collections::HashSet::new();
    let Ok(ids) = host.osd_ids().await else {
        return forgotten;
    };
    for (disk_id, &osd_id) in disk_to_osd {
        if disk_id == SYSTEM_OSD_ID || ids.contains(&osd_id) {
            continue;
        }
        if CREATING.lock().map(|c| c.contains(disk_id)).unwrap_or(true) {
            continue;
        }
        forgotten.insert(disk_id.clone());
        let unit = format!("yolab-ceph-osd@{osd_id}.service");
        host.systemctl(&["stop", &unit])
            .await
            .warn_on_err(format!("stop {unit}"));
        if !wants_on(desired, node, disk_id) {
            set_phase(disk_id, Phase::Removable, "Not in use. Safe to unplug.");
            continue;
        }
        let Some(dev_path) = meta.get(disk_id).and_then(Disk::dev_path) else {
            continue;
        };
        tracing::warn!(
            "{disk_id}: {dev_path} carries osd.{osd_id}, which this cluster no longer has — erasing it for a new one"
        );
        match destructive::zap(
            host,
            &dev_path,
            destructive::ZapWarrant::ForgottenByCluster {
                osd: osd_id,
                whole_disk: true,
            },
        )
        .await
        {
            Ok(()) => set_phase(
                disk_id,
                Phase::Creating,
                "Erasing old data before adding this disk.",
            ),
            Err(e) => {
                tracing::warn!("{disk_id}: erasing {dev_path} failed: {e}");
                set_phase(
                    disk_id,
                    Phase::Retrying,
                    "Could not erase this disk yet. YoLab will keep trying.",
                );
            }
        }
    }
    forgotten
}

async fn ensure_osd_unit_running<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    if is_running(osd_unit_state(host, osd_id).await.as_deref()) {
        return;
    }
    tracing::warn!("osd.{osd_id}: {unit} is not running — starting it");
    start_osd_unit(host, osd_id).await;
}

async fn start_osd_unit<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: starting {unit}");
    match host.systemctl(&["start", &unit]).await {
        Ok(o) if o.success => tracing::info!("osd.{osd_id}: {unit} started"),
        Ok(o) => tracing::warn!("osd.{osd_id}: starting {unit} failed: {}", o.stderr.trim()),
        Err(e) => tracing::warn!("osd.{osd_id}: could not run systemctl: {e}"),
    }
}

async fn disable_osd_unit<H: Host>(host: &H, osd_id: i64) {
    let unit = format!("yolab-ceph-osd@{osd_id}.service");
    tracing::info!("osd.{osd_id}: stopping {unit}");
    host.systemctl(&["stop", &unit])
        .await
        .warn_on_err(format!("stop {unit}"));

    for _ in 0..15 {
        if is_stopped(osd_unit_state(host, osd_id).await.as_deref()) {
            return;
        }
        sleep(Duration::from_secs(1)).await;
    }
    tracing::warn!("osd.{osd_id}: {unit} still active after 15s");
}

async fn wipe_device<H: Host>(host: &H, device: &str, receipt: destructive::Purged) {
    let dev_path = if device.starts_with('/') {
        device.to_string()
    } else {
        format!("/dev/{device}")
    };
    match destructive::zap(
        host,
        &dev_path,
        destructive::ZapWarrant::AfterPurge(receipt),
    )
    .await
    {
        Ok(_) => tracing::info!("wipe_device: {dev_path} zapped and returned to a blank state"),
        Err(e) => tracing::warn!(
            "wipe_device: could not zap {dev_path}: {e} — the disk stays registered and this \
             is retried on the next tick"
        ),
    }
}

async fn purge_drained_osds<H: Host>(
    host: &H,
    node: &str,
    crush_nodes: &[Value],
    disk_to_osd: &HashMap<String, i64>,
    unplugged_but_wanted: bool,
) {
    let host_osd_ids: std::collections::HashSet<i64> = crush_nodes
        .iter()
        .find(|n| n["type"].as_str() == Some("host") && n["name"].as_str() == Some(node))
        .and_then(|h| h["children"].as_array())
        .map(|c| c.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    let active_osd_ids: std::collections::HashSet<i64> = disk_to_osd.values().copied().collect();

    for n in crush_nodes {
        if n["type"].as_str() != Some("osd") {
            continue;
        }
        let Some(osd_id) = n["id"].as_i64() else {
            continue;
        };
        if !host_osd_ids.contains(&osd_id) {
            continue;
        }
        if active_osd_ids.contains(&osd_id) {
            continue;
        }

        if unplugged_but_wanted {
            tracing::info!(
                "osd.{osd_id}: leaving it alone — a disk on this node is switched on but not \
                 connected, and this may be it"
            );
            continue;
        }

        let reweight = n["reweight"].as_f64().unwrap_or(1.0);
        let status = n["status"].as_str().unwrap_or("up");
        if reweight > 0.5 || status != "down" {
            continue;
        }

        disable_osd_unit(host, osd_id).await;

        let proof = match destructive::safe_to_destroy(host, osd_id).await {
            Ok(Some(proof)) => proof,
            Ok(None) => {
                tracing::info!("osd.{osd_id}: disk gone but not yet safe-to-destroy — waiting");
                continue;
            }
            Err(e) => {
                tracing::warn!("osd.{osd_id}: could not ask whether it is safe to destroy: {e}");
                continue;
            }
        };

        tracing::info!("osd.{osd_id}: disk gone, out, safe-to-destroy — purging from Ceph");
        match destructive::purge_safe(host, proof).await {
            Ok(Some(_)) => tracing::info!("osd.{osd_id}: purged"),
            Ok(None) => {
                tracing::warn!("osd.{osd_id}: purge reported success but it is still listed")
            }
            Err(e) => tracing::warn!("osd.{osd_id}: purge failed: {e}"),
        }
    }
}

fn weight_tib_from(kb: u64, size_bytes: u64) -> f64 {
    if kb > 0 {
        kb as f64 / (1u64 << 30) as f64
    } else if size_bytes > 0 {
        size_bytes as f64 / (1u64 << 40) as f64
    } else {
        0.0
    }
}

async fn auto_register_all_disks<H: Host>(
    host: &H,
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
) -> usize {
    let records = new_disk_records(node, meta, desired);
    let registered = records.len();
    for (key, setting) in records {
        let full = format!("{}{key}", settings::DISKS);
        match settings::set(host, &full, setting).await {
            Ok(()) => tracing::info!("disk {key}: first seen on {node}, registered {setting}"),
            Err(e) => tracing::warn!("disk {key}: could not register it ({e})"),
        }
    }
    registered
}

fn new_disk_records(
    node: &str,
    meta: &HashMap<String, Disk>,
    desired: &HashMap<String, String>,
) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = meta
        .keys()
        .filter(|disk_id| disk_id.as_str() != SYSTEM_OSD_ID)
        .map(|disk_id| record_key(node, disk_id))
        .filter(|key| !desired.contains_key(key))
        .map(|key| (key, "OFF"))
        .collect();
    out.sort();
    out
}

fn is_user_disk(name: &str) -> bool {
    const VIRTUAL_PREFIXES: [&str; 6] = ["rbd", "loop", "zram", "zd", "md", "dm-"];
    !VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DiskFlags {
    pub has_partitions: bool,
    pub mounted: bool,
}

pub(crate) fn parse_disk_flags(dev: &Value) -> DiskFlags {
    fn mounted_anywhere(n: &Value) -> bool {
        let own = match &n["mountpoints"] {
            Value::Array(a) => a.iter().any(|m| m.as_str().is_some_and(|s| !s.is_empty())),
            v => v.as_str().is_some_and(|s| !s.is_empty()),
        };
        let legacy = n["mountpoint"].as_str().is_some_and(|s| !s.is_empty());
        own || legacy
            || n["children"]
                .as_array()
                .is_some_and(|c| c.iter().any(mounted_anywhere))
    }

    DiskFlags {
        has_partitions: dev["children"]
            .as_array()
            .is_some_and(|c| c.iter().any(|ch| ch["type"].as_str() == Some("part"))),
        mounted: mounted_anywhere(dev),
    }
}

async fn scan_devices<H: Host>(host: &H) -> Option<Vec<(String, DiskFlags)>> {
    let out = host
        .run_cmd("lsblk", &["-J", "-o", "NAME,TYPE,MOUNTPOINTS"])
        .await
        .ok()?;
    if !out.success {
        return None;
    }
    let json = serde_json::from_slice::<Value>(out.stdout.as_bytes()).ok()?;
    let mut devices = Vec::new();
    if let Some(devs) = json["blockdevices"].as_array() {
        for dev in devs {
            if dev["type"].as_str() != Some("disk") {
                continue;
            }
            let name = dev["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            if !is_user_disk(&name) {
                continue;
            }
            let flags = parse_disk_flags(dev);
            if flags.mounted {
                continue;
            }
            devices.push((name, flags));
        }
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    Some(devices)
}

fn read_bluestore_header(device: &str) -> Option<[u8; 4096]> {
    let path = if device.starts_with('/') {
        device.to_string()
    } else {
        format!("/dev/{device}")
    };
    let mut buf = [0u8; 4096];
    let mut f = std::fs::File::open(path).ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn bluestore_fsid(device: &str) -> Option<String> {
    let buf = read_bluestore_header(device)?;
    if !buf.starts_with(BLUESTORE_MAGIC) {
        return None;
    }
    let pos = buf
        .windows(CEPH_FSID_KEY.len())
        .position(|w| w == CEPH_FSID_KEY)?;
    let vs = pos + CEPH_FSID_KEY.len();
    if vs + 40 > buf.len() {
        return None;
    }
    if u32::from_le_bytes(buf[vs..vs + 4].try_into().ok()?) != 36 {
        return None;
    }
    let fsid = std::str::from_utf8(&buf[vs + 4..vs + 40]).ok()?;
    is_uuid(fsid).then(|| fsid.to_string())
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&l, p)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(crate) fn record_key(node: &str, disk_id: &str) -> String {
    if is_globally_unique_id(disk_id) {
        disk_id.to_string()
    } else {
        format!("{node}--{disk_id}")
    }
}

pub(crate) fn is_globally_unique_id(disk_id: &str) -> bool {
    disk_id.starts_with("serial-")
}

const ID_PREFIXES: [&str; 6] = ["wwn-", "nvme-eui.", "nvme-", "ata-", "scsi-", "usb-"];
const BY_ID_DIR: &str = "/dev/disk/by-id";

fn disk_id(device: &str) -> String {
    disk_id_from(device, stable_id_for(device).as_deref())
}

fn stable_id_for(device: &str) -> Option<String> {
    let target = std::fs::canonicalize(format!("/dev/{device}")).ok()?;
    let mut best: Option<(usize, String)> = None;
    for entry in std::fs::read_dir(BY_ID_DIR).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("-part") || name.contains("-part") {
            continue;
        }
        let Some(rank) = ID_PREFIXES.iter().position(|p| name.starts_with(p)) else {
            continue;
        };
        if std::fs::canonicalize(entry.path()).ok().as_deref() != Some(target.as_path()) {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, name));
        }
    }
    best.map(|(_, name)| name)
}

fn disk_id_from(device: &str, stable: Option<&str>) -> String {
    if let Some(serial) = stable {
        let s = serial.trim();
        if !s.is_empty() {
            let safe: String = s
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
                .to_lowercase();
            return format!("serial-{}", safe.trim_matches('-'));
        }
    }
    format!("dev-{device}")
}

fn disk_meta(device: &str, our_fsid: &str, flags: DiskFlags) -> Disk {
    let model = std::fs::read_to_string(format!("/sys/block/{device}/device/model"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let size_bytes: u64 = std::fs::read_to_string(format!("/sys/block/{device}/size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 512;
    Disk {
        device: device.to_string(),
        model,
        size_bytes,
        is_loop: false,
        ownership: Ownership::read(bluestore_fsid(device).as_deref(), our_fsid),
        has_partitions: flags.has_partitions,
        mounted: flags.mounted,
        osd_id: None,
        progress: None,
    }
}

async fn write_status<H: Host>(host: &H, node: &str, meta: &HashMap<String, Disk>) {
    let wire: BTreeMap<&str, Value> = meta
        .iter()
        .map(|(k, d)| (k.as_str(), d.to_value()))
        .collect();
    let payload = json!({ "disks": wire }).to_string();
    if last_published(node).as_deref() == Some(payload.as_str()) {
        return;
    }
    let key = format!("{}{node}", settings::DISK_STATUS);
    settings::set(host, &key, &payload)
        .await
        .warn_on_err("publish this node's disk inventory");
    remember_published(node, payload);
}

static PUBLISHED_STATUS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn published_status() -> &'static Mutex<HashMap<String, String>> {
    PUBLISHED_STATUS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn last_published(node: &str) -> Option<String> {
    published_status()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(node)
        .cloned()
}

fn remember_published(node: &str, payload: String) {
    published_status()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(node.to_string(), payload);
}

async fn read_desired<H: Host>(host: &H) -> Option<HashMap<String, String>> {
    match settings::dump(host, settings::DISKS).await {
        Ok(records) => Some(records.into_iter().collect()),
        Err(e) => {
            tracing::warn!("disk settings are unreadable right now ({e})");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use crate::exec::CmdError;
    use crate::host::fake::FakeHost;
    use crate::host::{CommandOutput, HostResult};

    const OURS: &str = "11111111-2222-3333-4444-555555555555";
    const THEIRS: &str = "99999999-8888-7777-6666-555555555555";

    #[derive(Clone, Default)]
    struct RecordingHost {
        ceph_volume_calls: Arc<Mutex<usize>>,
    }

    fn unreachable_err(what: &str) -> CmdError {
        CmdError::Timeout {
            cmd: what.to_string(),
            after: std::time::Duration::from_secs(30),
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for RecordingHost {
        fn ceph<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            async move { Err(unreachable_err("ceph")) }
        }

        fn ceph_json<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
            async move { Err(unreachable_err("ceph")) }
        }

        fn ceph_volume<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let calls = self.ceph_volume_calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Err(unreachable_err("ceph-volume"))
            }
        }

        fn kubectl<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            async move { Err(unreachable_err("kubectl")) }
        }

        fn kubectl_json<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
            async move { Err(unreachable_err("kubectl")) }
        }

        fn kubectl_apply<'a>(
            &self,
            _manifest: &'a str,
        ) -> impl Future<Output = HostResult<()>> + Send + 'a {
            async move { Err(unreachable_err("kubectl")) }
        }

        fn systemctl<'a>(
            &self,
            _args: &'a [&str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            async move { Err(unreachable_err("systemctl")) }
        }

        fn run_cmd<'a>(
            &self,
            _bin: &'a str,
            _args: &'a [&'a str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            async move { Err(unreachable_err("command")) }
        }
    }

    #[tokio::test]
    async fn create_osd_the_happy_path_confirms_and_starts_the_unit() {
        let host = FakeHost::new()
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .ok("wipefs --all /dev/sdb", "")
            .ok("ceph-volume lvm create", "")
            .ok(
                "ceph-volume lvm list",
                r#"{"5":[{"devices":["/dev/sdb"],"tags":{"ceph.cluster_fsid":"11111111-2222-3333-4444-555555555555"}}]}"#,
            )
            .ok("systemctl start yolab-ceph-osd@5.service", "");

        create_osd(&host, "disk-happy", "/dev/sdb").await;

        assert_eq!(progress_of("disk-happy").phase, Phase::Active);
        assert!(host.ran("start yolab-ceph-osd@5.service"));
        assert!(
            !host.ran("lvm zap"),
            "a blank disk must never be zapped: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn create_osd_erases_a_foreign_disk_before_attempting_create() {
        let host = FakeHost::new()
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph osd ls", "[]")
            .ok(
                "ceph-volume lvm list",
                r#"{"1":[{"devices":["/dev/sdc"],"tags":{"ceph.cluster_fsid":"99999999-8888-7777-6666-555555555555"}}]}"#,
            )
            .ok("ceph-volume lvm zap", "")
            .ok("wipefs --all /dev/sdc", "")
            .ok("ceph-volume lvm create", "");

        create_osd(&host, "disk-foreign", "/dev/sdc").await;

        let zap_at = host
            .position("lvm zap")
            .expect("a foreign disk must be zapped");
        let create_at = host
            .position("lvm create")
            .expect("create must still be attempted after the erase");
        assert!(
            zap_at < create_at,
            "zap must happen before create, not after a failure: {:?}",
            host.calls()
        );
        assert!(
            host.ran("lvm zap --destroy"),
            "a foreign stack is a volume group and must be removed, not just its contents: {:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn create_osd_retries_once_after_a_stale_bluestore_signature() {
        let host = FakeHost::new()
            .ok(
                "ceph fsid",
                r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#,
            )
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .fail(
                "ceph-volume lvm create",
                "RuntimeError: Device /dev/mapper/pool-ceph has bluestore signature.",
            )
            .ok("ceph-volume lvm create", "")
            .ok("ceph-volume lvm zap", "");

        create_osd(&host, "disk-stale", "/dev/mapper/pool-ceph").await;

        let creates = host
            .calls()
            .iter()
            .filter(|c| c.contains("lvm create"))
            .count();
        assert_eq!(
            creates,
            2,
            "must retry exactly once, not loop: {:?}",
            host.calls()
        );
        assert!(
            !host.ran("lvm zap --destroy"),
            "the system LV belongs to disko and must never be --destroy'd: {:?}",
            host.calls()
        );
    }

    #[test]
    fn retry_backoff_grows_exponentially_and_is_capped() {
        assert_eq!(retry_backoff(1), std::time::Duration::from_secs(30));
        assert_eq!(retry_backoff(2), std::time::Duration::from_secs(60));
        assert_eq!(retry_backoff(3), std::time::Duration::from_secs(120));
        assert_eq!(retry_backoff(7), std::time::Duration::from_secs(600));
        assert_eq!(
            retry_backoff(20),
            std::time::Duration::from_secs(600),
            "must stay capped, not overflow or keep growing forever"
        );
    }

    #[test]
    fn retry_due_is_always_true_on_the_first_attempt() {
        assert!(retry_due(0, None));
        assert!(retry_due(1, None));
    }

    #[test]
    fn retry_due_blocks_until_the_backoff_elapses() {
        assert!(!retry_due(3, Some(std::time::Duration::from_secs(100))));
        assert!(retry_due(3, Some(std::time::Duration::from_secs(120))));
        assert!(retry_due(3, Some(std::time::Duration::from_secs(121))));
    }

    #[test]
    fn plan_create_waits_rather_than_hammering_a_disk_that_just_failed() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            3,
            Some(std::time::Duration::from_secs(5)),
        );
        assert_eq!(plan, CreatePlan::Waiting);
    }

    #[test]
    fn plan_create_retries_once_the_backoff_has_elapsed() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            3,
            Some(std::time::Duration::from_secs(120)),
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/sdb".to_string()
            }
        );
    }

    #[tokio::test]
    async fn purge_wipes_the_disk_once_the_osd_is_confirmed_gone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph-volume lvm list", "{}")
            .ok(
                "ceph osd df tree",
                r#"{"nodes":[{"id":7,"type":"osd","status":"down","crush_weight":0.5,"reweight":0.0,"kb":0}]}"#,
            )
            .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[7]}"#)
            .ok(
                "systemctl show -p ActiveState --value yolab-ceph-osd@7.service",
                "inactive",
            )
            .ok("ceph osd purge", "purged osd.7")
            .ok("ceph osd ls", "[7]")
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm zap", "");

        let meta = HashMap::from([("disk-purge".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-purge".to_string(), "OFF".to_string())]);
        let disk_to_osd = HashMap::from([("disk-purge".to_string(), 7i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), true).await;

        assert!(
            host.ran("lvm zap"),
            "a confirmed purge must wipe the disk: {:?}",
            host.calls()
        );
        assert_eq!(progress_of("disk-purge").phase, Phase::Removable);
    }

    #[tokio::test]
    async fn a_switched_on_disk_whose_osd_the_cluster_forgot_is_erased_not_started() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd ls", "[0]")
            .ok("systemctl stop yolab-ceph-osd@1.service", "")
            .ok("ceph-volume lvm zap", "");
        let meta = HashMap::from([("disk-old".to_string(), disk(Ownership::Ours))]);
        let desired = HashMap::from([(record_key("node1", "disk-old"), "ON".to_string())]);
        let disk_to_osd = HashMap::from([("disk-old".to_string(), 1i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), true).await;

        assert!(
            host.ran("ceph-volume lvm zap --destroy /dev/sdb"),
            "{:?}",
            host.calls()
        );
        assert!(host.ran("systemctl stop yolab-ceph-osd@1.service"));
        assert!(!host.ran("systemctl start yolab-ceph-osd@1"));
        assert!(!host.ran("crush reweight") && !host.ran("osd in"));
        assert!(
            !host.ran("lvm create"),
            "created on the next tick, once it reads blank"
        );
    }

    #[tokio::test]
    async fn a_tick_on_the_mons_list_creates_nothing_but_still_starts_known_osds() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd ls", "[5]")
            .ok(
                "systemctl show -p ActiveState --value yolab-ceph-osd@5.service",
                "inactive",
            )
            .ok("systemctl start yolab-ceph-osd@5.service", "");
        let meta = HashMap::from([
            ("disk-new-osd".to_string(), disk(Ownership::Blank)),
            ("disk-known".to_string(), disk(Ownership::Ours)),
        ]);
        let desired = HashMap::from([
            (record_key("node1", "disk-new-osd"), "ON".to_string()),
            (record_key("node1", "disk-known"), "ON".to_string()),
        ]);
        let disk_to_osd = HashMap::from([("disk-known".to_string(), 5i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), false).await;

        assert!(
            !host.ran("lvm create") && !host.ran("wipefs"),
            "{:?}",
            host.calls()
        );
        assert!(!CREATING.lock().unwrap().contains("disk-new-osd"));
        assert!(
            host.ran("systemctl start yolab-ceph-osd@5.service"),
            "{:?}",
            host.calls()
        );
    }

    #[tokio::test]
    async fn the_osd_list_says_where_it_came_from() {
        let meta = HashMap::from([("disk-b".to_string(), disk(Ownership::Blank))]);

        let busy = FakeHost::new()
            .fail(
                "ceph-volume lvm list",
                "skipped, already running on this node",
            )
            .ok(
                "ceph osd metadata",
                r#"[{"id": 2, "hostname": "node1", "devices": "sdb"}]"#,
            );
        let (map, source) = fetch_disk_to_osd(&busy, "node1", &meta).await.unwrap();
        assert_eq!(source, OsdMapSource::Mon);
        assert_eq!(map.get("disk-b"), Some(&2));

        let local = FakeHost::new().ok("ceph-volume lvm list", "{}").ok(
            "ceph fsid",
            r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#,
        );
        let (_, source) = fetch_disk_to_osd(&local, "node1", &meta).await.unwrap();
        assert_eq!(source, OsdMapSource::CephVolume);
    }

    #[tokio::test]
    async fn a_switched_off_disk_whose_osd_the_cluster_forgot_is_left_alone() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph osd ls", "[0]")
            .ok("systemctl stop yolab-ceph-osd@3.service", "");
        let meta = HashMap::from([("disk-off".to_string(), disk(Ownership::Ours))]);
        let desired = HashMap::from([(record_key("node1", "disk-off"), "OFF".to_string())]);
        let disk_to_osd = HashMap::from([("disk-off".to_string(), 3i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), true).await;

        assert!(!host.ran("zap") && !host.ran("osd purge") && !host.ran("osd out"));
        assert_eq!(progress_of("disk-off").phase, Phase::Removable);
    }

    #[tokio::test]
    async fn purge_never_wipes_when_the_osd_is_still_listed_afterward() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", r#"{"fsid":"11111111-2222-3333-4444-555555555555"}"#)
            .ok("ceph-volume lvm list", "{}")
            .ok(
                "ceph osd df tree",
                r#"{"nodes":[{"id":9,"type":"osd","status":"down","crush_weight":0.5,"reweight":0.0,"kb":0}]}"#,
            )
            .ok("ceph osd safe-to-destroy", r#"{"safe_to_destroy":[9]}"#)
            .ok(
                "systemctl show -p ActiveState --value yolab-ceph-osd@9.service",
                "inactive",
            )
            .ok("ceph osd purge", "purged osd.9")
            .ok("ceph osd ls", "[9]");

        let meta = HashMap::from([("disk-stuck".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-stuck".to_string(), "OFF".to_string())]);
        let disk_to_osd = HashMap::from([("disk-stuck".to_string(), 9i64)]);

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), true).await;

        assert!(
            !host.ran("lvm zap"),
            "a purge that is not confirmed gone must never wipe the disk: {:?}",
            host.calls()
        );
        assert_eq!(progress_of("disk-stuck").phase, Phase::Removing);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_reports_unknown_and_never_touches_a_disk() {
        let host = RecordingHost::default();
        let meta = HashMap::from([("disk-a".to_string(), disk(Ownership::Blank))]);
        let desired = HashMap::from([("disk-a".to_string(), "ON".to_string())]);
        let disk_to_osd = HashMap::new();

        reconcile_local_osds(&host, "node1", &meta, &desired, Some(&disk_to_osd), true).await;

        assert_eq!(progress_of("disk-a").phase, Phase::Unknown);
        assert_eq!(*host.ceph_volume_calls.lock().unwrap(), 0);
    }

    fn bluestore_label(fsid: &str) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];
        buf[..BLUESTORE_MAGIC.len()].copy_from_slice(BLUESTORE_MAGIC);
        let at = 512;
        buf[at..at + CEPH_FSID_KEY.len()].copy_from_slice(CEPH_FSID_KEY);
        let vs = at + CEPH_FSID_KEY.len();
        buf[vs..vs + 4].copy_from_slice(&36u32.to_le_bytes());
        buf[vs + 4..vs + 4 + fsid.len()].copy_from_slice(fsid.as_bytes());
        buf
    }

    fn fake_device(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn is_uuid_accepts_a_canonical_uuid() {
        assert!(is_uuid(OURS));
        assert!(is_uuid("deadbeef-DEAD-beef-DEAD-beefdeadbeef"));
    }

    #[test]
    fn is_uuid_rejects_the_empty_string() {
        assert!(!is_uuid(""));
    }

    #[test]
    fn is_uuid_rejects_malformed_shapes() {
        assert!(!is_uuid("1111-2222-3333-4444"));
        assert!(!is_uuid("11111111-2222-3333-4444-555555555555-6"));
        assert!(!is_uuid("1111111-2222-3333-4444-555555555555"));
        assert!(!is_uuid("gggggggg-2222-3333-4444-555555555555"));
        assert!(!is_uuid("11111111 2222 3333 4444 555555555555"));
        assert!(!is_uuid("----"));
    }

    #[test]
    fn foreign_and_unknown_are_distinguishable_on_the_wire() {
        let disk = |o| Disk {
            device: "sdb".into(),
            model: "easystore".into(),
            size_bytes: 1000,
            is_loop: false,
            ownership: o,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        };

        let foreign = disk(Ownership::Foreign).to_value();
        let unknown = disk(Ownership::Unknown).to_value();

        assert_eq!(foreign["foreign_ceph"], json!(true));
        assert_eq!(unknown["foreign_ceph"], json!(true));
        assert_eq!(foreign["ownership"], json!("foreign"));
        assert_eq!(unknown["ownership"], json!("unknown"));
    }

    #[test]
    fn every_ownership_state_has_a_distinct_wire_name() {
        let names: Vec<&str> = [
            Ownership::Ours,
            Ownership::Foreign,
            Ownership::Blank,
            Ownership::Unknown,
        ]
        .iter()
        .map(|o| o.as_str())
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names collide: {names:?}");
    }

    #[test]
    fn bluestore_fsid_reads_a_well_formed_label() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        assert_eq!(bluestore_fsid(&dev).as_deref(), Some(OURS));
    }

    #[test]
    fn bluestore_fsid_returns_none_without_the_magic() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sdb", &vec![0u8; 4096]);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_for_a_device_shorter_than_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sdc", b"bluestore block device\n");
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_when_the_device_does_not_exist() {
        assert_eq!(bluestore_fsid("/nonexistent/definitely-not-a-device"), None);
    }

    #[test]
    fn bluestore_fsid_returns_none_when_the_length_prefix_is_not_36() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = bluestore_label(OURS);
        let vs = 512 + CEPH_FSID_KEY.len();
        buf[vs..vs + 4].copy_from_slice(&16u32.to_le_bytes());
        let dev = fake_device(&dir, "sdd", &buf);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn bluestore_fsid_never_returns_a_non_uuid_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = bluestore_label(OURS);
        let vs = 512 + CEPH_FSID_KEY.len();
        buf[vs + 4..vs + 40].fill(b' ');
        let dev = fake_device(&dir, "sde", &buf);
        assert_eq!(bluestore_fsid(&dev), None);
    }

    #[test]
    fn a_label_matching_our_cluster_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Ours
        );
    }

    #[test]
    fn a_label_from_another_cluster_is_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(THEIRS));
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Foreign
        );
    }

    #[test]
    fn a_label_we_cannot_attribute_is_unknown_and_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &bluestore_label(OURS));
        let d = disk_meta(&dev, "", DiskFlags::default());
        assert_eq!(d.ownership, Ownership::Unknown);
        assert!(refuse_osd_creation(&d).is_some());
    }

    #[test]
    fn an_unlabelled_disk_is_blank() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "sda", &vec![0u8; 4096]);
        assert_eq!(
            disk_meta(&dev, OURS, DiskFlags::default()).ownership,
            Ownership::Blank
        );
    }

    #[test]
    fn the_wire_shape_the_ui_reads_is_unchanged() {
        let base = Disk {
            device: "sdb".into(),
            model: "easystore".into(),
            size_bytes: 1000,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        };

        let v = base.to_value();
        assert_eq!(v["device"], json!("sdb"));
        assert_eq!(v["model"], json!("easystore"));
        assert_eq!(v["size_bytes"], json!(1000));
        assert_eq!(v["is_our_osd"], json!(false));
        assert_eq!(v["foreign_ceph"], json!(false));
        assert_eq!(v["ownership"], json!("blank"));
        assert_eq!(v["has_partitions"], json!(false));
        assert_eq!(v["mounted"], json!(false));
        assert_eq!(v["osd_id"], json!(null));
        assert!(
            v.get("is_loop").is_none(),
            "absent on a normal disk, as before"
        );

        for (own, ours, foreign) in [
            (Ownership::Ours, true, false),
            (Ownership::Foreign, false, true),
            (Ownership::Unknown, false, true),
            (Ownership::Blank, false, false),
        ] {
            let v = Disk {
                ownership: own,
                ..base.clone()
            }
            .to_value();
            assert_eq!(v["is_our_osd"], json!(ours), "{own:?}");
            assert_eq!(v["foreign_ceph"], json!(foreign), "{own:?}");
        }

        let sys = Disk {
            is_loop: true,
            ..base.clone()
        }
        .to_value();
        assert_eq!(sys["is_loop"], json!(true));

        let running = Disk {
            osd_id: Some(3),
            progress: Some(DiskProgress {
                phase: Phase::Active,
                message: "In use".into(),
                attempts: 2,
                orphan_osd_id: None,
                last_attempt: None,
            }),
            ..base.clone()
        }
        .to_value();
        assert_eq!(running["osd_id"], json!(3));
        assert_eq!(running["phase"], json!(Phase::Active.as_str()));
        assert_eq!(running["message"], json!("In use"));
        assert_eq!(running["attempts"], json!(2));
    }

    #[test]
    fn a_disk_with_no_progress_publishes_no_phase() {
        let v = Disk {
            device: "sdb".into(),
            model: String::new(),
            size_bytes: 0,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: Some(DiskProgress::default()),
        }
        .to_value();
        assert!(v.get("phase").is_none());
    }

    #[test]
    fn a_hardware_id_is_not_scoped_to_a_machine() {
        assert_eq!(
            record_key("node1", "serial-wwn-0x50014ee214caf529"),
            "serial-wwn-0x50014ee214caf529"
        );
        assert_eq!(
            record_key("node1", "serial-wwn-0xabc"),
            record_key("node3", "serial-wwn-0xabc")
        );
    }

    #[test]
    fn an_id_that_only_means_something_locally_stays_scoped() {
        assert_eq!(record_key("node1", "dev-sda"), "node1--dev-sda");
        assert_ne!(
            record_key("node1", "dev-sda"),
            record_key("node3", "dev-sda")
        );
        assert_eq!(record_key("node1", "system"), "node1--system");
        assert_ne!(record_key("node1", "system"), record_key("node3", "system"));
    }

    #[test]
    fn only_hardware_ids_count_as_globally_unique() {
        assert!(is_globally_unique_id("serial-wwn-0xabc"));
        assert!(is_globally_unique_id("serial-ata-wdc-wd10"));
        for local in ["dev-sda", "dev-nvme0n1", "system", "", "loop0"] {
            assert!(!is_globally_unique_id(local), "{local} names a position");
        }
    }

    #[test]
    fn a_hardware_id_beats_the_kernel_name() {
        assert_eq!(
            disk_id_from("sdc", Some("wwn-0x50014ee214caf529")),
            "serial-wwn-0x50014ee214caf529".replace(['.', '_'], "-")
        );
    }

    #[test]
    fn the_id_ranking_prefers_hardware_identity_over_the_enclosure() {
        let rank = |n: &str| ID_PREFIXES.iter().position(|p| n.starts_with(p));
        let wwn = rank("wwn-0x50014ee214caf529").unwrap();
        let ata = rank("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR33").unwrap();
        let usb = rank("usb-WD_easystore_2647_575844324135314C41523333-0:0").unwrap();
        assert!(wwn < ata, "the World Wide Name is the strongest identity");
        assert!(
            ata < usb,
            "a USB id may describe the caddy rather than the disk"
        );
    }

    #[test]
    fn unranked_links_are_ignored() {
        let rank = |n: &str| ID_PREFIXES.iter().position(|p| n.starts_with(p));
        assert!(rank("dm-name-pool-ceph").is_none());
        assert!(rank("lvm-pv-uuid-3MOvZ3-dMBQ").is_none());
    }

    #[test]
    fn the_kernel_name_remains_the_last_resort() {
        assert_eq!(disk_id_from("sdc", None), "dev-sdc");
        assert_eq!(disk_id_from("sdc", Some("")), "dev-sdc");
        assert_eq!(disk_id_from("sdc", Some("  \n ")), "dev-sdc");
    }

    #[test]
    fn two_different_disks_never_sanitise_to_the_same_id() {
        let a = disk_id_from("sdb", Some("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR33"));
        let b = disk_id_from("sdc", Some("ata-WDC_WD10SDRW-11A0XS1_WD-WXD2A51LAR34"));
        assert_ne!(a, b);
        assert!(a.starts_with("serial-"));
    }

    #[test]
    fn disk_id_prefers_the_serial_number() {
        assert_eq!(disk_id_from("sda", Some("S3Z1NB0K")), "serial-s3z1nb0k");
    }

    #[test]
    fn disk_id_replaces_characters_a_configmap_key_cannot_hold() {
        assert_eq!(
            disk_id_from("sda", Some("WD/Blue 500:GB")),
            "serial-wd-blue-500-gb"
        );
    }

    #[test]
    fn disk_id_trims_surrounding_whitespace_and_dashes() {
        assert_eq!(disk_id_from("sda", Some("  ABC123  ")), "serial-abc123");
        assert_eq!(disk_id_from("sda", Some("__ABC__")), "serial-abc");
    }

    #[test]
    fn disk_id_falls_back_to_the_device_name_without_a_usable_serial() {
        assert_eq!(disk_id_from("sda", None), "dev-sda");
        assert_eq!(disk_id_from("sda", Some("")), "dev-sda");
        assert_eq!(disk_id_from("sda", Some("   \n")), "dev-sda");
    }

    #[test]
    fn weight_prefers_cephs_own_kb_over_lsblk_bytes() {
        let kb = 1u64 << 30;
        assert_eq!(weight_tib_from(kb, 999), 1.0);
    }

    #[test]
    fn weight_falls_back_to_size_bytes_when_ceph_reports_nothing() {
        assert_eq!(weight_tib_from(0, 1u64 << 40), 1.0);
        assert_eq!(weight_tib_from(0, 1u64 << 39), 0.5);
    }

    #[test]
    fn weight_is_zero_when_no_size_is_known() {
        assert_eq!(weight_tib_from(0, 0), 0.0);
    }

    fn recs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn every_new_disk_is_registered_off_whatever_it_holds() {
        let meta = HashMap::from([
            ("dev-sdb".to_string(), disk(Ownership::Blank)),
            ("dev-sdc".to_string(), disk(Ownership::Ours)),
            ("dev-sdd".to_string(), disk(Ownership::Foreign)),
        ]);
        assert_eq!(
            new_disk_records("node1", &meta, &HashMap::new()),
            vec![
                ("node1--dev-sdb".to_string(), "OFF"),
                ("node1--dev-sdc".to_string(), "OFF"),
                ("node1--dev-sdd".to_string(), "OFF"),
            ]
        );
    }

    #[test]
    fn an_existing_record_is_never_overwritten_and_the_system_lv_needs_none() {
        let meta = HashMap::from([
            ("dev-sdb".to_string(), disk(Ownership::Ours)),
            (SYSTEM_OSD_ID.to_string(), disk(Ownership::Blank)),
        ]);
        let desired = recs(&[("node1--dev-sdb", "OFF")]);
        assert!(new_disk_records("node1", &meta, &desired).is_empty());
    }

    #[tokio::test]
    async fn switches_are_read_from_ceph_and_an_unreadable_store_changes_nothing() {
        let host = FakeHost::new().ok(
            "ceph config-key dump yolab/disks/",
            r#"{"yolab/disks/node1--dev-sdb":"ON"}"#,
        );
        let desired = read_desired(&host).await.expect("readable");
        assert_eq!(
            desired.get("node1--dev-sdb").map(String::as_str),
            Some("ON")
        );

        let down = FakeHost::new().fail("ceph config-key dump", "error connecting to the cluster");
        assert_eq!(read_desired(&down).await, None);
    }

    #[tokio::test]
    async fn a_node_publishes_its_inventory_under_its_own_key() {
        let host = FakeHost::new().ok("ceph config-key set yolab/disk-status/node1", "");
        let meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Blank))]);
        write_status(&host, "node1", &meta).await;
        assert!(host.ran("ceph config-key set yolab/disk-status/node1 {\"disks\":{\"dev-sdb\":"));
    }

    #[tokio::test]
    async fn an_unchanged_inventory_is_not_published_again() {
        let meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Blank))]);
        let first = FakeHost::new().ok("ceph config-key set yolab/disk-status/node-once", "");
        write_status(&first, "node-once", &meta).await;
        assert!(first.ran("ceph config-key set yolab/disk-status/node-once"));

        let second = FakeHost::new();
        write_status(&second, "node-once", &meta).await;
        assert!(
            second.calls().is_empty(),
            "nothing changed, so nothing should be written: {:?}",
            second.calls()
        );
    }

    #[tokio::test]
    async fn registering_reports_how_many_disks_were_new() {
        let host = FakeHost::new().ok("ceph config-key set yolab/disks/node1--dev-sdb", "");
        let meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Blank))]);
        let registered = auto_register_all_disks(&host, "node1", &meta, &HashMap::new()).await;
        assert_eq!(registered, 1);
        assert!(host.ran("ceph config-key set yolab/disks/node1--dev-sdb"));
    }

    fn disk(ownership: Ownership) -> Disk {
        Disk {
            device: "sdb".into(),
            model: String::new(),
            size_bytes: 1_000_000_000,
            is_loop: false,
            ownership,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        }
    }

    fn seen(crush_weight: f64, reweight: f64, kb: u64, up: bool) -> OsdState {
        OsdState {
            crush_weight,
            reweight,
            kb,
            up,
        }
    }

    #[test]
    fn a_freshly_created_osd_is_activated_with_a_weight() {
        let steer = decide_steer(
            true,
            1u64 << 40,
            seen(0.0, 0.0, 0, true),
            0,
            &[],
            "osd",
            None,
        );
        assert_eq!(steer.set_weight, Some(1.0));
        assert!(steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(Phase::Active));
    }

    #[test]
    fn an_on_osd_that_is_down_reports_retrying() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, false), 0, &[], "osd", None);
        assert_eq!(steer.set_weight, None);
        assert!(!steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(Phase::Retrying));
    }

    #[test]
    fn a_weighted_in_osd_that_is_up_needs_no_steering() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        assert_eq!(steer.set_weight, None);
        assert!(!steer.mark_in);
        assert!(!steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(Phase::Active));
    }

    #[test]
    fn an_off_disk_still_in_is_marked_out() {
        let steer = decide_steer(false, 0, seen(1.0, 1.0, 0, false), 0, &[], "osd", None);
        assert!(steer.mark_out);
        assert!(!steer.needs_purge);
        assert_eq!(steer.phase, Some(Phase::Draining));
        assert!(steer.message.is_some());
    }

    #[test]
    fn an_off_disk_already_out_needs_purge() {
        let steer = decide_steer(false, 0, seen(1.0, 0.0, 0, false), 0, &[], "osd", None);
        assert!(steer.needs_purge);
        assert!(!steer.mark_out);
        assert!(!steer.mark_in);
        assert_eq!(steer.phase, None);
        assert_eq!(steer.message, None);
    }

    #[test]
    fn a_unit_is_stopped_only_in_a_definitely_dead_state() {
        assert!(is_stopped(Some("inactive")));
        assert!(is_stopped(Some("failed")));
        assert!(!is_stopped(Some("active")));
        assert!(!is_stopped(Some("activating")));
        assert!(!is_stopped(None));
    }

    #[test]
    fn a_unit_is_running_when_active_or_coming_up() {
        assert!(is_running(Some("active")));
        assert!(is_running(Some("activating")));
        assert!(is_running(Some("reloading")));
        assert!(!is_running(Some("inactive")));
        assert!(!is_running(None));
    }

    #[test]
    fn a_successful_steer_reports_its_own_phase() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        let (phase, _) = steer_report(&steer, true);
        assert_eq!(phase, Phase::Active);
    }

    #[test]
    fn a_failed_steer_reports_retrying_not_active() {
        let steer = decide_steer(true, 0, seen(1.0, 1.0, 0, true), 0, &[], "osd", None);
        let (phase, message) = steer_report(&steer, false);
        assert_eq!(phase, Phase::Retrying);
        assert!(message.contains("keep trying"));
    }

    #[test]
    fn a_wanted_disk_absent_from_the_node_is_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "ON".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_node_scoped_wanted_disk_absent_is_unplugged() {
        let desired = HashMap::from([("node1--dev-sdb".to_string(), "ON".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_present_disk_is_not_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "ON".to_string())]);
        let meta = HashMap::from([("serial-abc".to_string(), disk(Ownership::Blank))]);
        assert!(!any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn an_off_disk_is_not_unplugged() {
        let desired = HashMap::from([("serial-abc".to_string(), "OFF".to_string())]);
        let meta: HashMap<String, Disk> = HashMap::new();
        assert!(!any_unplugged_but_wanted(&desired, "node1", &meta));
    }

    #[test]
    fn a_blank_disk_may_be_turned_into_an_osd() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
    }

    #[test]
    fn our_own_osd_is_never_recreated_over() {
        assert!(
            refuse_osd_creation(&disk(Ownership::Ours)).is_some(),
            "a disk carrying our own BlueStore label must never be handed to ceph-volume create"
        );
    }

    #[test]
    fn another_clusters_disk_is_auto_wiped() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Foreign)), None);
    }

    #[test]
    fn a_disk_of_unknown_ownership_is_refused() {
        assert!(refuse_osd_creation(&disk(Ownership::Unknown)).is_some());
    }

    #[test]
    fn only_provably_foreign_disks_are_auto_wiped() {
        for o in [Ownership::Ours, Ownership::Unknown] {
            assert!(
                refuse_osd_creation(&disk(o)).is_some(),
                "{o:?} must never be handed to ceph-volume create"
            );
        }
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
        assert_eq!(refuse_osd_creation(&disk(Ownership::Foreign)), None);
    }

    #[test]
    fn an_entry_without_a_device_is_refused() {
        let d = Disk {
            device: String::new(),
            ..disk(Ownership::Blank)
        };
        assert!(refuse_osd_creation(&d).is_some());
    }

    #[test]
    fn a_blank_but_mounted_disk_is_still_refused() {
        let mounted = Disk {
            mounted: true,
            ..disk(Ownership::Blank)
        };
        let partitioned = Disk {
            has_partitions: true,
            ..disk(Ownership::Blank)
        };
        assert!(refuse_osd_creation(&mounted).is_some());
        assert_eq!(refuse_osd_creation(&partitioned), None);
    }

    #[test]
    fn a_disk_ceph_knows_about_is_marked_as_ours() {
        let mut meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Blank))]);

        mark_known_osds(&mut meta, &HashMap::from([("dev-sdb".to_string(), 1i64)]));

        assert_eq!(meta["dev-sdb"].osd_id, Some(1));
        assert_eq!(
            meta["dev-sdb"].ownership,
            Ownership::Ours,
            "an OSD id from ceph-volume is authoritative over a missing on-disk label"
        );
    }

    #[test]
    fn a_known_osd_is_never_left_marked_foreign() {
        let mut meta = HashMap::from([("dev-sdb".to_string(), disk(Ownership::Foreign))]);
        mark_known_osds(&mut meta, &HashMap::from([("dev-sdb".to_string(), 4i64)]));
        assert_eq!(meta["dev-sdb"].ownership, Ownership::Ours);
    }

    #[test]
    fn disks_ceph_does_not_know_are_left_alone() {
        let mut meta = HashMap::from([("dev-sdc".to_string(), disk(Ownership::Foreign))]);
        mark_known_osds(&mut meta, &HashMap::new());
        assert_eq!(meta["dev-sdc"].ownership, Ownership::Foreign);
        assert_eq!(meta["dev-sdc"].osd_id, None);
    }

    #[test]
    fn an_id_for_an_absent_disk_adds_nothing() {
        let mut meta: HashMap<String, Disk> = HashMap::new();
        mark_known_osds(&mut meta, &HashMap::from([("dev-gone".to_string(), 9i64)]));
        assert!(meta.is_empty());
    }

    #[test]
    fn a_mounted_disk_is_refused_with_a_reason() {
        let d = Disk {
            device: "sda".into(),
            mounted: true,
            ..disk(Ownership::Blank)
        };
        let msg = refuse_osd_creation(&d).expect("a mounted disk must be refused");
        assert!(msg.contains("using this disk"), "{msg}");
    }

    #[test]
    fn a_partitioned_disk_is_wiped_not_refused() {
        let d = Disk {
            has_partitions: true,
            ..disk(Ownership::Blank)
        };
        assert_eq!(refuse_osd_creation(&d), None);
    }

    #[test]
    fn a_blank_unmounted_disk_is_accepted() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Blank)), None);
    }

    #[test]
    fn foreign_ceph_no_longer_blocks() {
        assert_eq!(refuse_osd_creation(&disk(Ownership::Foreign)), None);
        assert!(refuse_osd_creation(&disk(Ownership::Ours)).is_some());
        assert!(refuse_osd_creation(&disk(Ownership::Unknown)).is_some());
    }

    #[test]
    fn a_mounted_disk_is_not_offered_as_storage() {
        let v = json!({"name": "sda", "type": "disk", "children": [
            {"name": "sda1", "type": "part", "mountpoints": ["/boot"]}
        ]});
        assert!(parse_disk_flags(&v).mounted, "must be seen as mounted");
    }

    #[test]
    fn refusal_reasons_carry_no_jargon() {
        let cases = [
            disk(Ownership::Unknown),
            disk(Ownership::Ours),
            Disk {
                mounted: true,
                ..disk(Ownership::Blank)
            },
            Disk {
                device: String::new(),
                ..disk(Ownership::Blank)
            },
        ];
        const JARGON: [&str; 8] = [
            "BlueStore",
            "OSD",
            "ceph",
            "Ceph",
            "LVM",
            "device path",
            "metadata",
            "superblock",
        ];
        for c in cases {
            let msg = refuse_osd_creation(&c).expect("every case must refuse");
            for word in JARGON {
                assert!(
                    !msg.contains(word),
                    "refusal message leaks {word:?} to the user: {msg}"
                );
            }
            assert!(
                msg.ends_with('.'),
                "shown as a sentence to a person, so it needs to read as one: {msg}"
            );
        }
    }

    fn meta_json() -> serde_json::Value {
        json!([
            {"id": 0, "hostname": "node1", "devices": "sda",
             "bluestore_bdev_dev_node": "/dev/dm-1"},
            {"id": 1, "hostname": "node1", "devices": "sdb",
             "bluestore_bdev_dev_node": "/dev/dm-2"},
            {"id": 2, "hostname": "node3", "devices": "sda",
             "bluestore_bdev_dev_node": "/dev/dm-9"}
        ])
    }

    #[test]
    fn osd_metadata_returns_only_this_hosts_osds() {
        let got = parse_osd_metadata(&meta_json(), "node1");
        let ids: Vec<i64> = got.iter().map(|(_, id)| *id).collect();
        assert!(ids.contains(&0) && ids.contains(&1));
        assert!(!ids.contains(&2), "osd.2 belongs to another machine");
    }

    #[test]
    fn osd_metadata_reports_the_device_and_the_volume() {
        let got = parse_osd_metadata(&meta_json(), "node1");
        assert!(got.contains(&("sda".to_string(), 0)));
        assert!(got.contains(&("/dev/dm-1".to_string(), 0)));
    }

    #[test]
    fn osd_metadata_splits_a_multi_device_osd() {
        let v = json!([{"id": 4, "hostname": "n", "devices": "sdb,sdc"}]);
        let got = parse_osd_metadata(&v, "n");
        assert!(got.contains(&("sdb".to_string(), 4)));
        assert!(got.contains(&("sdc".to_string(), 4)));
    }

    #[test]
    fn osd_metadata_ignores_placeholder_device_nodes() {
        let v = json!([{"id": 5, "hostname": "n", "devices": "",
                        "bluestore_bdev_dev_node": "unknown"}]);
        assert!(parse_osd_metadata(&v, "n").is_empty());
    }

    #[test]
    fn osd_metadata_yields_nothing_from_a_shape_it_does_not_understand() {
        assert!(parse_osd_metadata(&json!({}), "n").is_empty());
        assert!(parse_osd_metadata(&json!([]), "n").is_empty());
        assert!(parse_osd_metadata(&json!([{"hostname": "n"}]), "n").is_empty());
    }

    fn osd(id: i64, up: bool, reweight: f64) -> Value {
        json!({"id": id, "type": "osd",
               "status": if up { "up" } else { "down" }, "reweight": reweight})
    }
    fn host(name: &str, children: Vec<i64>) -> Value {
        json!({"type": "host", "name": name, "children": children})
    }

    #[test]
    fn other_usable_osds_are_counted_for_the_osd_domain() {
        let nodes = vec![osd(0, true, 1.0), osd(1, true, 0.0), osd(2, true, 1.0)];
        assert_eq!(drain_targets_remaining(&nodes, 1, "osd"), 2);
    }

    #[test]
    fn down_and_out_osds_are_not_places_to_put_a_copy() {
        let nodes = vec![osd(0, false, 1.0), osd(1, true, 1.0), osd(2, true, 0.0)];
        assert_eq!(drain_targets_remaining(&nodes, 1, "osd"), 0);
    }

    #[test]
    fn the_host_domain_counts_machines_not_disks() {
        let nodes = vec![
            host("node1", vec![0, 1]),
            osd(0, true, 1.0),
            osd(1, true, 1.0),
            host("node2", vec![2]),
            osd(2, true, 1.0),
        ];
        assert_eq!(
            drain_targets_remaining(&nodes, 0, "host"),
            2,
            "node1 still has osd.1"
        );
        assert_eq!(drain_targets_remaining(&nodes, 2, "host"), 1);
    }

    #[test]
    fn a_drain_with_nowhere_to_go_says_so_and_says_what_to_do() {
        let m = drain_message(2, Some(3));
        assert!(m.contains("cannot be emptied"), "{m}");
        assert!(m.contains("Lower the number of copies to 2"), "{m}");
        assert!(m.contains("add another disk"), "{m}");
        assert!(
            !m.contains("until this finishes"),
            "must not promise completion it cannot deliver: {m}"
        );
    }

    #[test]
    fn a_drain_that_can_finish_says_do_not_unplug() {
        let m = drain_message(3, Some(2));
        assert!(m.contains("Do not unplug"), "{m}");
        assert!(!m.contains("cannot be emptied"), "{m}");
    }

    #[test]
    fn no_other_disk_at_all_is_its_own_message() {
        let m = drain_message(0, Some(1));
        assert!(m.contains("no other disk"), "{m}");
    }

    #[test]
    fn an_unknown_copy_count_promises_nothing_it_cannot_check() {
        let m = drain_message(2, None);
        assert!(!m.contains("cannot be emptied"), "{m}");
        assert!(!m.contains("until this finishes"), "{m}");
    }

    #[test]
    fn an_unreachable_cluster_does_nothing() {
        assert_eq!(plan_tick(false, true), TickPlan::Unreachable);
    }

    #[test]
    fn an_unreadable_osd_map_does_nothing() {
        assert_eq!(plan_tick(true, false), TickPlan::UnknownOsdMap);
    }

    #[test]
    fn an_unreachable_cluster_is_reported_before_an_unreadable_map() {
        assert_eq!(plan_tick(false, false), TickPlan::Unreachable);
    }

    #[test]
    fn a_reachable_cluster_with_a_readable_map_proceeds() {
        assert_eq!(plan_tick(true, true), TickPlan::Proceed);
    }

    fn osds(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn blank_disk(device: &str) -> Disk {
        Disk {
            device: device.into(),
            model: String::new(),
            size_bytes: 1_000_000_000,
            is_loop: false,
            ownership: Ownership::Blank,
            has_partitions: false,
            mounted: false,
            osd_id: None,
            progress: None,
        }
    }

    #[test]
    fn a_disk_switched_on_with_no_osd_is_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/sdb".to_string()
            }
        );
    }

    #[test]
    fn a_disk_switched_off_is_never_created() {
        for state in ["OFF", "", "on", "true", "1"] {
            let plan = plan_create(
                "node1",
                "dev-sdb",
                &blank_disk("sdb"),
                &recs(&[("node1--dev-sdb", state)]),
                &osds(&[]),
                false,
                0,
                None,
            );
            assert_eq!(plan, CreatePlan::Skip, "state {state:?} must not create");
        }
    }

    #[test]
    fn a_disk_with_no_record_at_all_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    #[test]
    fn another_nodes_record_never_creates_on_this_node() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node2--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    #[test]
    fn a_disk_that_already_has_an_osd_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[("dev-sdb", 3)]),
            false,
            0,
            None,
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    #[test]
    fn a_create_already_in_flight_is_not_started_again() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk("sdb"),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            true,
            0,
            None,
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    #[test]
    fn an_in_flight_create_wins_over_a_refusal() {
        let mounted = Disk {
            mounted: true,
            ..blank_disk("sdb")
        };
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &mounted,
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            true,
            0,
            None,
        );
        assert_eq!(plan, CreatePlan::Skip);
    }

    #[test]
    fn a_mounted_disk_switched_on_is_blocked_not_created() {
        let plan = plan_create(
            "node1",
            "dev-sda",
            &Disk {
                mounted: true,
                ..blank_disk("sda")
            },
            &recs(&[("node1--dev-sda", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert!(matches!(plan, CreatePlan::Blocked(_)), "{plan:?}");
    }

    #[test]
    fn a_partitioned_disk_switched_on_is_created_not_blocked() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &Disk {
                has_partitions: true,
                ..blank_disk("sdb")
            },
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert!(matches!(plan, CreatePlan::Create { .. }), "{plan:?}");
    }

    #[test]
    fn a_foreign_cluster_disk_switched_on_is_created_not_blocked() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &Disk {
                ownership: Ownership::Foreign,
                ..blank_disk("sdb")
            },
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert!(matches!(plan, CreatePlan::Create { .. }), "{plan:?}");
    }

    #[test]
    fn a_disk_without_a_usable_device_name_is_never_created() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk(""),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert!(!matches!(plan, CreatePlan::Create { .. }), "{plan:?}");
    }

    #[test]
    fn a_disk_that_vanished_says_so() {
        let plan = plan_create(
            "node1",
            "dev-sdb",
            &blank_disk(""),
            &recs(&[("node1--dev-sdb", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        let CreatePlan::Blocked(reason) = plan else {
            panic!("expected a reason, got {plan:?}");
        };
        assert!(reason.contains("disappeared"), "{reason}");
    }

    #[test]
    fn a_bare_device_name_is_rooted_under_dev() {
        let plan = plan_create(
            "node1",
            "d",
            &blank_disk("nvme0n1"),
            &recs(&[("node1--d", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/nvme0n1".to_string()
            }
        );
    }

    #[test]
    fn an_absolute_device_path_is_left_alone() {
        let plan = plan_create(
            "node1",
            "d",
            &blank_disk("/dev/disk/by-id/wwn-0x5000"),
            &recs(&[("node1--d", "ON")]),
            &osds(&[]),
            false,
            0,
            None,
        );
        assert_eq!(
            plan,
            CreatePlan::Create {
                dev_path: "/dev/disk/by-id/wwn-0x5000".to_string()
            }
        );
    }

    #[test]
    fn an_absent_record_is_off() {
        assert!(!wants_on(&recs(&[]), "node1", "dev-sdb"));
    }

    #[test]
    fn only_on_is_on() {
        assert!(wants_on(&recs(&[("node1--d", "ON")]), "node1", "d"));
        for off in ["OFF", "off", "On", "USING", "", "REMOVING"] {
            assert!(
                !wants_on(&recs(&[("node1--d", off)]), "node1", "d"),
                "{off:?} must not read as on"
            );
        }
    }

    #[test]
    fn a_hardware_id_record_is_found_without_a_node_prefix() {
        let id = "serial-wwn-0x50014ee214caf529";
        assert!(wants_on(&recs(&[(id, "ON")]), "node1", id));
        assert!(wants_on(&recs(&[(id, "ON")]), "node2", id));
    }

    use crate::routers::ceph::PgLoss;

    fn loss(stuck: u32, total: u32, unrecoverable: bool) -> PgLoss {
        PgLoss {
            stuck,
            total,
            unrecoverable,
            unrecoverable_pools: vec![],
            confirmed_lost: false,
            confirmed_lost_pools: vec![],
        }
    }

    #[test]
    fn a_drain_still_in_progress_waits() {
        assert_eq!(plan_purge(false, false, None), PurgeVerdict::Wait);
    }

    #[test]
    fn an_unsafe_osd_waits_whatever_else_is_true() {
        assert_eq!(
            plan_purge(false, true, Some(&loss(0, 100, false))),
            PurgeVerdict::Wait
        );
    }

    #[test]
    fn an_osd_that_stops_being_safe_after_the_daemon_stops_is_left_alone() {
        assert_eq!(plan_purge(true, false, None), PurgeVerdict::Recheck);
    }

    #[test]
    fn a_cluster_with_unrecoverable_stuck_pgs_refuses_to_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(63, 200, true))),
            PurgeVerdict::RefuseDataAtRisk
        );
    }

    #[test]
    fn stuck_pgs_that_can_still_be_rebuilt_do_not_block_a_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(63, 200, false))),
            PurgeVerdict::Purge
        );
    }

    #[test]
    fn a_single_copy_pool_with_nothing_stuck_does_not_block_a_purge() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(0, 200, true))),
            PurgeVerdict::Purge
        );
    }

    #[test]
    fn a_healthy_cluster_purges() {
        assert_eq!(
            plan_purge(true, true, Some(&loss(0, 200, false))),
            PurgeVerdict::Purge
        );
        assert_eq!(plan_purge(true, true, None), PurgeVerdict::Purge);
    }

    #[test]
    fn nothing_but_a_clear_yes_ever_reaches_purge() {
        for before in [false, true] {
            for after in [false, true] {
                for l in [
                    None,
                    Some(loss(0, 10, false)),
                    Some(loss(5, 10, false)),
                    Some(loss(0, 10, true)),
                    Some(loss(5, 10, true)),
                ] {
                    let at_risk = l.as_ref().is_some_and(|x| x.unrecoverable && x.stuck > 0);
                    let verdict = plan_purge(before, after, l.as_ref());
                    let should_purge = before && after && !at_risk;
                    assert_eq!(
                        verdict == PurgeVerdict::Purge,
                        should_purge,
                        "before={before} after={after} at_risk={at_risk} gave {verdict:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn phases_keep_the_names_the_ui_reads() {
        let wire: Vec<&str> = [
            Phase::Unset,
            Phase::Active,
            Phase::Creating,
            Phase::Retrying,
            Phase::Blocked,
            Phase::Draining,
            Phase::Removing,
            Phase::Removable,
            Phase::Unknown,
        ]
        .iter()
        .map(|p| p.as_str())
        .collect();
        assert_eq!(
            wire,
            [
                "",
                "active",
                "creating",
                "retrying",
                "blocked",
                "draining",
                "removing",
                "removable",
                "unknown"
            ]
        );
        assert_eq!(Phase::default(), Phase::Unset);
    }

    fn our_fsid_host() -> FakeHost {
        FakeHost::new().ok("ceph fsid", &format!(r#"{{"fsid":"{OURS}"}}"#))
    }

    fn listed_on(dev: &str, id: i64) -> String {
        json!({ id.to_string(): [{"devices": [dev], "tags": {"ceph.cluster_fsid": OURS}}] })
            .to_string()
    }

    fn not_yet(a: crate::storage::wait::Attempt<()>) -> String {
        match a {
            crate::storage::wait::Attempt::NotYet(why) => why,
            crate::storage::wait::Attempt::Ready(()) => panic!("expected NotYet"),
        }
    }

    #[tokio::test]
    async fn a_machine_without_the_system_lv_is_an_error_not_a_skip() {
        let host = FakeHost::new();
        assert!(lv_osd_attempt(&host, "/nonexistent/pool-ceph")
            .await
            .is_err());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = FakeHost::new().fail("ceph fsid", "error connecting to the cluster");
        let why = not_yet(lv_osd_attempt(&host, &dev).await.unwrap());
        assert!(why.contains("cluster's id"), "{why}");
        assert!(!host.ran("lvm create"));
    }

    #[tokio::test]
    async fn an_existing_system_osd_is_started_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = our_fsid_host()
            .ok("ceph-volume lvm list", &listed_on(&dev, 0))
            .ok("ceph osd ls", "[0, 1]")
            .ok("systemctl start yolab-ceph-osd@0.service", "");

        let ready = lv_osd_attempt(&host, &dev).await.unwrap();

        assert_eq!(ready, crate::storage::wait::Attempt::Ready(()));
        assert!(host.ran("systemctl start yolab-ceph-osd@0.service"));
        assert!(!host.ran("lvm create") && !host.ran("zap"));
    }

    #[tokio::test]
    async fn a_system_osd_the_cluster_forgot_is_erased_and_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = our_fsid_host()
            .ok("ceph-volume lvm list", &listed_on(&dev, 3))
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm zap", "")
            .ok("ceph-volume lvm list", "{}")
            .ok("ceph-volume lvm list", &listed_on(&dev, 0))
            .ok("wipefs --all", "")
            .ok("ceph-volume lvm create", "")
            .ok("systemctl start yolab-ceph-osd@0.service", "");

        let ready = lv_osd_attempt(&host, &dev).await.unwrap();

        assert_eq!(ready, crate::storage::wait::Attempt::Ready(()));
        assert!(host.ran(&format!("ceph-volume lvm zap {dev}")));
        assert!(!host.ran("--destroy"));
        assert!(
            !host.ran("yolab-ceph-osd@3"),
            "a forgotten OSD is never started"
        );
        assert!(host.position("lvm zap") < host.position("lvm create"));
    }

    #[tokio::test]
    async fn a_system_osd_is_never_erased_when_the_cluster_cannot_say() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = our_fsid_host()
            .ok("ceph-volume lvm list", &listed_on(&dev, 3))
            .fail("ceph osd ls", "timed out");

        let attempt = lv_osd_attempt(&host, &dev).await.unwrap();

        assert!(not_yet(attempt).contains("osd.3"));
        assert!(!host.ran("zap") && !host.ran("systemctl"));
    }

    #[tokio::test]
    async fn a_blank_system_lv_becomes_an_osd() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = our_fsid_host()
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .ok("ceph-volume lvm list", "{}")
            .ok("ceph-volume lvm list", &listed_on(&dev, 0))
            .ok("wipefs --all", "")
            .ok("ceph-volume lvm create", "")
            .ok("systemctl start yolab-ceph-osd@0.service", "");

        let ready = lv_osd_attempt(&host, &dev).await.unwrap();

        assert_eq!(ready, crate::storage::wait::Attempt::Ready(()));
        assert!(host.ran(&format!("ceph-volume lvm create --bluestore --data {dev}")));
    }

    #[tokio::test]
    async fn a_failed_create_is_waited_on_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &vec![0u8; 4096]);
        let host = our_fsid_host()
            .ok("ceph osd ls", "[]")
            .ok("ceph-volume lvm list", "{}")
            .ok("wipefs --all", "")
            .fail(
                "ceph-volume lvm create",
                "RuntimeError: Unable to create a new OSD id",
            );

        let why = not_yet(lv_osd_attempt(&host, &dev).await.unwrap());

        assert!(why.contains("did not succeed"), "{why}");
    }

    #[tokio::test]
    async fn our_own_label_without_an_osd_is_never_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fake_device(&dir, "pool-ceph", &bluestore_label(OURS));
        let host = our_fsid_host().ok("ceph-volume lvm list", "{}");

        let why = not_yet(lv_osd_attempt(&host, &dev).await.unwrap());

        assert!(why.contains("already holds your files"), "{why}");
        assert!(!host.ran("lvm create") && !host.ran("zap") && !host.ran("wipefs"));
    }

    #[test]
    fn the_system_disk_cannot_be_switched_off_by_a_record() {
        let desired = HashMap::from([(record_key("node1", SYSTEM_OSD_ID), "OFF".to_string())]);
        assert!(wants_on(&desired, "node1", SYSTEM_OSD_ID));
        assert!(wants_on(&HashMap::new(), "node1", SYSTEM_OSD_ID));
        assert!(!wants_on(&HashMap::new(), "node1", "dev-sdb"));
    }
}
