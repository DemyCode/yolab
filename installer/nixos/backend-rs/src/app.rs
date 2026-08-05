use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{backend::CrosstermBackend, layout::Position, Terminal};
use std::io;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{install, wireguard::PLATFORM_API};

// ── Step ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Mode,
    Account,
    Disk,
    Configure,
    Install,
}

impl Step {
    pub fn index(&self) -> usize {
        match self {
            Step::Mode => 0,
            Step::Account => 1,
            Step::Disk => 2,
            Step::Configure => 3,
            Step::Install => 4,
        }
    }
}

// ── Domain types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ClusterMode {
    New,
    Join,
}

pub struct DiskInfo {
    pub name: String,
    pub size: String,
    pub tran: String,
    pub is_usb: bool,
    pub mounted: bool,
    pub recommended: bool,
}

// ── Events from background tasks ──────────────────────────────────────────────

pub enum AppEvent {
    NetworkReady,
    AccountCreated(String),
    AccountVerified,
    DisksLoaded(Vec<DiskInfo>),
    JoinConnected { server_addr: String, k3s_token: String },
    SshKeyGenerated { private_key: String, public_key: String },
    Log(String),
    InstallComplete { url: String },
    Failed(String),
}

// ── Click target registry ─────────────────────────────────────────────────────

#[derive(Clone)]
pub enum ClickTarget {
    ModeOption(usize),
    AcctMethod(u8),
    DiskOption(usize),
    CfgField(u8),
    Btn(BtnId),
}

#[derive(Clone, PartialEq)]
#[allow(dead_code)]
pub enum BtnId {
    Continue,
    Back,
    CreateAcct,
    VerifyToken,
    Connect,
    GenSshKey,
    Reboot,
    Poweroff,
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct App {
    pub step: Step,

    // Step 1 – Mode
    pub mode: Option<ClusterMode>,
    pub mode_cursor: usize, // 0=New 1=Join

    // Step 2 – Account (new) / Connect (join)
    pub acct_cursor: u8,        // 0=Create 1=Existing
    pub acct_input: String,     // existing token input
    pub account_token: Option<String>,
    pub created_token: Option<String>,
    pub join_url: String,
    pub join_pass: String,
    pub join_field: u8, // 0=URL 1=pass

    // Step 3 – Disk
    pub disks: Vec<DiskInfo>,
    pub disk_cursor: usize,

    // Step 4 – Configure
    pub timezone: String,
    pub password: String,
    pub password2: String,
    pub ssh_pub: String,
    pub cfg_field: u8,
    pub gen_privkey: Option<String>,

    // Carry-over from join
    pub join_server_addr: Option<String>,
    pub join_k3s_token: Option<String>,

    // Step 5 – Install
    pub log_lines: Vec<String>,
    pub install_done: bool,
    pub install_failed: bool,
    pub mgmt_url: Option<String>,

    pub network_ready: bool,
    pub boot_mode: String, // "uefi" or "bios"

    // Global UI
    pub error: Option<String>,
    pub loading: bool,
    pub loading_msg: String,
    pub click_areas: Vec<(ratatui::layout::Rect, ClickTarget)>,

    pub tx: mpsc::UnboundedSender<AppEvent>,
    pub rx: mpsc::UnboundedReceiver<AppEvent>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        // Kick off network readiness check immediately — retries every second
        // until api.demycode.ovh resolves (NetworkManager + DHCP may not be up yet).
        {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                loop {
                    if tokio::net::lookup_host("api.demycode.ovh:443").await.is_ok() {
                        let _ = tx2.send(AppEvent::NetworkReady);
                        return;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            });
        }
        Self {
            step: Step::Mode,
            mode: None,
            mode_cursor: 0,
            acct_cursor: 0,
            acct_input: String::new(),
            account_token: None,
            created_token: None,
            join_url: String::new(),
            join_pass: String::new(),
            join_field: 0,
            disks: vec![],
            disk_cursor: 0,
            timezone: "UTC".into(),
            password: String::new(),
            password2: String::new(),
            ssh_pub: String::new(),
            cfg_field: 0,
            gen_privkey: None,
            join_server_addr: None,
            join_k3s_token: None,
            log_lines: vec![],
            install_done: false,
            install_failed: false,
            mgmt_url: None,
            network_ready: false,
            boot_mode: if std::path::Path::new("/sys/firmware/efi/efivars").exists() {
                "uefi".into()
            } else {
                "bios".into()
            },
            error: None,
            loading: true,
            loading_msg: "Waiting for network…".into(),
            click_areas: vec![],
            tx,
            rx,
        }
    }

    // ── Main loop ─────────────────────────────────────────────────────────────

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> anyhow::Result<()> {
        use crossterm::event::EventStream;
        let mut events = EventStream::new();

        loop {
            self.click_areas.clear();
            terminal.draw(|f| crate::ui::render(f, self))?;

            tokio::select! {
                maybe = events.next() => {
                    let Some(event) = maybe else { break };
                    let event = event?;
                    match event {
                        Event::Key(k) => {
                            if self.handle_key(k).await { return Ok(()); }
                        }
                        Event::Mouse(m) => self.handle_mouse(m).await,
                        _ => {}
                    }
                }
                Some(ev) = self.rx.recv() => self.handle_app_event(ev),
            }
        }
        Ok(())
    }

    // ── Key handling ──────────────────────────────────────────────────────────

    async fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl-C always quits
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.loading { return false; }

        self.error = None;

        match &self.step.clone() {
            Step::Mode => self.key_mode(key).await,
            Step::Account => self.key_account(key).await,
            Step::Disk => self.key_disk(key).await,
            Step::Configure => self.key_configure(key).await,
            Step::Install => self.key_install(key),
        }
        false
    }

    fn key_install(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('r') if self.install_done => self.do_reboot(),
            KeyCode::Char('p') if self.install_done => self.do_poweroff(),
            _ => {}
        }
    }

    async fn key_mode(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.mode_cursor = self.mode_cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.mode_cursor < 1 { self.mode_cursor += 1; }
            }
            KeyCode::Enter => self.confirm_mode().await,
            _ => {}
        }
    }

    async fn key_account(&mut self, key: KeyEvent) {
        match self.mode {
            Some(ClusterMode::New) => self.key_account_new(key).await,
            Some(ClusterMode::Join) => self.key_account_join(key).await,
            None => {}
        }
    }

    async fn key_account_new(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => { self.step = Step::Mode; }
            KeyCode::Tab | KeyCode::BackTab => {
                self.acct_cursor = if self.acct_cursor == 0 { 1 } else { 0 };
                self.error = None;
            }
            KeyCode::Enter => {
                if self.account_token.is_some() {
                    self.goto_disk();
                } else if self.acct_cursor == 0 {
                    self.create_account();
                } else {
                    self.verify_token().await;
                }
            }
            KeyCode::Char(c) if self.acct_cursor == 1 && self.account_token.is_none() => {
                self.acct_input.push(c);
            }
            KeyCode::Backspace if self.acct_cursor == 1 => {
                self.acct_input.pop();
            }
            _ => {}
        }
    }

    async fn key_account_join(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => { self.step = Step::Mode; }
            KeyCode::Tab => {
                self.join_field = if self.join_field == 0 { 1 } else { 0 };
            }
            KeyCode::BackTab => {
                self.join_field = if self.join_field == 0 { 1 } else { 0 };
            }
            KeyCode::Enter => self.fetch_join_info().await,
            KeyCode::Char(c) => {
                if self.join_field == 0 { self.join_url.push(c); }
                else { self.join_pass.push(c); }
            }
            KeyCode::Backspace => {
                if self.join_field == 0 { self.join_url.pop(); }
                else { self.join_pass.pop(); }
            }
            _ => {}
        }
    }

    async fn key_disk(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => { self.step = Step::Account; }
            KeyCode::Up | KeyCode::Char('k') => {
                self.disk_cursor = self.disk_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.disks.is_empty() && self.disk_cursor < self.disks.len() - 1 {
                    self.disk_cursor += 1;
                }
            }
            KeyCode::Enter => {
                let disk = self.disks.get(self.disk_cursor);
                if let Some(d) = disk {
                    if d.mounted {
                        self.error = Some("That disk is in use — choose another.".into());
                    } else {
                        self.step = Step::Configure;
                    }
                }
            }
            _ => {}
        }
    }

    async fn key_configure(&mut self, key: KeyEvent) {
        const FIELDS: u8 = 4; // timezone, pass, pass2, ssh_pub
        match key.code {
            KeyCode::Esc => { self.step = Step::Disk; }
            KeyCode::Tab => {
                self.cfg_field = (self.cfg_field + 1) % FIELDS;
            }
            KeyCode::BackTab => {
                self.cfg_field = if self.cfg_field == 0 { FIELDS - 1 } else { self.cfg_field - 1 };
            }
            KeyCode::Char(c) => self.cfg_push(c),
            KeyCode::Backspace => self.cfg_pop(),
            KeyCode::Enter => self.confirm_configure().await,
            _ => {}
        }
    }

    fn cfg_push(&mut self, c: char) {
        match self.cfg_field {
            0 => self.timezone.push(c),
            1 => self.password.push(c),
            2 => self.password2.push(c),
            3 => self.ssh_pub.push(c),
            _ => {}
        }
    }

    fn cfg_pop(&mut self) {
        match self.cfg_field {
            0 => { self.timezone.pop(); }
            1 => { self.password.pop(); }
            2 => { self.password2.pop(); }
            3 => { self.ssh_pub.pop(); }
            _ => {}
        }
    }

    // ── Mouse handling ────────────────────────────────────────────────────────

    async fn handle_mouse(&mut self, m: MouseEvent) {
        if self.loading { return; }
        if m.kind != MouseEventKind::Down(MouseButton::Left) { return; }

        let pos = Position { x: m.column, y: m.row };
        let target = self.click_areas.iter()
            .find(|(rect, _)| rect.contains(pos))
            .map(|(_, t)| t.clone());

        if let Some(target) = target {
            self.error = None;
            self.handle_click(target).await;
        }
    }

    async fn handle_click(&mut self, target: ClickTarget) {
        match target {
            ClickTarget::ModeOption(n) => {
                self.mode_cursor = n;
                self.confirm_mode().await;
            }
            ClickTarget::AcctMethod(n) => {
                self.acct_cursor = n;
            }
            ClickTarget::DiskOption(n) => {
                self.disk_cursor = n;
            }
            ClickTarget::CfgField(n) => {
                self.cfg_field = n;
            }
            ClickTarget::Btn(id) => self.handle_btn(id).await,
        }
    }

    async fn handle_btn(&mut self, id: BtnId) {
        match id {
            BtnId::Continue => match &self.step.clone() {
                Step::Mode => self.confirm_mode().await,
                Step::Account if self.account_token.is_some() => self.goto_disk(),
                Step::Account => {
                    if self.mode == Some(ClusterMode::New) {
                        if self.acct_cursor == 0 { self.create_account(); }
                        else { self.verify_token().await; }
                    } else {
                        self.fetch_join_info().await;
                    }
                }
                Step::Disk => {
                    if let Some(d) = self.disks.get(self.disk_cursor) {
                        if d.mounted { self.error = Some("Disk is in use.".into()); }
                        else { self.step = Step::Configure; }
                    }
                }
                Step::Configure => self.confirm_configure().await,
                Step::Install => {}
            },
            BtnId::Back => match &self.step.clone() {
                Step::Account => { self.step = Step::Mode; }
                Step::Disk => { self.step = Step::Account; }
                Step::Configure => { self.step = Step::Disk; }
                _ => {}
            },
            BtnId::CreateAcct => self.create_account(),
            BtnId::VerifyToken => self.verify_token().await,
            BtnId::Connect => self.fetch_join_info().await,
            BtnId::GenSshKey => self.gen_ssh_key(),
            BtnId::Reboot => self.do_reboot(),
            BtnId::Poweroff => self.do_poweroff(),
        }
    }

    // ── App event handler ─────────────────────────────────────────────────────

    fn handle_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::NetworkReady => {
                self.network_ready = true;
                self.loading = false;
                return;
            }
            _ => { self.loading = false; }
        }
        match ev {
            AppEvent::NetworkReady => unreachable!(),
            AppEvent::AccountCreated(token) => {
                self.created_token = Some(token.clone());
                self.account_token = Some(token);
            }
            AppEvent::AccountVerified => {
                self.account_token = Some(self.acct_input.trim().to_string());
            }
            AppEvent::DisksLoaded(disks) => {
                let rec = disks.iter().position(|d| d.recommended).unwrap_or(0);
                self.disks = disks;
                self.disk_cursor = rec;
            }
            AppEvent::JoinConnected { server_addr, k3s_token } => {
                self.join_server_addr = Some(server_addr);
                self.join_k3s_token = Some(k3s_token);
                // Pre-fill the configure password from the join password — same homelab, same password.
                if self.password.is_empty() {
                    self.password  = self.join_pass.clone();
                    self.password2 = self.join_pass.clone();
                }
                self.goto_disk();
            }
            AppEvent::SshKeyGenerated { private_key, public_key } => {
                self.gen_privkey = Some(private_key);
                self.ssh_pub = public_key;
            }
            AppEvent::Log(line) => {
                self.log_lines.push(line);
            }
            AppEvent::InstallComplete { url } => {
                self.install_done = true;
                self.mgmt_url = Some(url);
            }
            AppEvent::Failed(msg) => {
                if self.step == Step::Install {
                    self.install_failed = true;
                }
                self.error = Some(msg);
            }
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    async fn confirm_mode(&mut self) {
        if !self.network_ready {
            self.error = Some("Network not ready — waiting for DNS…".into());
            return;
        }
        self.mode = Some(if self.mode_cursor == 0 {
            ClusterMode::New
        } else {
            ClusterMode::Join
        });
        self.step = Step::Account;
    }

    fn create_account(&mut self) {
        self.loading = true;
        self.loading_msg = "Creating account…".into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match do_create_account().await {
                Ok(token) => { let _ = tx.send(AppEvent::AccountCreated(token)); }
                Err(e) => { let _ = tx.send(AppEvent::Failed(e.to_string())); }
            }
        });
    }

    async fn verify_token(&mut self) {
        let token = self.acct_input.trim().to_string();
        if token.is_empty() {
            self.error = Some("Please paste your account token.".into());
            return;
        }
        self.loading = true;
        self.loading_msg = "Verifying token…".into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match do_verify_token(&token).await {
                Ok(()) => { let _ = tx.send(AppEvent::AccountVerified); }
                Err(e) => { let _ = tx.send(AppEvent::Failed(e.to_string())); }
            }
        });
    }

    async fn fetch_join_info(&mut self) {
        let url = self.join_url.trim().to_string();
        let pass = self.join_pass.clone();
        if url.is_empty() {
            self.error = Some("Node URL is required.".into());
            return;
        }
        self.loading = true;
        self.loading_msg = "Connecting to existing node…".into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match do_fetch_join_info(&url, &pass).await {
                Ok((server_addr, k3s_token, account_token)) => {
                    // Store account token via a combined event
                    if let Some(t) = account_token {
                        let _ = tx.send(AppEvent::AccountCreated(t));
                    }
                    let _ = tx.send(AppEvent::JoinConnected { server_addr, k3s_token });
                }
                Err(e) => { let _ = tx.send(AppEvent::Failed(e.to_string())); }
            }
        });
    }

    fn goto_disk(&mut self) {
        self.step = Step::Disk;
        if self.disks.is_empty() {
            self.loading = true;
            self.loading_msg = "Detecting disks…".into();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                match detect_disks().await {
                    Ok(disks) => { let _ = tx.send(AppEvent::DisksLoaded(disks)); }
                    Err(e) => { let _ = tx.send(AppEvent::Failed(e.to_string())); }
                }
            });
        }
    }

    fn gen_ssh_key(&mut self) {
        self.loading = true;
        self.loading_msg = "Generating SSH key…".into();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match do_gen_ssh_key().await {
                Ok((priv_key, pub_key)) => {
                    let _ = tx.send(AppEvent::SshKeyGenerated {
                        private_key: priv_key,
                        public_key: pub_key,
                    });
                }
                Err(e) => { let _ = tx.send(AppEvent::Failed(e.to_string())); }
            }
        });
    }

    async fn confirm_configure(&mut self) {
        if self.password.len() < 8 {
            self.error = Some("Password must be at least 8 characters.".into());
            return;
        }
        if !self.password.chars().any(|c| c.is_uppercase()) {
            self.error = Some("Password must contain at least one uppercase letter.".into());
            return;
        }
        if !self.password.chars().any(|c| c.is_lowercase()) {
            self.error = Some("Password must contain at least one lowercase letter.".into());
            return;
        }
        if !self.password.chars().any(|c| c.is_ascii_digit()) {
            self.error = Some("Password must contain at least one number.".into());
            return;
        }
        if self.password != self.password2 {
            self.error = Some("Passwords do not match.".into());
            return;
        }
        self.start_install();
    }

    fn start_install(&mut self) {
        self.step = Step::Install;
        self.log_lines.clear();

        let disk = self.disks.get(self.disk_cursor)
            .map(|d| d.name.clone())
            .unwrap_or_default();

        let params = install::InstallParams {
            disk,
            timezone: self.timezone.trim().to_string(),
            password: self.password.clone(),
            root_ssh_key: self.ssh_pub.trim().to_string(),
            account_token: self.account_token.clone().unwrap_or_default(),
            server_addr: self.join_server_addr.clone(),
            k3s_token: self.join_k3s_token.clone(),
            boot_mode: self.boot_mode.clone(),
        };

        let tx = self.tx.clone();
        tokio::spawn(install::run_install(params, tx));
    }

    fn do_reboot(&self) {
        let _ = std::process::Command::new("systemctl").arg("reboot").spawn();
    }

    fn do_poweroff(&self) {
        let _ = std::process::Command::new("systemctl").arg("poweroff").spawn();
    }
}

// ── Platform API helpers ──────────────────────────────────────────────────────

async fn do_create_account() -> anyhow::Result<String> {
    let resp = reqwest::Client::new()
        .post(format!("{PLATFORM_API}/users"))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    resp["account_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing account_token in response"))
}

async fn do_verify_token(token: &str) -> anyhow::Result<()> {
    let resp = reqwest::Client::new()
        .get(format!("{PLATFORM_API}/tunnels"))
        .bearer_auth(token)
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "invalid account token");
    Ok(())
}

async fn do_fetch_join_info(
    url: &str,
    password: &str,
) -> anyhow::Result<(String, String, Option<String>)> {
    let client = reqwest::Client::new();
    let url = url.trim_end_matches('/');

    let login = client
        .post(format!("{url}/api/login"))
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await?;
    anyhow::ensure!(login.status().is_success(), "authentication failed");

    let cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .ok_or_else(|| anyhow::anyhow!("no session cookie"))?
        .to_string();

    let info = client
        .get(format!("{url}/api/cluster/join-info"))
        .header("Cookie", cookie)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let server_addr = info["server_addr"].as_str().unwrap_or("").to_string();
    let k3s_token = info["k3s_token"].as_str().unwrap_or("").to_string();
    let account_token = info["account_token"].as_str().map(|s| s.to_string());

    Ok((server_addr, k3s_token, account_token))
}

async fn detect_disks() -> anyhow::Result<Vec<DiskInfo>> {
    let out = tokio::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,SIZE,TRAN,TYPE,MOUNTPOINT"])
        .output()
        .await?;
    let data: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(disks_from_lsblk(&data))
}

/// Turns `lsblk -J -b` output into the disk list the picker shows, marking one
/// as recommended.
///
/// Split from `detect_disks` so the selection rule is testable: this is the
/// screen where someone chooses which disk to erase, and the recommendation is
/// what most people will accept without reading. It must never land on the USB
/// stick the installer itself booted from, nor on a mounted disk.
fn disks_from_lsblk(data: &serde_json::Value) -> Vec<DiskInfo> {
    let empty = vec![];
    let devices = data["blockdevices"].as_array().unwrap_or(&empty);

    let mut disks: Vec<DiskInfo> = devices
        .iter()
        .filter(|d| d["type"].as_str() == Some("disk"))
        .filter_map(|d| {
            let raw = d["name"].as_str()?;
            let name = format!("/dev/{raw}");
            let bytes = d["size"].as_u64().unwrap_or(0);
            let size = fmt_bytes(bytes);
            let tran = d["tran"].as_str().unwrap_or("").to_string();
            let is_usb = tran == "usb";
            let mounted = disk_has_mount(d);
            Some(DiskInfo { name, size, tran, is_usb, mounted, recommended: false })
        })
        .collect();

    let rec = disks
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.is_usb && !d.mounted)
        .max_by_key(|(_, d)| parse_gb(&d.size))
        .map(|(i, _)| i);
    if let Some(i) = rec { disks[i].recommended = true; }

    disks
}

fn disk_has_mount(d: &serde_json::Value) -> bool {
    if d["mountpoint"].as_str().is_some_and(|s| !s.is_empty()) { return true; }
    d["children"].as_array().is_some_and(|ch| ch.iter().any(disk_has_mount))
}

fn fmt_bytes(b: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000 * GB;
    if b >= TB { format!("{:.1} TB", b as f64 / TB as f64) }
    else { format!("{:.0} GB", b as f64 / GB as f64) }
}

fn parse_gb(s: &str) -> u64 {
    let n: f64 = s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    if s.contains("TB") { (n * 1000.0) as u64 } else { n as u64 }
}

async fn do_gen_ssh_key() -> anyhow::Result<(String, String)> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("installer_key");
    let path_str = path.to_str().unwrap();

    tokio::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f", path_str, "-N", "", "-q"])
        .output()
        .await?;

    let private_key = tokio::fs::read_to_string(path_str).await?;
    let public_key = tokio::fs::read_to_string(format!("{path_str}.pub")).await?;
    Ok((private_key.trim().to_string(), public_key.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Step ──────────────────────────────────────────────────────────────────

    /// The index drives the sidebar's progress highlight. Duplicates or gaps show
    /// up as two steps lit at once, or none.
    #[test]
    fn step_indices_are_sequential_and_unique() {
        let steps = [Step::Mode, Step::Account, Step::Disk, Step::Configure, Step::Install];
        let indices: Vec<usize> = steps.iter().map(Step::index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    // ── fmt_bytes ─────────────────────────────────────────────────────────────

    /// Disks are sold in decimal gigabytes, so a 500 GB disk must read "500 GB"
    /// and not the 465 GiB a binary divisor would print — the user is matching
    /// this against the label on the drive.
    #[test]
    fn sizes_are_formatted_in_the_units_printed_on_the_box() {
        assert_eq!(fmt_bytes(500_000_000_000), "500 GB");
        assert_eq!(fmt_bytes(256_000_000_000), "256 GB");
        assert_eq!(fmt_bytes(1_000_000_000_000), "1.0 TB");
        assert_eq!(fmt_bytes(2_000_000_000_000), "2.0 TB");
    }

    #[test]
    fn terabyte_sizes_keep_one_decimal() {
        assert_eq!(fmt_bytes(1_500_000_000_000), "1.5 TB");
    }

    #[test]
    fn small_and_zero_sizes_do_not_panic() {
        assert_eq!(fmt_bytes(0), "0 GB");
        assert_eq!(fmt_bytes(1), "0 GB");
    }

    // ── parse_gb ──────────────────────────────────────────────────────────────

    /// `parse_gb` reads back what `fmt_bytes` wrote, so the two have to agree —
    /// this is what orders the disks when choosing which to recommend.
    #[test]
    fn parse_gb_round_trips_what_fmt_bytes_produces() {
        assert_eq!(parse_gb(&fmt_bytes(500_000_000_000)), 500);
        assert_eq!(parse_gb(&fmt_bytes(2_000_000_000_000)), 2000);
        assert_eq!(parse_gb(&fmt_bytes(1_500_000_000_000)), 1500);
    }

    #[test]
    fn parse_gb_orders_terabyte_disks_above_gigabyte_disks() {
        assert!(parse_gb("1.0 TB") > parse_gb("999 GB"));
    }

    #[test]
    fn parse_gb_reads_unparseable_sizes_as_zero() {
        assert_eq!(parse_gb(""), 0);
        assert_eq!(parse_gb("unknown"), 0);
        assert_eq!(parse_gb("GB"), 0);
    }

    // ── disk_has_mount ────────────────────────────────────────────────────────

    #[test]
    fn a_disk_mounted_at_its_top_level_counts_as_mounted() {
        assert!(disk_has_mount(&json!({"mountpoint": "/"})));
    }

    /// The live USB's root is on a *partition*, not the disk node, so only
    /// recursing into children detects it — and that disk is the one thing the
    /// installer must never offer to erase.
    #[test]
    fn a_disk_is_mounted_when_any_partition_below_it_is() {
        let disk = json!({
            "name": "sda", "mountpoint": null,
            "children": [
                {"name": "sda1", "mountpoint": null},
                {"name": "sda2", "mountpoint": "/iso"},
            ],
        });
        assert!(disk_has_mount(&disk));
    }

    #[test]
    fn nesting_deeper_than_one_level_is_still_detected() {
        // e.g. disk → partition → LUKS/LVM mapping that holds the mount.
        let disk = json!({
            "children": [{"children": [{"mountpoint": "/nix/store"}]}],
        });
        assert!(disk_has_mount(&disk));
    }

    #[test]
    fn an_unmounted_disk_is_not_reported_as_mounted() {
        assert!(!disk_has_mount(&json!({"mountpoint": null})));
        assert!(!disk_has_mount(&json!({"mountpoint": ""})));
        assert!(!disk_has_mount(&json!({})));
        assert!(!disk_has_mount(&json!({
            "children": [{"mountpoint": null}, {"mountpoint": ""}]
        })));
    }

    // ── disks_from_lsblk ──────────────────────────────────────────────────────

    fn lsblk(devices: serde_json::Value) -> serde_json::Value {
        json!({"blockdevices": devices})
    }

    fn disk(name: &str, size: u64, tran: &str, mountpoint: Option<&str>) -> serde_json::Value {
        json!({
            "name": name, "size": size, "tran": tran, "type": "disk",
            "mountpoint": mountpoint,
        })
    }

    #[test]
    fn disks_are_listed_with_full_device_paths() {
        let disks = disks_from_lsblk(&lsblk(json!([disk("nvme0n1", 512_000_000_000, "nvme", None)])));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/nvme0n1");
        assert_eq!(disks[0].size, "512 GB");
        assert_eq!(disks[0].tran, "nvme");
    }

    #[test]
    fn partitions_and_loop_devices_are_not_offered_as_install_targets() {
        let data = lsblk(json!([
            disk("sda", 500_000_000_000, "sata", None),
            {"name": "sda1", "size": 1_000_000, "type": "part"},
            {"name": "loop0", "size": 1_000_000, "type": "loop"},
            {"name": "sr0", "size": 1_000_000, "type": "rom"},
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sda");
    }

    #[test]
    fn the_largest_internal_disk_is_recommended() {
        let data = lsblk(json!([
            disk("sda", 250_000_000_000, "sata", None),
            disk("nvme0n1", 1_000_000_000_000, "nvme", None),
            disk("sdb", 500_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks.iter().filter(|d| d.recommended).map(|d| d.name.as_str()).collect();
        assert_eq!(rec, vec!["/dev/nvme0n1"]);
    }

    /// The installer is running *from* the USB stick. Recommending it would mean
    /// the default action destroys the installer mid-install.
    #[test]
    fn the_usb_stick_is_never_recommended_even_when_it_is_the_largest() {
        let data = lsblk(json!([
            disk("sdb", 2_000_000_000_000, "usb", None),
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks.iter().filter(|d| d.recommended).map(|d| d.name.as_str()).collect();
        assert_eq!(rec, vec!["/dev/sda"]);
        // It is still listed, just not preselected — an advanced user may want it.
        assert_eq!(disks.len(), 2);
        assert!(disks.iter().any(|d| d.name == "/dev/sdb" && d.is_usb));
    }

    #[test]
    fn a_mounted_disk_is_never_recommended() {
        let data = lsblk(json!([
            json!({
                "name": "sdb", "size": 2_000_000_000_000u64, "tran": "sata", "type": "disk",
                "children": [{"name": "sdb1", "mountpoint": "/iso"}],
            }),
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks.iter().filter(|d| d.recommended).map(|d| d.name.as_str()).collect();
        assert_eq!(rec, vec!["/dev/sda"]);
    }

    /// With nothing safe to pick, nothing is preselected — better an explicit
    /// choice than a default that erases the wrong device.
    #[test]
    fn nothing_is_recommended_when_every_disk_is_usb_or_mounted() {
        let data = lsblk(json!([
            disk("sdb", 2_000_000_000_000, "usb", None),
            disk("sdc", 1_000_000_000_000, "usb", Some("/iso")),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 2);
        assert!(!disks.iter().any(|d| d.recommended));
    }

    #[test]
    fn exactly_one_disk_is_ever_recommended() {
        let data = lsblk(json!([
            disk("sda", 500_000_000_000, "sata", None),
            disk("sdb", 500_000_000_000, "sata", None),
            disk("sdc", 500_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.iter().filter(|d| d.recommended).count(), 1);
    }

    #[test]
    fn a_machine_with_no_disks_yields_an_empty_list() {
        assert!(disks_from_lsblk(&lsblk(json!([]))).is_empty());
        assert!(disks_from_lsblk(&json!({})).is_empty());
        assert!(disks_from_lsblk(&json!({"blockdevices": "unexpected"})).is_empty());
    }

    #[test]
    fn a_disk_with_no_name_is_skipped_rather_than_listed_as_dev() {
        let data = lsblk(json!([
            {"size": 500_000_000_000u64, "type": "disk"},
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sda");
    }

    #[test]
    fn a_disk_with_no_reported_size_still_appears() {
        // Some virtio/NVMe setups omit `size`; hiding the disk entirely would
        // leave a user with no installable target at all.
        let data = lsblk(json!([{"name": "vda", "type": "disk"}]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].size, "0 GB");
        assert_eq!(disks[0].tran, "");
    }
}
