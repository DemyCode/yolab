use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

use crate::effects::Effect;
use crate::install::InstallParams;

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

#[derive(Debug, Clone, PartialEq)]
pub enum ClusterMode {
    New,
    Join,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskInfo {
    pub name: String,
    pub size: String,
    pub tran: String,
    pub is_usb: bool,
    pub mounted: bool,
    pub recommended: bool,
}

#[derive(Debug, PartialEq)]
pub enum AppEvent {
    NetworkReady,
    AccountCreated(String),
    AccountVerified,
    DisksLoaded(Vec<DiskInfo>),
    JoinConnected {
        server_addr: String,
        k3s_token: String,
        ceph_fsid: String,
    },
    SshKeyGenerated {
        private_key: String,
        public_key: String,
    },
    Log(String),
    InstallComplete {
        url: String,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClickTarget {
    ModeOption(usize),
    AcctMethod(u8),
    JoinField(u8),
    DiskOption(usize),
    CfgField(u8),
    Btn(BtnId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BtnId {
    Continue,
    CreateAcct,
    VerifyToken,
    Connect,
    GenSshKey,
    Reboot,
    Poweroff,
}

pub const REFERRAL_CODE_MAX: usize = 32;
const CONFIG_FIELDS: u8 = 4;

pub fn boot_mode(has_efi: bool) -> &'static str {
    if has_efi {
        "uefi"
    } else {
        "bios"
    }
}

pub struct App {
    pub step: Step,

    pub mode: Option<ClusterMode>,
    pub mode_cursor: usize,

    pub acct_cursor: u8,
    pub acct_input: String,
    pub acct_referral: String,
    pub account_token: Option<String>,
    pub created_token: Option<String>,
    pub join_url: String,
    pub join_pass: String,
    pub join_field: u8,

    pub disks: Vec<DiskInfo>,
    pub disk_cursor: usize,

    pub timezone: String,
    pub password: String,
    pub password2: String,
    pub ssh_pub: String,
    pub cfg_field: u8,
    pub gen_privkey: Option<String>,

    pub join_server_addr: Option<String>,
    pub join_k3s_token: Option<String>,
    pub join_ceph_fsid: Option<String>,

    pub log_lines: Vec<String>,
    pub install_done: bool,
    pub install_failed: bool,
    pub mgmt_url: Option<String>,

    pub network_ready: bool,
    pub boot_mode: String,

    pub error: Option<String>,
    pub loading: bool,
    pub loading_msg: String,
    pub click_areas: Vec<(ratatui::layout::Rect, ClickTarget)>,

    pub effects: Vec<Effect>,
}

impl App {
    pub fn new(boot_mode: &str) -> Self {
        Self {
            step: Step::Mode,
            mode: None,
            mode_cursor: 0,
            acct_cursor: 0,
            acct_input: String::new(),
            acct_referral: String::new(),
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
            join_ceph_fsid: None,
            log_lines: vec![],
            install_done: false,
            install_failed: false,
            mgmt_url: None,
            network_ready: false,
            boot_mode: boot_mode.to_string(),
            error: None,
            loading: true,
            loading_msg: "Waiting for network…".into(),
            click_areas: vec![],
            effects: vec![],
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.loading {
            return false;
        }

        self.error = None;

        match self.step {
            Step::Mode => self.key_mode(key),
            Step::Account => self.key_account(key),
            Step::Disk => self.key_disk(key),
            Step::Configure => self.key_configure(key),
            Step::Install => self.key_install(key),
        }
        false
    }

    fn key_install(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('r') if self.install_done => self.power("reboot"),
            KeyCode::Char('p') if self.install_done => self.power("poweroff"),
            _ => {}
        }
    }

    fn key_mode(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode_cursor = self.mode_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => self.mode_cursor = 1,
            KeyCode::Enter => self.confirm_mode(),
            _ => {}
        }
    }

    fn key_account(&mut self, key: KeyEvent) {
        match self.mode {
            Some(ClusterMode::New) => self.key_account_new(key),
            Some(ClusterMode::Join) => self.key_account_join(key),
            None => {}
        }
    }

    fn key_account_new(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.step = Step::Mode,
            KeyCode::Tab | KeyCode::BackTab => self.acct_cursor = 1 - self.acct_cursor,
            KeyCode::Enter => self.submit_account(),
            KeyCode::Char(c) if self.account_token.is_none() => {
                if self.acct_cursor == 1 {
                    self.acct_input.push(c);
                } else if self.acct_referral.chars().count() < REFERRAL_CODE_MAX {
                    self.acct_referral.push(c);
                }
            }
            KeyCode::Backspace => {
                if self.acct_cursor == 1 {
                    self.acct_input.pop();
                } else {
                    self.acct_referral.pop();
                }
            }
            _ => {}
        }
    }

    fn key_account_join(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.step = Step::Mode,
            KeyCode::Tab | KeyCode::BackTab => self.join_field = 1 - self.join_field,
            KeyCode::Enter => self.fetch_join_info(),
            KeyCode::Char(c) => self.join_input().push(c),
            KeyCode::Backspace => {
                self.join_input().pop();
            }
            _ => {}
        }
    }

    fn join_input(&mut self) -> &mut String {
        if self.join_field == 0 {
            &mut self.join_url
        } else {
            &mut self.join_pass
        }
    }

    fn key_disk(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.step = Step::Account,
            KeyCode::Up | KeyCode::Char('k') => {
                self.disk_cursor = self.disk_cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.disk_cursor + 1 < self.disks.len() {
                    self.disk_cursor += 1;
                }
            }
            KeyCode::Enter => self.confirm_disk(),
            _ => {}
        }
    }

    fn key_configure(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.step = Step::Disk,
            KeyCode::Tab => self.cfg_field = (self.cfg_field + 1) % CONFIG_FIELDS,
            KeyCode::BackTab => {
                self.cfg_field = (self.cfg_field + CONFIG_FIELDS - 1) % CONFIG_FIELDS
            }
            KeyCode::Char(c) => self.cfg_input().push(c),
            KeyCode::Backspace => {
                self.cfg_input().pop();
            }
            KeyCode::Enter => self.confirm_configure(),
            _ => {}
        }
    }

    fn cfg_input(&mut self) -> &mut String {
        match self.cfg_field {
            0 => &mut self.timezone,
            1 => &mut self.password,
            2 => &mut self.password2,
            _ => &mut self.ssh_pub,
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) {
        if self.loading || m.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let pos = Position {
            x: m.column,
            y: m.row,
        };
        let target = self
            .click_areas
            .iter()
            .find(|(rect, _)| rect.contains(pos))
            .map(|(_, t)| t.clone());
        if let Some(target) = target {
            self.error = None;
            self.handle_click(target);
        }
    }

    fn handle_click(&mut self, target: ClickTarget) {
        match target {
            ClickTarget::ModeOption(n) => {
                self.mode_cursor = n;
                self.confirm_mode();
            }
            ClickTarget::AcctMethod(n) => self.acct_cursor = n,
            ClickTarget::JoinField(n) => self.join_field = n,
            ClickTarget::DiskOption(n) => self.disk_cursor = n,
            ClickTarget::CfgField(n) => self.cfg_field = n,
            ClickTarget::Btn(id) => self.handle_btn(id),
        }
    }

    fn handle_btn(&mut self, id: BtnId) {
        match id {
            BtnId::Continue => match self.step {
                Step::Account => self.submit_account(),
                Step::Disk => self.confirm_disk(),
                Step::Configure => self.confirm_configure(),
                Step::Mode | Step::Install => {}
            },
            BtnId::CreateAcct => self.create_account(),
            BtnId::VerifyToken => self.verify_token(),
            BtnId::Connect => self.fetch_join_info(),
            BtnId::GenSshKey => self.start(Effect::GenSshKey, "Generating SSH key…"),
            BtnId::Reboot => self.power("reboot"),
            BtnId::Poweroff => self.power("poweroff"),
        }
    }

    pub fn handle_app_event(&mut self, ev: AppEvent) {
        self.loading = false;
        match ev {
            AppEvent::NetworkReady => self.network_ready = true,
            AppEvent::AccountCreated(token) => {
                self.created_token = Some(token.clone());
                self.account_token = Some(token);
            }
            AppEvent::AccountVerified => {
                self.account_token = Some(self.acct_input.trim().to_string());
            }
            AppEvent::DisksLoaded(disks) => {
                self.disk_cursor = disks.iter().position(|d| d.recommended).unwrap_or(0);
                self.disks = disks;
            }
            AppEvent::JoinConnected {
                server_addr,
                k3s_token,
                ceph_fsid,
            } => {
                self.join_server_addr = Some(server_addr);
                self.join_k3s_token = Some(k3s_token);
                self.join_ceph_fsid = Some(ceph_fsid);
                if self.password.is_empty() {
                    self.password = self.join_pass.clone();
                    self.password2 = self.join_pass.clone();
                }
                self.goto_disk();
            }
            AppEvent::SshKeyGenerated {
                private_key,
                public_key,
            } => {
                self.gen_privkey = Some(private_key);
                self.ssh_pub = public_key;
            }
            AppEvent::Log(line) => self.log_lines.push(line),
            AppEvent::InstallComplete { url } => {
                self.install_done = true;
                self.mgmt_url = Some(url);
            }
            AppEvent::Failed(msg) => {
                self.install_failed = self.step == Step::Install;
                self.error = Some(msg);
            }
        }
    }

    fn start(&mut self, effect: Effect, msg: &str) {
        self.loading = true;
        self.loading_msg = msg.to_string();
        self.effects.push(effect);
    }

    fn confirm_mode(&mut self) {
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

    fn submit_account(&mut self) {
        if self.account_token.is_some() {
            self.goto_disk();
        } else if self.mode == Some(ClusterMode::Join) {
            self.fetch_join_info();
        } else if self.acct_cursor == 0 {
            self.create_account();
        } else {
            self.verify_token();
        }
    }

    fn create_account(&mut self) {
        let referral = self.acct_referral.clone();
        self.start(Effect::CreateAccount { referral }, "Creating account…");
    }

    fn verify_token(&mut self) {
        let token = self.acct_input.trim().to_string();
        if token.is_empty() {
            self.error = Some("Please paste your account token.".into());
            return;
        }
        self.start(Effect::VerifyToken { token }, "Verifying token…");
    }

    fn fetch_join_info(&mut self) {
        let url = self.join_url.trim().to_string();
        if url.is_empty() {
            self.error = Some("Node URL is required.".into());
            return;
        }
        let password = self.join_pass.clone();
        self.start(
            Effect::FetchJoinInfo { url, password },
            "Connecting to existing node…",
        );
    }

    fn goto_disk(&mut self) {
        self.step = Step::Disk;
        if self.disks.is_empty() {
            self.start(Effect::DetectDisks, "Detecting disks…");
        }
    }

    fn confirm_disk(&mut self) {
        if let Some(d) = self.disks.get(self.disk_cursor) {
            if d.mounted {
                self.error = Some("That disk is in use — choose another.".into());
            } else {
                self.step = Step::Configure;
            }
        }
    }

    fn confirm_configure(&mut self) {
        match password_problem(&self.password, &self.password2) {
            Some(problem) => self.error = Some(problem.into()),
            None => self.start_install(),
        }
    }

    fn start_install(&mut self) {
        self.step = Step::Install;
        self.log_lines.clear();
        let params = InstallParams {
            disk: self
                .disks
                .get(self.disk_cursor)
                .map(|d| d.name.clone())
                .unwrap_or_default(),
            timezone: self.timezone.trim().to_string(),
            password: self.password.clone(),
            root_ssh_key: self.ssh_pub.trim().to_string(),
            account_token: self.account_token.clone().unwrap_or_default(),
            server_addr: self.join_server_addr.clone(),
            k3s_token: self.join_k3s_token.clone(),
            ceph_fsid: self.join_ceph_fsid.clone(),
            boot_mode: self.boot_mode.clone(),
        };
        self.effects.push(Effect::Install(params));
    }

    fn power(&mut self, action: &'static str) {
        self.effects.push(Effect::Power(action));
    }
}

fn password_problem(password: &str, confirm: &str) -> Option<&'static str> {
    if password.chars().count() < 8 {
        Some("Password must be at least 8 characters.")
    } else if !password.chars().any(char::is_uppercase) {
        Some("Password must contain at least one uppercase letter.")
    } else if !password.chars().any(char::is_lowercase) {
        Some("Password must contain at least one lowercase letter.")
    } else if !password.chars().any(|c| c.is_ascii_digit()) {
        Some("Password must contain at least one number.")
    } else if password != confirm {
        Some("Passwords do not match.")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn ready() -> App {
        let mut app = App::new("uefi");
        app.loading = false;
        app.network_ready = true;
        app
    }

    fn press(app: &mut App, codes: &[KeyCode]) {
        for c in codes {
            assert!(!app.handle_key(key(*c)));
        }
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn at_account(mode: ClusterMode) -> App {
        let mut app = ready();
        app.mode = Some(mode);
        app.step = Step::Account;
        app
    }

    fn disk(name: &str, mounted: bool, recommended: bool) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            size: "500 GB".into(),
            tran: "nvme".into(),
            is_usb: false,
            mounted,
            recommended,
        }
    }

    fn at_disk(disks: Vec<DiskInfo>) -> App {
        let mut app = ready();
        app.step = Step::Disk;
        app.disks = disks;
        app
    }

    fn click(app: &mut App, target: ClickTarget) {
        app.click_areas = vec![(
            Rect {
                x: 10,
                y: 10,
                width: 5,
                height: 2,
            },
            target,
        )];
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 11, 11));
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn step_indices_are_sequential_and_unique() {
        let steps = [
            Step::Mode,
            Step::Account,
            Step::Disk,
            Step::Configure,
            Step::Install,
        ];
        let indices: Vec<usize> = steps.iter().map(Step::index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_boot_mode_follows_the_firmware() {
        assert_eq!(boot_mode(true), "uefi");
        assert_eq!(boot_mode(false), "bios");
    }

    #[test]
    fn the_installer_starts_waiting_for_the_network() {
        let app = App::new("bios");
        assert_eq!(app.step, Step::Mode);
        assert!(app.loading);
        assert_eq!(app.loading_msg, "Waiting for network…");
        assert_eq!(app.boot_mode, "bios");
        assert_eq!(app.timezone, "UTC");
        assert!(app.effects.is_empty());
    }

    #[test]
    fn ctrl_c_quits_even_while_loading() {
        let mut app = App::new("uefi");
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!App::new("uefi").handle_key(key(KeyCode::Char('c'))));
    }

    #[test]
    fn keys_do_nothing_while_loading() {
        let mut app = App::new("uefi");
        app.error = Some("kept".into());
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(app.mode_cursor, 0);
        assert_eq!(app.error.as_deref(), Some("kept"));
    }

    #[test]
    fn any_key_clears_the_previous_error() {
        let mut app = ready();
        app.error = Some("old".into());
        press(&mut app, &[KeyCode::Left]);
        assert_eq!(app.error, None);
    }

    #[test]
    fn the_mode_cursor_moves_between_the_two_setups() {
        let mut app = ready();
        press(&mut app, &[KeyCode::Down]);
        assert_eq!(app.mode_cursor, 1);
        press(&mut app, &[KeyCode::Char('j')]);
        assert_eq!(app.mode_cursor, 1);
        press(&mut app, &[KeyCode::Up]);
        assert_eq!(app.mode_cursor, 0);
        press(&mut app, &[KeyCode::Char('k')]);
        assert_eq!(app.mode_cursor, 0);
    }

    #[test]
    fn a_setup_cannot_be_chosen_before_the_network_is_up() {
        let mut app = ready();
        app.network_ready = false;
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Mode);
        assert_eq!(
            app.error.as_deref(),
            Some("Network not ready — waiting for DNS…")
        );
    }

    #[test]
    fn choosing_a_setup_moves_to_the_account_step() {
        let mut app = ready();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            (app.step.clone(), app.mode.clone()),
            (Step::Account, Some(ClusterMode::New))
        );

        let mut app = ready();
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(app.mode, Some(ClusterMode::Join));
    }

    #[test]
    fn typing_on_the_create_tab_fills_the_referral_code_not_the_token() {
        let mut app = at_account(ClusterMode::New);
        typed(&mut app, "ab3k");
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(app.acct_referral, "ab3");
        assert!(app.acct_input.is_empty());
    }

    #[test]
    fn a_referral_code_stops_at_its_maximum_length() {
        let mut app = at_account(ClusterMode::New);
        typed(&mut app, &"é".repeat(REFERRAL_CODE_MAX + 5));
        assert_eq!(app.acct_referral.chars().count(), REFERRAL_CODE_MAX);
    }

    #[test]
    fn tab_switches_between_creating_and_pasting_a_token() {
        let mut app = at_account(ClusterMode::New);
        press(&mut app, &[KeyCode::Tab]);
        assert_eq!(app.acct_cursor, 1);
        typed(&mut app, "tok9");
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(app.acct_input, "tok");
        assert!(app.acct_referral.is_empty());
        press(&mut app, &[KeyCode::BackTab]);
        assert_eq!(app.acct_cursor, 0);
    }

    #[test]
    fn nothing_can_be_typed_once_an_account_is_connected() {
        let mut app = at_account(ClusterMode::New);
        app.account_token = Some("t".into());
        typed(&mut app, "x");
        press(&mut app, &[KeyCode::Tab]);
        typed(&mut app, "y");
        assert!(app.acct_referral.is_empty() && app.acct_input.is_empty());
    }

    #[test]
    fn enter_on_the_create_tab_creates_an_account() {
        let mut app = at_account(ClusterMode::New);
        typed(&mut app, "AB3");
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.effects,
            vec![Effect::CreateAccount {
                referral: "AB3".into()
            }]
        );
        assert!(app.loading);
        assert_eq!(app.loading_msg, "Creating account…");
    }

    #[test]
    fn a_pasted_token_is_verified_trimmed() {
        let mut app = at_account(ClusterMode::New);
        app.acct_cursor = 1;
        typed(&mut app, "  tok  ");
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.effects,
            vec![Effect::VerifyToken {
                token: "tok".into()
            }]
        );
        assert_eq!(app.loading_msg, "Verifying token…");
    }

    #[test]
    fn an_empty_token_is_refused_without_asking_the_platform() {
        let mut app = at_account(ClusterMode::New);
        app.acct_cursor = 1;
        typed(&mut app, "   ");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.effects.is_empty());
        assert!(!app.loading);
        assert_eq!(
            app.error.as_deref(),
            Some("Please paste your account token.")
        );
    }

    #[test]
    fn a_connected_account_continues_to_the_disks() {
        let mut app = at_account(ClusterMode::New);
        app.account_token = Some("t".into());
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Disk);
        assert_eq!(app.effects, vec![Effect::DetectDisks]);
        assert_eq!(app.loading_msg, "Detecting disks…");
    }

    #[test]
    fn disks_already_found_are_not_detected_again() {
        let mut app = at_account(ClusterMode::New);
        app.account_token = Some("t".into());
        app.disks = vec![disk("/dev/sda", false, true)];
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Disk);
        assert!(app.effects.is_empty());
        assert!(!app.loading);
    }

    #[test]
    fn escape_goes_back_one_step() {
        let mut app = at_account(ClusterMode::New);
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.step, Step::Mode);

        let mut app = at_account(ClusterMode::Join);
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.step, Step::Mode);

        let mut app = at_disk(vec![]);
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.step, Step::Account);

        let mut app = ready();
        app.step = Step::Configure;
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.step, Step::Disk);
    }

    #[test]
    fn without_a_mode_the_account_step_ignores_keys() {
        let mut app = ready();
        app.step = Step::Account;
        typed(&mut app, "abc");
        press(&mut app, &[KeyCode::Enter, KeyCode::Esc]);
        assert_eq!(app.step, Step::Account);
        assert!(app.effects.is_empty());
    }

    #[test]
    fn unused_keys_on_the_create_tab_change_nothing() {
        let mut app = at_account(ClusterMode::New);
        press(&mut app, &[KeyCode::Left, KeyCode::F(1)]);
        assert_eq!(app.step, Step::Account);
        assert!(app.effects.is_empty());
    }

    #[test]
    fn joining_takes_a_url_and_a_password_in_turn() {
        let mut app = at_account(ClusterMode::Join);
        typed(&mut app, "https://n1x");
        press(&mut app, &[KeyCode::Backspace, KeyCode::Tab]);
        typed(&mut app, "pw1");
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(
            (app.join_url.as_str(), app.join_pass.as_str()),
            ("https://n1", "pw")
        );
        press(&mut app, &[KeyCode::BackTab]);
        assert_eq!(app.join_field, 0);
        press(&mut app, &[KeyCode::Left]);
        assert_eq!(app.join_url, "https://n1");
    }

    #[test]
    fn joining_needs_a_url() {
        let mut app = at_account(ClusterMode::Join);
        app.join_field = 1;
        typed(&mut app, "pw");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.effects.is_empty());
        assert_eq!(app.error.as_deref(), Some("Node URL is required."));
    }

    #[test]
    fn joining_connects_with_the_trimmed_url_and_the_password_as_typed() {
        let mut app = at_account(ClusterMode::Join);
        app.join_url = " https://n1 ".into();
        app.join_pass = " pw ".into();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.effects,
            vec![Effect::FetchJoinInfo {
                url: "https://n1".into(),
                password: " pw ".into()
            }]
        );
        assert_eq!(app.loading_msg, "Connecting to existing node…");
    }

    #[test]
    fn the_disk_cursor_stays_on_the_list() {
        let mut app = at_disk(vec![
            disk("/dev/a", false, false),
            disk("/dev/b", false, false),
        ]);
        press(
            &mut app,
            &[KeyCode::Down, KeyCode::Char('j'), KeyCode::Down],
        );
        assert_eq!(app.disk_cursor, 1);
        press(&mut app, &[KeyCode::Up, KeyCode::Char('k'), KeyCode::Up]);
        assert_eq!(app.disk_cursor, 0);

        let mut app = at_disk(vec![]);
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!((app.disk_cursor, app.step.clone()), (0, Step::Disk));
    }

    #[test]
    fn a_disk_in_use_cannot_be_chosen() {
        let mut app = at_disk(vec![disk("/dev/a", true, false)]);
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Disk);
        assert_eq!(
            app.error.as_deref(),
            Some("That disk is in use — choose another.")
        );
    }

    #[test]
    fn a_free_disk_moves_on_to_configuration() {
        let mut app = at_disk(vec![disk("/dev/a", false, false)]);
        press(&mut app, &[KeyCode::Left, KeyCode::Enter]);
        assert_eq!(app.step, Step::Configure);
    }

    fn at_configure() -> App {
        let mut app = at_disk(vec![
            disk("/dev/a", false, false),
            disk("/dev/b", false, true),
        ]);
        app.disk_cursor = 1;
        app.step = Step::Configure;
        app.timezone.clear();
        app
    }

    #[test]
    fn tab_cycles_through_the_four_fields_both_ways() {
        let mut app = at_configure();
        let mut seen = vec![];
        for _ in 0..5 {
            seen.push(app.cfg_field);
            press(&mut app, &[KeyCode::Tab]);
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 0]);
        press(&mut app, &[KeyCode::BackTab]);
        assert_eq!(app.cfg_field, 0);
        press(&mut app, &[KeyCode::BackTab]);
        assert_eq!(app.cfg_field, 3);
        press(&mut app, &[KeyCode::BackTab]);
        assert_eq!(app.cfg_field, 2);
    }

    #[test]
    fn each_field_takes_its_own_typing() {
        let mut app = at_configure();
        for (field, text) in [
            "Europe/Parisx",
            "Passw0rdx",
            "Passw0rdx",
            " ssh-ed25519 Kx ",
        ]
        .iter()
        .enumerate()
        {
            app.cfg_field = field as u8;
            typed(&mut app, text);
            press(&mut app, &[KeyCode::Backspace]);
        }
        press(&mut app, &[KeyCode::F(2)]);
        assert_eq!(
            [
                app.timezone.as_str(),
                app.password.as_str(),
                app.password2.as_str(),
                app.ssh_pub.as_str()
            ],
            ["Europe/Paris", "Passw0rd", "Passw0rd", " ssh-ed25519 Kx"]
        );
    }

    #[test]
    fn a_weak_or_mismatched_password_names_the_problem() {
        let cases = [
            ("Ab1", "Ab1", "Password must be at least 8 characters."),
            (
                "abcdefg1",
                "abcdefg1",
                "Password must contain at least one uppercase letter.",
            ),
            (
                "ABCDEFG1",
                "ABCDEFG1",
                "Password must contain at least one lowercase letter.",
            ),
            (
                "Abcdefgh",
                "Abcdefgh",
                "Password must contain at least one number.",
            ),
            ("Abcdefg1", "Abcdefg2", "Passwords do not match."),
        ];
        for (password, confirm, problem) in cases {
            assert_eq!(
                password_problem(password, confirm),
                Some(problem),
                "{password}"
            );
        }
        assert_eq!(password_problem("Abcdefg1", "Abcdefg1"), None);
        assert_eq!(
            password_problem("Abcdéf1", "Abcdéf1"),
            Some("Password must be at least 8 characters.")
        );
    }

    #[test]
    fn a_rejected_password_does_not_start_the_install() {
        let mut app = at_configure();
        app.password = "short".into();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Configure);
        assert_eq!(
            app.error.as_deref(),
            Some("Password must be at least 8 characters.")
        );
        assert!(app.effects.is_empty());
    }

    #[test]
    fn a_good_password_starts_the_install_with_everything_chosen() {
        let mut app = at_configure();
        app.timezone = " Europe/Paris ".into();
        app.password = "Passw0rd".into();
        app.password2 = "Passw0rd".into();
        app.ssh_pub = " ssh-ed25519 K ".into();
        app.account_token = Some("tok".into());
        app.join_server_addr = Some("https://[fd00::1]:6443".into());
        app.join_k3s_token = Some("K10".into());
        app.join_ceph_fsid = Some("fsid".into());
        app.log_lines = vec!["old".into()];
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.step, Step::Install);
        assert!(app.log_lines.is_empty());
        assert_eq!(
            app.effects,
            vec![Effect::Install(InstallParams {
                disk: "/dev/b".into(),
                timezone: "Europe/Paris".into(),
                password: "Passw0rd".into(),
                root_ssh_key: "ssh-ed25519 K".into(),
                account_token: "tok".into(),
                server_addr: Some("https://[fd00::1]:6443".into()),
                k3s_token: Some("K10".into()),
                ceph_fsid: Some("fsid".into()),
                boot_mode: "uefi".into(),
            })]
        );
    }

    #[test]
    fn an_install_without_a_disk_or_account_still_carries_empty_fields() {
        let mut app = at_configure();
        app.disks.clear();
        app.password = "Passw0rd".into();
        app.password2 = "Passw0rd".into();
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.effects,
            vec![Effect::Install(InstallParams {
                disk: String::new(),
                timezone: String::new(),
                password: "Passw0rd".into(),
                root_ssh_key: String::new(),
                account_token: String::new(),
                server_addr: None,
                k3s_token: None,
                ceph_fsid: None,
                boot_mode: "uefi".into(),
            })]
        );
    }

    #[test]
    fn reboot_and_power_off_wait_for_the_install_to_finish() {
        let mut app = ready();
        app.step = Step::Install;
        typed(&mut app, "rpx");
        assert!(app.effects.is_empty());
        app.install_done = true;
        typed(&mut app, "xrp");
        assert_eq!(
            app.effects,
            vec![Effect::Power("reboot"), Effect::Power("poweroff")]
        );
    }

    #[test]
    fn clicks_are_ignored_while_loading_and_for_anything_but_a_left_press() {
        let mut app = ready();
        app.click_areas = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 5,
            },
            ClickTarget::ModeOption(1),
        )];
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), 1, 1));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 1));
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 9, 9));
        assert_eq!(app.step, Step::Mode);
        app.loading = true;
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        assert_eq!(app.step, Step::Mode);
        app.loading = false;
        app.error = Some("old".into());
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        assert_eq!(app.mode, Some(ClusterMode::Join));
        assert_eq!(app.error, None);
    }

    #[test]
    fn clicking_a_field_or_option_selects_it() {
        let mut app = ready();
        click(&mut app, ClickTarget::AcctMethod(1));
        click(&mut app, ClickTarget::JoinField(1));
        click(&mut app, ClickTarget::DiskOption(2));
        click(&mut app, ClickTarget::CfgField(3));
        assert_eq!(
            (
                app.acct_cursor,
                app.join_field,
                app.disk_cursor,
                app.cfg_field
            ),
            (1, 1, 2, 3)
        );
    }

    #[test]
    fn continue_does_what_enter_does_on_each_step() {
        let mut app = at_account(ClusterMode::New);
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(
            app.effects,
            vec![Effect::CreateAccount {
                referral: String::new()
            }]
        );

        let mut app = at_account(ClusterMode::New);
        app.acct_cursor = 1;
        app.acct_input = "t".into();
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(app.effects, vec![Effect::VerifyToken { token: "t".into() }]);

        let mut app = at_account(ClusterMode::Join);
        app.join_url = "u".into();
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(
            app.effects,
            vec![Effect::FetchJoinInfo {
                url: "u".into(),
                password: String::new()
            }]
        );

        let mut app = at_account(ClusterMode::Join);
        app.account_token = Some("t".into());
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(app.step, Step::Disk);

        let mut app = at_disk(vec![disk("/dev/a", false, false)]);
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(app.step, Step::Configure);

        let mut app = at_configure();
        click(&mut app, ClickTarget::Btn(BtnId::Continue));
        assert_eq!(
            app.error.as_deref(),
            Some("Password must be at least 8 characters.")
        );

        for step in [Step::Mode, Step::Install] {
            let mut app = ready();
            app.step = step.clone();
            click(&mut app, ClickTarget::Btn(BtnId::Continue));
            assert_eq!(app.step, step);
            assert!(app.effects.is_empty());
        }
    }

    #[test]
    fn each_button_starts_its_own_action() {
        let cases = [
            (
                BtnId::CreateAcct,
                Effect::CreateAccount {
                    referral: "r".into(),
                },
            ),
            (
                BtnId::VerifyToken,
                Effect::VerifyToken { token: "t".into() },
            ),
            (
                BtnId::Connect,
                Effect::FetchJoinInfo {
                    url: "u".into(),
                    password: "p".into(),
                },
            ),
            (BtnId::GenSshKey, Effect::GenSshKey),
            (BtnId::Reboot, Effect::Power("reboot")),
            (BtnId::Poweroff, Effect::Power("poweroff")),
        ];
        for (btn, effect) in cases {
            let mut app = ready();
            app.acct_referral = "r".into();
            app.acct_input = "t".into();
            app.join_url = "u".into();
            app.join_pass = "p".into();
            click(&mut app, ClickTarget::Btn(btn.clone()));
            assert_eq!(app.effects, vec![effect], "{btn:?}");
        }
        let mut app = ready();
        click(&mut app, ClickTarget::Btn(BtnId::GenSshKey));
        assert_eq!(app.loading_msg, "Generating SSH key…");
    }

    #[test]
    fn every_answer_ends_the_wait() {
        let mut app = App::new("uefi");
        app.handle_app_event(AppEvent::NetworkReady);
        assert!(app.network_ready && !app.loading);

        let mut app = App::new("uefi");
        app.handle_app_event(AppEvent::Log("x".into()));
        assert!(!app.loading);
        assert_eq!(app.log_lines, vec!["x"]);
    }

    #[test]
    fn a_created_account_is_both_shown_and_used() {
        let mut app = ready();
        app.handle_app_event(AppEvent::AccountCreated("tok".into()));
        assert_eq!(app.created_token.as_deref(), Some("tok"));
        assert_eq!(app.account_token.as_deref(), Some("tok"));
    }

    #[test]
    fn a_verified_token_is_used_trimmed_but_not_shown_as_new() {
        let mut app = ready();
        app.acct_input = " tok ".into();
        app.handle_app_event(AppEvent::AccountVerified);
        assert_eq!(app.account_token.as_deref(), Some("tok"));
        assert_eq!(app.created_token, None);
    }

    #[test]
    fn loaded_disks_start_on_the_recommended_one() {
        let mut app = ready();
        app.handle_app_event(AppEvent::DisksLoaded(vec![
            disk("/dev/a", false, false),
            disk("/dev/b", false, true),
        ]));
        assert_eq!(app.disk_cursor, 1);
        assert_eq!(app.disks.len(), 2);

        app.disk_cursor = 5;
        app.handle_app_event(AppEvent::DisksLoaded(vec![disk("/dev/a", false, false)]));
        assert_eq!(app.disk_cursor, 0);
    }

    fn joined() -> AppEvent {
        AppEvent::JoinConnected {
            server_addr: "s".into(),
            k3s_token: "k".into(),
            ceph_fsid: "f".into(),
        }
    }

    #[test]
    fn a_joined_cluster_reuses_its_password_and_moves_to_the_disks() {
        let mut app = at_account(ClusterMode::Join);
        app.join_pass = "Clust3rPw".into();
        app.handle_app_event(joined());
        assert_eq!(app.join_server_addr.as_deref(), Some("s"));
        assert_eq!(app.join_k3s_token.as_deref(), Some("k"));
        assert_eq!(app.join_ceph_fsid.as_deref(), Some("f"));
        assert_eq!(
            (app.password.as_str(), app.password2.as_str()),
            ("Clust3rPw", "Clust3rPw")
        );
        assert_eq!(app.step, Step::Disk);
        assert_eq!(app.effects, vec![Effect::DetectDisks]);
    }

    #[test]
    fn a_password_already_typed_is_kept_when_joining() {
        let mut app = at_account(ClusterMode::Join);
        app.join_pass = "Clust3rPw".into();
        app.password = "Mine1234".into();
        app.handle_app_event(joined());
        assert_eq!(
            (app.password.as_str(), app.password2.as_str()),
            ("Mine1234", "")
        );
    }

    #[test]
    fn a_generated_key_fills_the_public_key_field() {
        let mut app = ready();
        app.handle_app_event(AppEvent::SshKeyGenerated {
            private_key: "PRIV".into(),
            public_key: "PUB".into(),
        });
        assert_eq!(app.gen_privkey.as_deref(), Some("PRIV"));
        assert_eq!(app.ssh_pub, "PUB");
    }

    #[test]
    fn a_finished_install_records_where_to_manage_it() {
        let mut app = ready();
        app.handle_app_event(AppEvent::InstallComplete {
            url: "https://n".into(),
        });
        assert!(app.install_done);
        assert_eq!(app.mgmt_url.as_deref(), Some("https://n"));
    }

    #[test]
    fn a_failure_only_fails_the_install_during_the_install() {
        let mut app = ready();
        app.handle_app_event(AppEvent::Failed("nope".into()));
        assert!(!app.install_failed);
        assert_eq!(app.error.as_deref(), Some("nope"));

        app.step = Step::Install;
        app.handle_app_event(AppEvent::Failed("disko".into()));
        assert!(app.install_failed);
    }
}
