//! One machine's part of a FORCE HEAL: becoming a fresh member of the new
//! cluster at its next boot.
//!
//! What makes a machine create a cluster or join one, and which Ceph cluster it
//! belongs to, is read from its config.toml when its system is BUILT
//! (`[node.k3s] server_addr`, `[ceph] fsid`; see homelab/nixos/common.nix). So
//! a machine is reset by building the system its new config.toml describes and
//! booting it, with `storage::reset_wipe` erasing the old state first.
//!
//! TWO PHASES, so that a heal that cannot finish leaves every machine as it was:
//!
//!   build   Writes the new config.toml into a staging copy of the machine
//!           directory and builds the system from it. Slow, and the step that
//!           fails (a broken repo, a full disk) — and it changes nothing: the
//!           running system, the boot entry and config.toml are untouched.
//!   commit  Makes that system the default boot entry, installs the new
//!           config.toml and leaves the wipe marker. Seconds.
//!
//! Until the machine restarts, a commit is undone by putting all three back.
//! After the restart there is nothing left to undo: the wipe has run.
//!
//! The driver (`heal`) asks every machine that answers, itself included, over
//! the same HTTP endpoints, so every machine does exactly the same thing.

use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::host::Host;
use crate::storage::reset_wipe;

/// The profile whose newest generation is the default boot entry.
const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
/// A build runs from the repo as it is; one that needs to compile large parts
/// of the system can take a long time on a laptop.
const BUILD_TIMEOUT: Duration = Duration::from_secs(2 * 3600);
/// Installing the boot entry.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Where this machine keeps its part of a heal.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    /// `/`, or a temporary directory in tests.
    pub root: PathBuf,
    /// This machine's own files: the `yolab-machine` flake input.
    pub machine_dir: PathBuf,
    pub repo: String,
    pub flake_target: String,
}

impl Layout {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            root: PathBuf::from("/"),
            machine_dir: PathBuf::from(&cfg.machine_dir),
            repo: cfg.repo_path.clone(),
            flake_target: cfg.flake_target.clone(),
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("var/lib/yolab/reset")
    }
    fn state(&self) -> PathBuf {
        self.dir().join("state.json")
    }
    /// The machine directory as it will be. Outside `machine_dir`, whose whole
    /// content is copied into the Nix store by every rebuild.
    fn staging(&self) -> PathBuf {
        self.dir().join("machine")
    }
    /// The built system, kept alive against garbage collection by this link.
    fn system_link(&self) -> PathBuf {
        self.dir().join("system")
    }
    fn config_before(&self) -> PathBuf {
        self.dir().join("config.toml.before")
    }
    fn config(&self) -> PathBuf {
        self.machine_dir.join("config.toml")
    }
    fn marker(&self) -> PathBuf {
        self.root.join(reset_wipe::MARKER)
    }
    fn current_system(&self) -> PathBuf {
        self.root.join("run/current-system")
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

/// What the driver asks a machine to become.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildRequest {
    pub heal_id: String,
    /// The machine driving the heal.
    pub driver: String,
    /// The new cluster's Ceph fsid.
    pub fsid: String,
    /// `""` for the machine that creates the new cluster, the creator's k3s URL
    /// (`https://[<addr>]:6443`) for every other.
    pub server_addr: String,
}

impl BuildRequest {
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

/// The address inside `https://[<ipv6>]:6443` — the only shape
/// homelab/nixos/common.nix can read a Ceph seed address out of.
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
    Building,
    Built {
        system: String,
    },
    Failed {
        error: String,
    },
    /// The next boot starts `system` and wipes the machine. `boot_id` is the
    /// boot the commit happened in: a different one means it restarted.
    Committed {
        system: String,
        boot_id: String,
    },
    Undone,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Reset {
    #[serde(flatten)]
    request: BuildRequest,
    #[serde(flatten)]
    phase: Phase,
}

/// A machine's part in a heal, as the driver and the page see it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PhaseView {
    Building,
    Built,
    Failed,
    /// Switched over; restarts into the new cluster.
    Committed,
    /// Restarted since the commit: the machine is part of the new cluster.
    Restarted,
    Undone,
}

impl PhaseView {
    /// Whether the machine is taken by this heal: a new one must not start
    /// under it.
    pub fn active(self) -> bool {
        matches!(
            self,
            PhaseView::Building | PhaseView::Built | PhaseView::Committed
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

fn view(reset: &Reset, boot_id: &str, building: Option<&str>) -> ResetView {
    let (phase, error) = match &reset.phase {
        Phase::Building if building == Some(reset.request.heal_id.as_str()) => {
            (PhaseView::Building, None)
        }
        // The process that ran the build is gone (local-api restarted).
        Phase::Building => (
            PhaseView::Failed,
            Some("the build was interrupted".to_string()),
        ),
        Phase::Built { .. } => (PhaseView::Built, None),
        Phase::Failed { error } => (PhaseView::Failed, Some(error.clone())),
        Phase::Committed { boot_id: at, .. } if at == boot_id => (PhaseView::Committed, None),
        Phase::Committed { .. } => (PhaseView::Restarted, None),
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

fn save(layout: &Layout, reset: &Reset) -> Result<()> {
    write_file(&layout.state(), &serde_json::to_vec_pretty(reset)?)
}

/// Written whole to a temporary file and renamed into place, root-only: a crash
/// mid-write leaves the previous content, never half of the new one.
pub(crate) fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = path.parent().context("a path without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open {}", tmp.display()))?;
    file.write_all(content)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// Which heal's build runs in this process right now. A `Building` state with
/// no build here was interrupted.
#[derive(Clone, Default)]
pub(crate) struct Builds(Arc<Mutex<Option<String>>>);

impl Builds {
    pub fn global() -> &'static Builds {
        static BUILDS: OnceLock<Builds> = OnceLock::new();
        BUILDS.get_or_init(Builds::default)
    }
    fn running(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set(&self, heal_id: Option<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = heal_id;
    }
}

/// Every change to a machine's part in a heal happens under this lock, so a
/// commit and an undo, or a finishing build and an undo, never interleave.
pub(crate) fn lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn current(layout: &Layout, boot_id: &str, builds: &Builds) -> Result<Option<ResetView>> {
    Ok(load(layout)?.map(|r| view(&r, boot_id, builds.running().as_deref())))
}

// ── Build ─────────────────────────────────────────────────────────────────────

/// What `begin_build` decided.
#[derive(Debug, PartialEq)]
pub(crate) enum Begin {
    /// This heal's build is already under way or done.
    Already(ResetView),
    /// The state says `Building`: run `build` now.
    Start,
}

/// Records that a build for `request` starts, unless one is already under way.
///
/// A machine still committed to ANOTHER heal is undone first. The driver only
/// asks when that heal's own driver no longer answers (see `Survey::refusal`),
/// and a machine left committed to a heal nobody drives would wipe itself into
/// a cluster that never forms at its next restart.
pub(crate) async fn begin_build<H: Host>(
    host: &H,
    layout: &Layout,
    request: &BuildRequest,
    boot_id: &str,
    builds: &Builds,
) -> Result<Begin> {
    request.validate()?;
    if let Some(reset) = load(layout)? {
        let v = view(&reset, boot_id, builds.running().as_deref());
        if reset.request.heal_id == request.heal_id {
            if reset.request != *request {
                bail!("heal {} asked for a different build before", request.heal_id);
            }
            if v.phase != PhaseView::Failed || builds.running().is_some() {
                return Ok(Begin::Already(v));
            }
            // Failed: the driver fails the heal on it. Asked again, build again.
        } else {
            if builds.running().is_some() {
                bail!("heal {} is building on this machine", reset.request.heal_id);
            }
            if v.phase == PhaseView::Committed {
                tracing::warn!(
                    "heal {}: undoing heal {} from {} first",
                    request.heal_id,
                    reset.request.heal_id,
                    reset.request.driver
                );
                restore_previous(host, layout).await?;
            }
        }
    }
    save(
        layout,
        &Reset {
            request: request.clone(),
            phase: Phase::Building,
        },
    )?;
    builds.set(Some(request.heal_id.clone()));
    Ok(Begin::Start)
}

/// Builds the system and records the outcome — unless the heal was undone
/// meanwhile, which then stays undone.
pub(crate) async fn build<H: Host>(host: &H, layout: &Layout, request: &BuildRequest, builds: &Builds) {
    let outcome = stage_and_build(host, layout, request).await;
    let _held = lock().lock().await;
    builds.set(None);
    let still_building = matches!(
        load(layout),
        Ok(Some(Reset { request: ref r, phase: Phase::Building })) if r == request
    );
    if !still_building {
        tracing::warn!("heal {}: build finished after it was cancelled", request.heal_id);
        return;
    }
    let phase = match outcome {
        Ok(system) => {
            tracing::warn!("heal {}: built {system}", request.heal_id);
            Phase::Built { system }
        }
        Err(e) => {
            tracing::error!("heal {}: build failed: {e:#}", request.heal_id);
            Phase::Failed {
                error: format!("{e:#}"),
            }
        }
    };
    let saved = save(
        layout,
        &Reset {
            request: request.clone(),
            phase,
        },
    );
    if let Err(e) = saved {
        tracing::error!("heal {}: record the build: {e:#}", request.heal_id);
    }
}

/// The staging copy of the machine directory with the new config.toml, and the
/// system built from it. Returns the system's store path.
async fn stage_and_build<H: Host>(host: &H, layout: &Layout, request: &BuildRequest) -> Result<String> {
    let staging = layout.staging();
    match std::fs::remove_dir_all(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clear {}", staging.display())),
    }
    std::fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    std::fs::set_permissions(&staging, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    for entry in std::fs::read_dir(&layout.machine_dir)
        .with_context(|| format!("read {}", layout.machine_dir.display()))?
    {
        let entry = entry?;
        // Following links: a machine directory may link its files elsewhere.
        if std::fs::metadata(entry.path())?.is_file() {
            std::fs::copy(entry.path(), staging.join(entry.file_name()))
                .with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }
    let config = std::fs::read_to_string(layout.config())
        .with_context(|| format!("read {}", layout.config().display()))?;
    let rewritten = rewrite_config(&config, &request.server_addr, &request.fsid)?;
    write_file(&staging.join("config.toml"), rewritten.as_bytes())?;

    let installable = format!(
        "{}#nixosConfigurations.{}.config.system.build.toplevel",
        layout.repo, layout.flake_target
    );
    let input = format!("path:{}", staging.display());
    let link = layout.system_link().to_string_lossy().into_owned();
    let out = host
        .run_cmd_bounded(
            "nix",
            &[
                "build",
                &installable,
                "--override-input",
                "yolab-machine",
                &input,
                "--no-write-lock-file",
                "--accept-flake-config",
                "--out-link",
                &link,
            ],
            BUILD_TIMEOUT,
        )
        .await?;
    if !out.success {
        bail!("nix build failed: {}", tail(&out.stderr, 20));
    }
    let system = std::fs::read_link(layout.system_link())
        .with_context(|| format!("nix build left no {link}"))?;
    Ok(system.to_string_lossy().into_owned())
}

/// `config` with the new cluster's settings, everything else as it was.
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
    toml::to_string(&table).context("write config.toml")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// ── Commit and undo ───────────────────────────────────────────────────────────

/// Switches this machine over. On failure, whatever was switched is put back
/// and the heal's state is `Failed`.
pub(crate) async fn commit<H: Host>(host: &H, layout: &Layout, heal_id: &str, boot_id: &str) -> Result<()> {
    let Some(reset) = load(layout)?.filter(|r| r.request.heal_id == heal_id) else {
        bail!("this machine has not built anything for heal {heal_id}");
    };
    let system = match &reset.phase {
        Phase::Committed { boot_id: at, .. } if at == boot_id => return Ok(()),
        Phase::Built { system } => system.clone(),
        other => bail!("heal {heal_id} cannot be committed here: it is {other:?}"),
    };
    match switch_over(host, layout, &reset.request, &system).await {
        Ok(()) => {
            save(
                layout,
                &Reset {
                    request: reset.request,
                    phase: Phase::Committed {
                        system,
                        boot_id: boot_id.to_string(),
                    },
                },
            )?;
            tracing::warn!("heal {heal_id}: this machine restarts into the new cluster");
            Ok(())
        }
        Err(e) => {
            let mut error = format!("{e:#}");
            if let Err(undo) = restore_previous(host, layout).await {
                error = format!("{error}; putting the previous system back also failed: {undo:#}");
            }
            save(
                layout,
                &Reset {
                    request: reset.request,
                    phase: Phase::Failed {
                        error: error.clone(),
                    },
                },
            )?;
            bail!("{error}")
        }
    }
}

async fn switch_over<H: Host>(host: &H, layout: &Layout, request: &BuildRequest, system: &str) -> Result<()> {
    let switch = format!("{system}/bin/switch-to-configuration");
    if !Path::new(&switch).exists() {
        bail!("{switch} does not exist — the build is gone");
    }
    std::fs::copy(layout.config(), layout.config_before())
        .with_context(|| format!("keep a copy of {}", layout.config().display()))?;
    run(host, "nix-env", &["-p", SYSTEM_PROFILE, "--set", system]).await?;
    run(host, &switch, &["boot"]).await?;
    let staged = std::fs::read(layout.staging().join("config.toml"))
        .context("read the staged config.toml")?;
    write_file(&layout.config(), &staged)?;
    write_file(&layout.marker(), request.heal_id.as_bytes())?;
    Ok(())
}

/// Undoes this heal on this machine, if it has not restarted yet.
///
/// `may_switch`: whether this caller holds off updates, which putting the
/// previous system back needs. Cancelling a build does not: the build itself
/// holds them off.
pub(crate) async fn undo<H: Host>(
    host: &H,
    layout: &Layout,
    heal_id: &str,
    boot_id: &str,
    may_switch: bool,
) -> Result<()> {
    let Some(reset) = load(layout)?.filter(|r| r.request.heal_id == heal_id) else {
        return Ok(());
    };
    let switch_back = match &reset.phase {
        Phase::Undone => return Ok(()),
        Phase::Committed { boot_id: at, .. } if at != boot_id => {
            bail!("this machine already restarted into the new cluster")
        }
        Phase::Committed { .. } => true,
        // A failed commit already tried to put things back; make sure.
        Phase::Failed { .. } => layout.marker().exists(),
        Phase::Building | Phase::Built { .. } => false,
    };
    if switch_back {
        if !may_switch {
            bail!("an update is switching this machine's system — try again");
        }
        restore_previous(host, layout).await?;
    }
    save(
        layout,
        &Reset {
            request: reset.request,
            phase: Phase::Undone,
        },
    )?;
    tracing::warn!("heal {heal_id}: undone on this machine");
    Ok(())
}

/// The machine as it was before a commit: no wipe marker, the running system as
/// the default boot entry, the previous config.toml.
///
/// The marker goes first: a machine that boots with it wipes itself, whichever
/// system it boots.
async fn restore_previous<H: Host>(host: &H, layout: &Layout) -> Result<()> {
    match std::fs::remove_file(layout.marker()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("remove the wipe marker"),
    }
    let running = std::fs::read_link(layout.current_system()).context("read /run/current-system")?;
    let running = running.to_string_lossy().into_owned();
    run(host, "nix-env", &["-p", SYSTEM_PROFILE, "--set", &running]).await?;
    run(host, &format!("{running}/bin/switch-to-configuration"), &["boot"]).await?;
    if layout.config_before().exists() {
        let before = std::fs::read(layout.config_before()).context("read the previous config.toml")?;
        write_file(&layout.config(), &before)?;
    }
    Ok(())
}

async fn run<H: Host>(host: &H, bin: &str, args: &[&str]) -> Result<()> {
    let out = host.run_cmd_bounded(bin, args, SWITCH_TIMEOUT).await?;
    if !out.success {
        bail!("{bin} {}: {}", args.join(" "), tail(&out.stderr, 10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const FSID: &str = "0a1b2c3d-1111-4222-8333-444455556666";

    struct Machine {
        _dir: tempfile::TempDir,
        layout: Layout,
        system: String,
        running: String,
    }

    /// A machine directory with a config.toml, a built system and a running one.
    fn machine() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let machine_dir = root.join("var/lib/yolab/machine");
        std::fs::create_dir_all(&machine_dir).unwrap();
        std::fs::write(
            machine_dir.join("config.toml"),
            "[node]\nsub_ipv6_private = \"fd00::2\"\n[node.k3s]\ntoken = \"t\"\nserver_addr = \"\"\n[ceph]\nfsid = \"old\"\n",
        )
        .unwrap();
        std::fs::write(machine_dir.join("hardware-configuration.nix"), "{}").unwrap();
        let system = root.join("nix/store/new-system");
        let running = root.join("nix/store/old-system");
        for s in [&system, &running] {
            std::fs::create_dir_all(s.join("bin")).unwrap();
            std::fs::write(s.join("bin/switch-to-configuration"), "").unwrap();
        }
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::os::unix::fs::symlink(&running, root.join("run/current-system")).unwrap();
        let layout = Layout {
            root,
            machine_dir,
            repo: "/etc/nixos".into(),
            flake_target: "yolab".into(),
        };
        Machine {
            system: system.to_string_lossy().into_owned(),
            running: running.to_string_lossy().into_owned(),
            _dir: dir,
            layout,
        }
    }

    fn request(heal_id: &str) -> BuildRequest {
        BuildRequest {
            heal_id: heal_id.into(),
            driver: "node1".into(),
            fsid: FSID.into(),
            server_addr: "https://[fd00::1]:6443".into(),
        }
    }

    /// A host whose `nix build` leaves the link to `system`, the way the real one does.
    fn build_host(m: &Machine) -> FakeHost {
        std::fs::create_dir_all(m.layout.dir()).unwrap();
        std::os::unix::fs::symlink(&m.system, m.layout.system_link()).unwrap();
        FakeHost::new().ok("nix build", "")
    }

    fn built(m: &Machine, heal_id: &str) {
        save(
            &m.layout,
            &Reset {
                request: request(heal_id),
                phase: Phase::Built {
                    system: m.system.clone(),
                },
            },
        )
        .unwrap();
        std::fs::create_dir_all(m.layout.staging()).unwrap();
        std::fs::write(m.layout.staging().join("config.toml"), "new config").unwrap();
    }

    fn config(m: &Machine) -> String {
        std::fs::read_to_string(m.layout.config()).unwrap()
    }

    #[test]
    fn the_config_gets_the_new_cluster_and_keeps_everything_else() {
        let before = "[homelab]\nhostname = \"node2\"\n[node]\nsub_ipv6_private = \"fd00::2\"\n[node.k3s]\ntoken = \"tok\"\nserver_addr = \"https://[fd00::1]:6443\"\n[ceph]\nfsid = \"old\"\n";
        let after: toml::Table = toml::from_str(&rewrite_config(before, "", FSID).unwrap()).unwrap();
        assert_eq!(after["node"]["k3s"]["server_addr"].as_str(), Some(""));
        assert_eq!(after["node"]["k3s"]["token"].as_str(), Some("tok"));
        assert_eq!(after["ceph"]["fsid"].as_str(), Some(FSID));
        assert_eq!(after["homelab"]["hostname"].as_str(), Some("node2"));
        assert_eq!(after["node"]["sub_ipv6_private"].as_str(), Some("fd00::2"));

        let without_ceph = "[node.k3s]\ntoken = \"tok\"\n";
        let after: toml::Table =
            toml::from_str(&rewrite_config(without_ceph, "https://[fd00::1]:6443", FSID).unwrap()).unwrap();
        assert_eq!(after["ceph"]["fsid"].as_str(), Some(FSID));
        assert!(rewrite_config("[homelab]\n", "", FSID).is_err(), "no [node.k3s] to set");
    }

    #[test]
    fn a_request_is_checked_before_anything_is_built_from_it() {
        assert!(request("ab12").validate().is_ok());
        let bad = [
            BuildRequest { heal_id: "../x".into(), ..request("ab12") },
            BuildRequest { fsid: "not-a-uuid".into(), ..request("ab12") },
            BuildRequest { server_addr: "https://fd00::1:6443".into(), ..request("ab12") },
            BuildRequest { server_addr: "https://[fd00::1]:6443\"\n[x]".into(), ..request("ab12") },
            BuildRequest { driver: "".into(), ..request("ab12") },
        ];
        for r in bad {
            assert!(r.validate().is_err(), "{r:?}");
        }
        assert!(BuildRequest { server_addr: "".into(), ..request("ab12") }.validate().is_ok());
    }

    #[tokio::test]
    async fn a_build_stages_the_new_config_and_touches_nothing_the_machine_runs_on() {
        let m = machine();
        let host = build_host(&m);
        let builds = Builds::default();
        let req = request("ab12");
        let before = config(&m);

        assert_eq!(begin_build(&host, &m.layout, &req, "boot1", &builds).await.unwrap(), Begin::Start);
        assert_eq!(current(&m.layout, "boot1", &builds).unwrap().unwrap().phase, PhaseView::Building);
        build(&host, &m.layout, &req, &builds).await;

        let v = current(&m.layout, "boot1", &builds).unwrap().unwrap();
        assert_eq!(v.phase, PhaseView::Built, "{v:?}");
        let staged = std::fs::read_to_string(m.layout.staging().join("config.toml")).unwrap();
        assert!(staged.contains(FSID) && staged.contains("https://[fd00::1]:6443"));
        assert!(m.layout.staging().join("hardware-configuration.nix").exists());
        assert!(host.ran(&format!(
            "nix build /etc/nixos#nixosConfigurations.yolab.config.system.build.toplevel --override-input yolab-machine path:{}",
            m.layout.staging().display()
        )));
        assert_eq!(config(&m), before, "config.toml is untouched");
        assert!(!m.layout.marker().exists());
        assert!(!host.ran("switch-to-configuration") && !host.ran("nix-env"));
    }

    #[tokio::test]
    async fn a_failed_build_is_reported_with_its_error() {
        let m = machine();
        let host = FakeHost::new().fail("nix build", "error: attribute 'yolab' missing");
        let builds = Builds::default();
        let req = request("ab12");
        begin_build(&host, &m.layout, &req, "boot1", &builds).await.unwrap();
        build(&host, &m.layout, &req, &builds).await;
        let v = current(&m.layout, "boot1", &builds).unwrap().unwrap();
        assert_eq!(v.phase, PhaseView::Failed);
        assert!(v.error.unwrap().contains("attribute 'yolab' missing"));
    }

    #[tokio::test]
    async fn asking_again_does_not_build_twice_and_an_interrupted_build_is_a_failure() {
        let m = machine();
        let host = FakeHost::new();
        let builds = Builds::default();
        let req = request("ab12");
        begin_build(&host, &m.layout, &req, "boot1", &builds).await.unwrap();
        assert!(matches!(
            begin_build(&host, &m.layout, &req, "boot1", &builds).await.unwrap(),
            Begin::Already(ResetView { phase: PhaseView::Building, .. })
        ));
        assert!(
            begin_build(&host, &m.layout, &request("cd34"), "boot1", &builds).await.is_err(),
            "one build at a time"
        );

        // local-api restarted: nothing builds any more.
        let fresh = Builds::default();
        let v = current(&m.layout, "boot1", &fresh).unwrap().unwrap();
        assert_eq!(v.phase, PhaseView::Failed);
        assert_eq!(v.error.as_deref(), Some("the build was interrupted"));
    }

    #[tokio::test]
    async fn a_commit_switches_the_boot_entry_then_the_config_then_leaves_the_marker() {
        let m = machine();
        built(&m, "ab12");
        let host = FakeHost::new().ok("nix-env", "").ok(&m.system, "");

        commit(&host, &m.layout, "ab12", "boot1").await.unwrap();

        let set = host.position(&format!("nix-env -p /nix/var/nix/profiles/system --set {}", m.system)).unwrap();
        let switch = host.position(&format!("{}/bin/switch-to-configuration boot", m.system)).unwrap();
        assert!(set < switch);
        assert_eq!(config(&m), "new config");
        assert!(std::fs::read_to_string(m.layout.config_before()).unwrap().contains("fsid = \"old\""));
        assert_eq!(std::fs::read_to_string(m.layout.marker()).unwrap(), "ab12");
        let builds = Builds::default();
        assert_eq!(current(&m.layout, "boot1", &builds).unwrap().unwrap().phase, PhaseView::Committed);
        assert_eq!(current(&m.layout, "boot2", &builds).unwrap().unwrap().phase, PhaseView::Restarted);

        // Asked again in the same boot: nothing more happens.
        let calls = host.calls().len();
        commit(&host, &m.layout, "ab12", "boot1").await.unwrap();
        assert_eq!(host.calls().len(), calls);
    }

    #[tokio::test]
    async fn a_commit_that_fails_puts_the_running_system_back() {
        let m = machine();
        built(&m, "ab12");
        let before = config(&m);
        let host = FakeHost::new()
            .ok("nix-env", "")
            .fail(&format!("{}/bin/switch-to-configuration", m.system), "bootctl: no space left")
            .ok(&format!("{}/bin/switch-to-configuration", m.running), "");

        let err = commit(&host, &m.layout, "ab12", "boot1").await.unwrap_err();

        assert!(format!("{err:#}").contains("no space left"));
        assert!(host.ran(&format!("nix-env -p /nix/var/nix/profiles/system --set {}", m.running)));
        assert!(host.ran(&format!("{}/bin/switch-to-configuration boot", m.running)));
        assert_eq!(config(&m), before);
        assert!(!m.layout.marker().exists());
        assert_eq!(current(&m.layout, "boot1", &Builds::default()).unwrap().unwrap().phase, PhaseView::Failed);
    }

    #[tokio::test]
    async fn only_a_built_heal_is_committed() {
        let m = machine();
        let host = FakeHost::new();
        assert!(commit(&host, &m.layout, "ab12", "boot1").await.is_err(), "nothing built");
        built(&m, "ab12");
        assert!(commit(&host, &m.layout, "cd34", "boot1").await.is_err(), "another heal");
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn an_undo_before_the_restart_puts_everything_back() {
        let m = machine();
        built(&m, "ab12");
        let before = config(&m);
        let host = FakeHost::new().ok("nix-env", "").ok(&m.system, "").ok(&m.running, "");
        commit(&host, &m.layout, "ab12", "boot1").await.unwrap();

        undo(&host, &m.layout, "ab12", "boot1", true).await.unwrap();

        assert!(!m.layout.marker().exists());
        assert_eq!(config(&m), before);
        assert!(host.ran(&format!("{}/bin/switch-to-configuration boot", m.running)));
        assert_eq!(current(&m.layout, "boot1", &Builds::default()).unwrap().unwrap().phase, PhaseView::Undone);
        // Again, and for a heal this machine never heard of: nothing to do.
        undo(&host, &m.layout, "ab12", "boot1", true).await.unwrap();
        undo(&host, &m.layout, "ffff", "boot1", true).await.unwrap();
    }

    #[tokio::test]
    async fn a_machine_that_restarted_into_the_new_cluster_cannot_be_undone() {
        let m = machine();
        built(&m, "ab12");
        let host = FakeHost::new().ok("nix-env", "").ok(&m.system, "");
        commit(&host, &m.layout, "ab12", "boot1").await.unwrap();
        assert!(undo(&host, &m.layout, "ab12", "boot2", true).await.is_err());
    }

    #[tokio::test]
    async fn an_undo_during_the_build_keeps_the_heal_undone_when_the_build_ends() {
        let m = machine();
        let host = build_host(&m);
        let builds = Builds::default();
        let req = request("ab12");
        begin_build(&host, &m.layout, &req, "boot1", &builds).await.unwrap();
        undo(&host, &m.layout, "ab12", "boot1", true).await.unwrap();
        build(&host, &m.layout, &req, &builds).await;
        assert_eq!(current(&m.layout, "boot1", &builds).unwrap().unwrap().phase, PhaseView::Undone);
    }

    #[tokio::test]
    async fn a_new_heal_undoes_a_commit_nobody_drives_any_more() {
        let m = machine();
        built(&m, "ab12");
        let host = FakeHost::new().ok("nix-env", "").ok(&m.system, "").ok(&m.running, "");
        commit(&host, &m.layout, "ab12", "boot1").await.unwrap();

        let builds = Builds::default();
        let begin = begin_build(&host, &m.layout, &request("cd34"), "boot1", &builds).await.unwrap();

        assert_eq!(begin, Begin::Start);
        assert!(!m.layout.marker().exists());
        assert!(host.ran(&format!("{}/bin/switch-to-configuration boot", m.running)));
    }
}
