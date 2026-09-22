use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::write_private_file;
use crate::host::Host;
use crate::storage::reset_wipe;

const REBUILD_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub root: PathBuf,
    pub machine_dir: PathBuf,
    pub flake: String,
    pub flake_target: String,
}

impl Layout {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            root: PathBuf::from("/"),
            machine_dir: PathBuf::from(&cfg.machine_dir),
            flake: cfg.flake_ref(),
            flake_target: cfg.flake_target.clone(),
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("var/lib/yolab/reset/state.json")
    }
    fn config_before(&self) -> PathBuf {
        self.root.join("var/lib/yolab/reset/config.toml.before")
    }
    fn system_before(&self) -> PathBuf {
        self.root.join("var/lib/yolab/reset/system.before")
    }
    fn config(&self) -> PathBuf {
        self.machine_dir.join("config.toml")
    }
    fn system_profile(&self) -> PathBuf {
        self.root.join("nix/var/nix/profiles/system")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrepareRequest {
    pub heal_id: String,
    pub driver: String,
    pub fsid: String,
    pub server_addr: String,
}

impl PrepareRequest {
    fn validate(&self) -> Result<()> {
        if self.heal_id.is_empty() || !self.heal_id.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("heal id {:?} is not hexadecimal", self.heal_id);
        }
        if self.driver.is_empty() {
            bail!("a heal without a driving machine");
        }
        if !is_uuid(&self.fsid) {
            bail!("fsid {:?} is not a UUID", self.fsid);
        }
        if !self.server_addr.is_empty() && join_addr(&self.server_addr).is_none() {
            bail!(
                "server address {:?} is not https://[<ipv6>]:6443",
                self.server_addr
            );
        }
        Ok(())
    }
}

fn join_addr(server_addr: &str) -> Option<Ipv6Addr> {
    server_addr
        .strip_prefix("https://[")?
        .strip_suffix("]:6443")?
        .parse()
        .ok()
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    Preparing,
    Prepared,
    Failed { error: String },
    Armed { boot_id: String },
    Undone,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Reset {
    #[serde(flatten)]
    request: PrepareRequest,
    #[serde(flatten)]
    phase: Phase,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PhaseView {
    Preparing,
    Prepared,
    Failed,
    Armed,
    Restarted,
    Undone,
}

impl PhaseView {
    pub fn active(self) -> bool {
        matches!(
            self,
            PhaseView::Preparing | PhaseView::Prepared | PhaseView::Armed
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResetView {
    pub heal_id: String,
    pub driver: String,
    pub phase: PhaseView,
    pub error: Option<String>,
}

fn view(reset: &Reset, boot_id: &str, preparing: Option<&str>) -> ResetView {
    let (phase, error) = match &reset.phase {
        Phase::Preparing if preparing == Some(reset.request.heal_id.as_str()) => {
            (PhaseView::Preparing, None)
        }
        Phase::Preparing => (
            PhaseView::Failed,
            Some("preparing was interrupted".to_string()),
        ),
        Phase::Prepared => (PhaseView::Prepared, None),
        Phase::Failed { error } => (PhaseView::Failed, Some(error.clone())),
        Phase::Armed { boot_id: at } if at == boot_id => (PhaseView::Armed, None),
        Phase::Armed { .. } => (PhaseView::Restarted, None),
        Phase::Undone => (PhaseView::Undone, None),
    };
    ResetView {
        heal_id: reset.request.heal_id.clone(),
        driver: reset.request.driver.clone(),
        phase,
        error,
    }
}

fn load(layout: &Layout) -> Result<Option<Reset>> {
    let path = layout.state();
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .with_context(|| format!("{} is unreadable", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save(layout: &Layout, request: &PrepareRequest, phase: Phase) -> Result<()> {
    let reset = Reset {
        request: request.clone(),
        phase,
    };
    write_private_file(&layout.state(), &serde_json::to_vec_pretty(&reset)?)
}

#[derive(Clone, Default)]
pub(crate) struct Preparing(Arc<Mutex<Option<String>>>);

impl Preparing {
    pub fn global() -> &'static Preparing {
        static PREPARING: OnceLock<Preparing> = OnceLock::new();
        PREPARING.get_or_init(Preparing::default)
    }
    fn running(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set(&self, heal_id: Option<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = heal_id;
    }
}

pub(crate) fn lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn current(
    layout: &Layout,
    boot_id: &str,
    preparing: &Preparing,
) -> Result<Option<ResetView>> {
    Ok(load(layout)?.map(|r| view(&r, boot_id, preparing.running().as_deref())))
}

#[derive(Debug, PartialEq)]
pub(crate) enum Begin {
    Already(ResetView),
    Start,
}

pub(crate) async fn begin_prepare<H: Host>(
    host: &H,
    layout: &Layout,
    request: &PrepareRequest,
    boot_id: &str,
    preparing: &Preparing,
) -> Result<Begin> {
    request.validate()?;
    if let Some(reset) = load(layout)? {
        let v = view(&reset, boot_id, preparing.running().as_deref());
        if preparing.running().is_some() {
            if reset.request.heal_id == request.heal_id {
                return Ok(Begin::Already(v));
            }
            bail!(
                "heal {} is being prepared on this machine",
                reset.request.heal_id
            );
        }
        if reset.request.heal_id == request.heal_id {
            if reset.request != *request {
                bail!("heal {} asked for something else before", request.heal_id);
            }
            if v.phase != PhaseView::Failed {
                return Ok(Begin::Already(v));
            }
        } else if matches!(
            v.phase,
            PhaseView::Prepared | PhaseView::Armed | PhaseView::Failed
        ) {
            tracing::warn!(
                "heal {}: putting back heal {} from {} first",
                request.heal_id,
                reset.request.heal_id,
                reset.request.driver
            );
            put_back(host, layout).await?;
        }
    }
    save(layout, request, Phase::Preparing)?;
    preparing.set(Some(request.heal_id.clone()));
    Ok(Begin::Start)
}

pub(crate) async fn prepare<H: Host>(
    host: &H,
    layout: &Layout,
    request: &PrepareRequest,
    preparing: &Preparing,
) {
    let outcome = rewrite_and_rebuild(host, layout, request).await;
    let _held = lock().lock().await;
    preparing.set(None);
    let phase = match outcome {
        Ok(()) => {
            tracing::warn!("heal {}: prepared", request.heal_id);
            Phase::Prepared
        }
        Err(e) => {
            tracing::error!("heal {}: preparing failed: {e:#}", request.heal_id);
            Phase::Failed {
                error: format!("{e:#}"),
            }
        }
    };
    if let Err(e) = save(layout, request, phase) {
        tracing::error!("heal {}: record the outcome: {e:#}", request.heal_id);
    }
}

async fn rewrite_and_rebuild<H: Host>(
    host: &H,
    layout: &Layout,
    request: &PrepareRequest,
) -> Result<()> {
    if !layout.config_before().exists() {
        let current = std::fs::read(layout.config())
            .with_context(|| format!("read {}", layout.config().display()))?;
        match std::fs::read_link(layout.system_profile()) {
            Ok(generation) => write_private_file(
                &layout.system_before(),
                generation.to_string_lossy().as_bytes(),
            )?,
            Err(e) => tracing::warn!("heal: read the system profile ({e}) — an undo will rebuild"),
        }
        write_private_file(&layout.config_before(), &current)?;
    }
    let before =
        std::fs::read_to_string(layout.config_before()).context("read the copy of config.toml")?;
    let new = rewrite_config(&before, &request.server_addr, &request.fsid)?;
    write_private_file(&layout.config(), new.as_bytes())?;
    if let Err(e) = rebuild(host, layout).await {
        if let Err(restore) = write_private_file(&layout.config(), before.as_bytes()) {
            tracing::error!("heal: put config.toml back: {restore:#}");
        }
        return Err(e);
    }
    Ok(())
}

pub(crate) fn rewrite_config(config: &str, server_addr: &str, fsid: &str) -> Result<String> {
    let mut table: toml::Table = toml::from_str(config).context("config.toml is not TOML")?;
    let k3s = table
        .get_mut("node")
        .and_then(|n| n.as_table_mut())
        .and_then(|n| n.get_mut("k3s"))
        .and_then(|k| k.as_table_mut())
        .context("config.toml has no [node.k3s]")?;
    k3s.insert("server_addr".into(), server_addr.into());
    let ceph = table
        .entry("ceph")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("[ceph] in config.toml is not a table")?;
    ceph.insert("fsid".into(), fsid.into());
    let text = toml::to_string(&table).context("write config.toml")?;
    reset_wipe::with_wipe_condition(&text, false)
}

async fn rebuild<H: Host>(host: &H, layout: &Layout) -> Result<()> {
    host.systemctl(&[
        "reset-failed",
        "nixos-rebuild-switch-to-configuration.service",
    ])
    .await
    .ok();
    let flake = format!("{}#{}", layout.flake, layout.flake_target);
    let input = format!("path:{}", layout.machine_dir.display());
    let out = host
        .run_cmd_bounded(
            "nixos-rebuild",
            &[
                "boot",
                "--flake",
                &flake,
                "--override-input",
                "yolab-machine",
                &input,
                "--no-write-lock-file",
                "--accept-flake-config",
            ],
            REBUILD_TIMEOUT,
        )
        .await?;
    if !out.success {
        bail!("nixos-rebuild boot failed: {}", tail(&out.stderr, 20));
    }
    Ok(())
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub(crate) fn holds_config(layout: &Layout) -> bool {
    layout.config_before().exists()
}

pub(crate) fn arm(layout: &Layout, heal_id: &str, boot_id: &str) -> Result<()> {
    let Some(reset) = load(layout)?.filter(|r| r.request.heal_id == heal_id) else {
        bail!("this machine was not prepared for heal {heal_id}");
    };
    match &reset.phase {
        Phase::Armed { boot_id: at } if at == boot_id => return Ok(()),
        Phase::Prepared => {}
        other => bail!("heal {heal_id} cannot be armed here: it is {other:?}"),
    }
    let config = std::fs::read_to_string(layout.config())
        .with_context(|| format!("read {}", layout.config().display()))?;
    let armed = reset_wipe::with_wipe_condition(&config, true)?;
    write_private_file(&layout.config(), armed.as_bytes())?;
    save(
        layout,
        &reset.request,
        Phase::Armed {
            boot_id: boot_id.to_string(),
        },
    )?;
    tracing::warn!("heal {heal_id}: this machine wipes itself at its next boot");
    Ok(())
}

pub(crate) async fn undo<H: Host>(
    host: &H,
    layout: &Layout,
    heal_id: &str,
    boot_id: &str,
    preparing: &Preparing,
    may_rebuild: bool,
) -> Result<()> {
    let Some(reset) = load(layout)?.filter(|r| r.request.heal_id == heal_id) else {
        return Ok(());
    };
    match view(&reset, boot_id, preparing.running().as_deref()).phase {
        PhaseView::Undone => return Ok(()),
        PhaseView::Restarted => bail!("this machine already restarted into the new cluster"),
        PhaseView::Preparing => {
            bail!("this machine is still preparing — try again when it is done")
        }
        PhaseView::Prepared | PhaseView::Failed | PhaseView::Armed => {}
    }
    if !may_rebuild {
        bail!("an update is rebuilding this machine's system — try again");
    }
    put_back(host, layout).await?;
    save(layout, &reset.request, Phase::Undone)?;
    tracing::warn!("heal {heal_id}: undone on this machine");
    Ok(())
}

async fn put_back<H: Host>(host: &H, layout: &Layout) -> Result<()> {
    let before = match std::fs::read(layout.config_before()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("read the copy of config.toml"),
    };
    write_private_file(&layout.config(), &before)?;
    if boot_entry_unchanged(layout) {
        tracing::info!("heal: the boot entry never changed — no rebuild needed");
    } else {
        rebuild(host, layout).await?;
    }
    for copy in [layout.system_before(), layout.config_before()] {
        match std::fs::remove_file(&copy) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", copy.display())),
        }
    }
    Ok(())
}

fn boot_entry_unchanged(layout: &Layout) -> bool {
    match (
        std::fs::read_to_string(layout.system_before()),
        std::fs::read_link(layout.system_profile()),
    ) {
        (Ok(before), Ok(now)) => now.as_path() == Path::new(&before),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const FSID: &str = "0a1b2c3d-1111-4222-8333-444455556666";
    const REBUILD: &str = "nixos-rebuild boot --flake github:DemyCode/yolab/main#yolab";
    const ORIGINAL: &str = "[node]\nsub_ipv6_private = \"fd00::2\"\n\n[node.k3s]\ntoken = \"t\"\nserver_addr = \"\"\n\n[ceph]\nfsid = \"old\"\n";

    struct Machine {
        _dir: tempfile::TempDir,
        layout: Layout,
    }

    fn machine() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let machine_dir = root.join("var/lib/yolab/machine");
        std::fs::create_dir_all(&machine_dir).unwrap();
        std::fs::write(machine_dir.join("config.toml"), ORIGINAL).unwrap();
        Machine {
            layout: Layout {
                root,
                machine_dir,
                flake: "github:DemyCode/yolab/main".into(),
                flake_target: "yolab".into(),
            },
            _dir: dir,
        }
    }

    fn request(heal_id: &str) -> PrepareRequest {
        PrepareRequest {
            heal_id: heal_id.into(),
            driver: "node1".into(),
            fsid: FSID.into(),
            server_addr: "https://[fd00::1]:6443".into(),
        }
    }

    fn host() -> FakeHost {
        FakeHost::new()
            .ok("systemctl reset-failed", "")
            .ok("nixos-rebuild boot", "")
    }

    fn config(m: &Machine) -> String {
        std::fs::read_to_string(m.layout.config()).unwrap()
    }

    fn armed(m: &Machine) -> bool {
        reset_wipe::wipe_condition(&config(m)).unwrap()
    }

    fn phase(m: &Machine, boot: &str) -> PhaseView {
        current(&m.layout, boot, &Preparing::default())
            .unwrap()
            .unwrap()
            .phase
    }

    async fn prepared(m: &Machine, host: &FakeHost, heal_id: &str) {
        let preparing = Preparing::default();
        let req = request(heal_id);
        assert_eq!(
            begin_prepare(host, &m.layout, &req, "boot1", &preparing)
                .await
                .unwrap(),
            Begin::Start
        );
        prepare(host, &m.layout, &req, &preparing).await;
    }

    #[test]
    fn the_config_gets_the_new_cluster_and_keeps_everything_else() {
        let before = "[homelab]\nhostname = \"node2\"\n[node]\nsub_ipv6_private = \"fd00::2\"\nwipe_condition = true\n[node.k3s]\ntoken = \"tok\"\nserver_addr = \"https://[fd00::1]:6443\"\n[ceph]\nfsid = \"old\"\n";
        let after: toml::Table =
            toml::from_str(&rewrite_config(before, "", FSID).unwrap()).unwrap();
        assert_eq!(after["node"]["k3s"]["server_addr"].as_str(), Some(""));
        assert_eq!(after["node"]["k3s"]["token"].as_str(), Some("tok"));
        assert_eq!(after["ceph"]["fsid"].as_str(), Some(FSID));
        assert_eq!(after["homelab"]["hostname"].as_str(), Some("node2"));
        assert_eq!(after["node"]["wipe_condition"].as_bool(), Some(false));

        let without_ceph = "[node.k3s]\ntoken = \"tok\"\n";
        let after: toml::Table =
            toml::from_str(&rewrite_config(without_ceph, "https://[fd00::1]:6443", FSID).unwrap())
                .unwrap();
        assert_eq!(after["ceph"]["fsid"].as_str(), Some(FSID));
        assert!(
            rewrite_config("[homelab]\n", "", FSID).is_err(),
            "no [node.k3s] to set"
        );
    }

    #[test]
    fn a_request_is_checked_before_anything_is_written_from_it() {
        assert!(request("ab12").validate().is_ok());
        let bad = [
            PrepareRequest {
                heal_id: "../x".into(),
                ..request("ab12")
            },
            PrepareRequest {
                fsid: "not-a-uuid".into(),
                ..request("ab12")
            },
            PrepareRequest {
                server_addr: "https://fd00::1:6443".into(),
                ..request("ab12")
            },
            PrepareRequest {
                server_addr: "https://[fd00::1]:6443\"\n[x]".into(),
                ..request("ab12")
            },
            PrepareRequest {
                driver: "".into(),
                ..request("ab12")
            },
        ];
        for r in bad {
            assert!(r.validate().is_err(), "{r:?}");
        }
        assert!(PrepareRequest {
            server_addr: "".into(),
            ..request("ab12")
        }
        .validate()
        .is_ok());
    }

    #[tokio::test]
    async fn preparing_rewrites_the_config_and_rebuilds_the_boot_entry_without_arming() {
        let m = machine();
        let host = host();
        prepared(&m, &host, "ab12").await;

        assert_eq!(phase(&m, "boot1"), PhaseView::Prepared);
        assert!(config(&m).contains(FSID) && config(&m).contains("https://[fd00::1]:6443"));
        assert!(!armed(&m));
        assert_eq!(
            std::fs::read_to_string(m.layout.config_before()).unwrap(),
            ORIGINAL
        );
        assert!(host.ran(&format!(
            "{REBUILD} --override-input yolab-machine path:{}",
            m.layout.machine_dir.display()
        )));
        assert!(
            !host.ran("nixos-rebuild switch"),
            "the running system is left alone"
        );
        assert!(holds_config(&m.layout), "updates wait");
    }

    #[tokio::test]
    async fn a_failed_rebuild_puts_the_config_back_and_reports_why() {
        let m = machine();
        let host = FakeHost::new()
            .ok("systemctl reset-failed", "")
            .fail("nixos-rebuild boot", "error: no space left on device");
        prepared(&m, &host, "ab12").await;

        let v = current(&m.layout, "boot1", &Preparing::default())
            .unwrap()
            .unwrap();
        assert_eq!(v.phase, PhaseView::Failed);
        assert!(v.error.unwrap().contains("no space left"));
        assert_eq!(config(&m), ORIGINAL);
    }

    #[tokio::test]
    async fn asking_again_does_not_prepare_twice_and_an_interrupted_prepare_is_a_failure() {
        let m = machine();
        let host = FakeHost::new();
        let preparing = Preparing::default();
        let req = request("ab12");
        begin_prepare(&host, &m.layout, &req, "boot1", &preparing)
            .await
            .unwrap();
        assert!(matches!(
            begin_prepare(&host, &m.layout, &req, "boot1", &preparing)
                .await
                .unwrap(),
            Begin::Already(ResetView {
                phase: PhaseView::Preparing,
                ..
            })
        ));
        assert!(
            begin_prepare(&host, &m.layout, &request("cd34"), "boot1", &preparing)
                .await
                .is_err(),
            "one heal at a time"
        );
        let v = current(&m.layout, "boot1", &Preparing::default())
            .unwrap()
            .unwrap();
        assert_eq!(v.phase, PhaseView::Failed);
        assert_eq!(v.error.as_deref(), Some("preparing was interrupted"));
    }

    #[tokio::test]
    async fn preparing_again_starts_from_the_original_config() {
        let m = machine();
        let host = host();
        prepared(&m, &host, "ab12").await;
        let other = PrepareRequest {
            fsid: "99999999-1111-4222-8333-444455556666".into(),
            ..request("cd34")
        };
        let preparing = Preparing::default();
        begin_prepare(&host, &m.layout, &other, "boot1", &preparing)
            .await
            .unwrap();
        assert_eq!(config(&m), ORIGINAL);
        assert!(!m.layout.config_before().exists());
        prepare(&host, &m.layout, &other, &preparing).await;
        assert!(config(&m).contains("99999999") && !config(&m).contains(FSID));
        assert_eq!(
            std::fs::read_to_string(m.layout.config_before()).unwrap(),
            ORIGINAL
        );
    }

    #[tokio::test]
    async fn only_a_prepared_machine_is_armed_and_the_flag_is_all_that_changes() {
        let m = machine();
        let host = host();
        assert!(arm(&m.layout, "ab12", "boot1").is_err(), "not prepared");
        prepared(&m, &host, "ab12").await;
        assert!(arm(&m.layout, "cd34", "boot1").is_err(), "another heal");
        let before = config(&m);
        let rebuilds = host.calls().len();

        arm(&m.layout, "ab12", "boot1").unwrap();

        assert!(armed(&m));
        assert_eq!(
            reset_wipe::with_wipe_condition(&config(&m), false).unwrap(),
            reset_wipe::with_wipe_condition(&before, false).unwrap()
        );
        assert_eq!(host.calls().len(), rebuilds, "arming needs no rebuild");
        assert_eq!(phase(&m, "boot1"), PhaseView::Armed);
        assert_eq!(phase(&m, "boot2"), PhaseView::Restarted);
        arm(&m.layout, "ab12", "boot1").unwrap();
    }

    #[tokio::test]
    async fn an_undo_before_the_restart_disarms_and_rebuilds_the_previous_boot_entry() {
        let m = machine();
        let host = host();
        prepared(&m, &host, "ab12").await;
        arm(&m.layout, "ab12", "boot1").unwrap();
        let rebuilds = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("nixos-rebuild"))
            .count();

        undo(
            &host,
            &m.layout,
            "ab12",
            "boot1",
            &Preparing::default(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(config(&m), ORIGINAL);
        assert!(!armed(&m));
        assert!(!m.layout.config_before().exists());
        assert!(!holds_config(&m.layout), "updates may run again");
        let after = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("nixos-rebuild"))
            .count();
        assert_eq!(after, rebuilds + 1);
        assert_eq!(phase(&m, "boot1"), PhaseView::Undone);
        undo(
            &host,
            &m.layout,
            "ab12",
            "boot1",
            &Preparing::default(),
            true,
        )
        .await
        .unwrap();
        undo(
            &host,
            &m.layout,
            "ffff",
            "boot1",
            &Preparing::default(),
            true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_undo_rebuilds_only_when_the_boot_entry_changed() {
        let m = machine();
        let profiles = m.layout.root.join("nix/var/nix/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::os::unix::fs::symlink("system-7-link", profiles.join("system")).unwrap();
        let rebuilds = |h: &FakeHost| {
            h.calls()
                .iter()
                .filter(|c| c.starts_with("nixos-rebuild"))
                .count()
        };

        let broken = FakeHost::new().ok("systemctl reset-failed", "").fail(
            "nixos-rebuild boot",
            "error: flake has no attribute 'yolab'",
        );
        prepared(&m, &broken, "ab12").await;
        assert_eq!(phase(&m, "boot1"), PhaseView::Failed);
        undo(
            &broken,
            &m.layout,
            "ab12",
            "boot1",
            &Preparing::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(rebuilds(&broken), 1, "only the prepare rebuilt");
        assert_eq!(config(&m), ORIGINAL);
        assert!(!holds_config(&m.layout));

        let host = host();
        prepared(&m, &host, "cd34").await;
        std::fs::remove_file(profiles.join("system")).unwrap();
        std::os::unix::fs::symlink("system-8-link", profiles.join("system")).unwrap();
        undo(
            &host,
            &m.layout,
            "cd34",
            "boot1",
            &Preparing::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(rebuilds(&host), 2);
    }

    #[tokio::test]
    async fn an_undo_waits_for_a_prepare_that_still_runs_and_for_updates() {
        let m = machine();
        let host = host();
        let preparing = Preparing::default();
        begin_prepare(&host, &m.layout, &request("ab12"), "boot1", &preparing)
            .await
            .unwrap();
        assert!(undo(&host, &m.layout, "ab12", "boot1", &preparing, true)
            .await
            .is_err());

        prepare(&host, &m.layout, &request("ab12"), &preparing).await;
        assert!(undo(&host, &m.layout, "ab12", "boot1", &preparing, false)
            .await
            .is_err());
        assert!(
            armed(&m) || config(&m).contains(FSID),
            "nothing was put back"
        );
    }

    #[tokio::test]
    async fn a_machine_that_restarted_into_the_new_cluster_cannot_be_undone() {
        let m = machine();
        let host = host();
        prepared(&m, &host, "ab12").await;
        arm(&m.layout, "ab12", "boot1").unwrap();
        assert!(undo(
            &host,
            &m.layout,
            "ab12",
            "boot2",
            &Preparing::default(),
            true
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn a_new_heal_disarms_a_machine_nobody_drives_any_more() {
        let m = machine();
        let host = host();
        prepared(&m, &host, "ab12").await;
        arm(&m.layout, "ab12", "boot1").unwrap();

        let begin = begin_prepare(
            &host,
            &m.layout,
            &request("cd34"),
            "boot1",
            &Preparing::default(),
        )
        .await
        .unwrap();

        assert_eq!(begin, Begin::Start);
        assert!(!armed(&m));
        assert_eq!(config(&m), ORIGINAL);
    }
}
