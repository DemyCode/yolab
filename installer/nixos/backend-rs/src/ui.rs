use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Stylize,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, BtnId, ClickTarget, ClusterMode, DiskInfo, Step};

// ── Palette ───────────────────────────────────────────────────────────────────
const PURPLE: Color = Color::Rgb(167, 139, 250);
const GREEN: Color = Color::Rgb(74, 222, 128);
const RED: Color = Color::Rgb(248, 113, 113);
const MUTED: Color = Color::Rgb(113, 113, 122);
const SURFACE: Color = Color::Rgb(24, 24, 27);
const BORDER: Color = Color::Rgb(39, 39, 42);
const WHITE: Color = Color::Rgb(250, 250, 250);

fn accent() -> Style { Style::default().fg(PURPLE) }
fn muted() -> Style { Style::default().fg(MUTED) }
fn white() -> Style { Style::default().fg(WHITE) }
fn success() -> Style { Style::default().fg(GREEN) }
fn danger() -> Style { Style::default().fg(RED) }
fn surface_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(format!(" {title} "), accent()))
        .bg(SURFACE)
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(0),     // body
            Constraint::Length(3),  // footer
        ])
        .split(area);

    render_header(f, rows[0], app);
    render_body(f, rows[1], app);
    render_footer(f, rows[2], app);
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let step_label = format!(
        "Step {} / {}",
        app.step.index() + 1,
        5
    );
    let boot_badge = if app.boot_mode == "uefi" {
        Span::styled("  UEFI  ", Style::default().fg(GREEN))
    } else {
        Span::styled("  BIOS  ", Style::default().fg(Color::Rgb(251, 191, 36)))
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled("Yo", white().add_modifier(Modifier::BOLD)),
        Span::styled("Lab", accent().add_modifier(Modifier::BOLD)),
        Span::styled("  Installer", muted()),
        Span::raw("  "),
        boot_badge,
        Span::styled("─".repeat(area.width.saturating_sub(38) as usize), Style::default().fg(BORDER)),
        Span::styled(format!("  {step_label}  "), muted()),
    ]))
    .block(Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(BORDER)));
    f.render_widget(title, area);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let hints = match app.step {
        Step::Mode | Step::Disk => "  ↑↓ / Click  Select     Enter  Confirm     Ctrl-C  Quit",
        Step::Account => "  Tab  Switch field     Enter  Submit     Esc  Back     Ctrl-C  Quit",
        Step::Configure => "  Tab  Next field     Enter  Continue     Esc  Back     Ctrl-C  Quit",
        Step::Install => "  r  Reboot     p  Power off     Ctrl-C  Quit",
    };
    let footer = Paragraph::new(hints)
        .style(muted())
        .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(BORDER)));
    f.render_widget(footer, area);
}

// ── Body ──────────────────────────────────────────────────────────────────────

fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(22),
            Constraint::Min(0),
        ])
        .split(area);

    render_sidebar(f, cols[0], app);

    let content = cols[1];

    if app.loading {
        render_loading(f, content, &app.loading_msg);
        return;
    }

    match app.step {
        Step::Mode => render_mode(f, content, app),
        Step::Account => render_account(f, content, app),
        Step::Disk => render_disk(f, content, app),
        Step::Configure => render_configure(f, content, app),
        Step::Install => render_install(f, content, app),
    }
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let steps = [
        (Step::Mode, "Mode"),
        (Step::Account, if app.mode == Some(ClusterMode::Join) { "Connect" } else { "Account" }),
        (Step::Disk, "Disk"),
        (Step::Configure, "Configure"),
        (Step::Install, "Install"),
    ];

    let current_idx = app.step.index();
    let mut lines: Vec<Line> = vec![Line::from(""), Line::from("")];

    for (i, (_step, label)) in steps.iter().enumerate() {
        let (icon, style) = if i < current_idx {
            ("✓", Style::default().fg(GREEN))
        } else if i == current_idx {
            ("▶", accent().add_modifier(Modifier::BOLD))
        } else {
            ("○", muted())
        };

        let step_style = if i == current_idx {
            white().add_modifier(Modifier::BOLD)
        } else if i < current_idx {
            Style::default().fg(GREEN)
        } else {
            muted()
        };

        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(icon, style),
            Span::raw("  "),
            Span::styled(*label, step_style),
        ]));
        lines.push(Line::from(""));
    }

    let sidebar = Paragraph::new(lines)
        .block(Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(BORDER)));
    f.render_widget(sidebar, area);
}

// ── Loading ───────────────────────────────────────────────────────────────────

fn render_loading(f: &mut Frame, area: Rect, msg: &str) {
    let inner = centered_rect(60, 20, area);
    let text = Paragraph::new(format!("\n  ⋯  {msg}"))
        .style(muted())
        .block(surface_block("Please wait"));
    f.render_widget(Clear, inner);
    f.render_widget(text, inner);
}

// ── Step: Mode ────────────────────────────────────────────────────────────────

fn render_mode(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),   // heading
            Constraint::Length(5),   // option 0
            Constraint::Length(1),   // gap
            Constraint::Length(5),   // option 1
            Constraint::Min(0),
            Constraint::Length(3),   // error
        ])
        .split(padded(area, 3, 1));

    render_heading(f, rows[0], "Choose your setup");

    let options = [
        ("New cluster", "First YoLab node — creates a fresh single-node setup"),
        ("Join existing cluster", "Additional machine — joins a running YoLab cluster"),
    ];

    for (i, (title, desc)) in options.iter().enumerate() {
        let rect = rows[if i == 0 { 1 } else { 3 }];
        let selected = app.mode_cursor == i;
        render_option_card(f, rect, title, desc, selected);
        app.click_areas.push((rect, ClickTarget::ModeOption(i)));
    }

    if let Some(err) = &app.error {
        f.render_widget(error_paragraph(err), rows[5]);
    }
}

// ── Step: Account / Connect ───────────────────────────────────────────────────

fn render_account(f: &mut Frame, area: Rect, app: &mut App) {
    match app.mode {
        Some(ClusterMode::New) => render_account_new(f, area, app),
        Some(ClusterMode::Join) => render_account_join(f, area, app),
        None => {}
    }
}

fn render_account_new(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // heading
            Constraint::Length(1),  // subtitle
            Constraint::Length(2),  // gap
            Constraint::Length(3),  // method tabs
            Constraint::Length(6),  // method content
            Constraint::Min(0),
            Constraint::Length(3),  // token display (if created)
            Constraint::Length(3),  // error
        ])
        .split(inner);

    render_heading(f, rows[0], "YoLab account");
    f.render_widget(
        Paragraph::new("  Connect an account so each node gets its own public URL.").style(muted()),
        rows[1],
    );

    // Method selector tabs
    let tab_row = rows[3];
    let tab_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(tab_row);

    let methods = ["Create new account", "I have an account"];
    for (i, label) in methods.iter().enumerate() {
        let selected = app.acct_cursor == i as u8;
        let style = if selected { accent().add_modifier(Modifier::BOLD) } else { muted() };
        let border_style = if selected { Style::default().fg(PURPLE) } else { Style::default().fg(BORDER) };
        let w = Paragraph::new(format!("  {label}"))
            .style(style)
            .block(Block::default().borders(Borders::BOTTOM).border_style(border_style));
        f.render_widget(w, tab_cols[i]);
        app.click_areas.push((tab_cols[i], ClickTarget::AcctMethod(i as u8)));
    }

    if app.account_token.is_some() {
        // Already have an account — show success
        f.render_widget(
            Paragraph::new("\n  ✓ Account connected")
                .style(success()),
            rows[4],
        );
        let btn_rect = Rect { x: rows[4].x + 2, y: rows[4].y + 4, width: 18, height: 3, ..rows[4] };
        render_button(f, btn_rect, "Continue →", true, app, ClickTarget::Btn(BtnId::Continue));
    } else if app.acct_cursor == 0 {
        // Create
        f.render_widget(
            Paragraph::new("\n  No email required — a token is generated instantly.\n  Keep it safe: you'll need it to add more nodes later.")
                .style(muted())
                .wrap(Wrap { trim: true }),
            rows[4],
        );
        let btn_rect = Rect { x: rows[4].x + 2, y: rows[4].y + 3, width: 22, height: 3, ..rows[4] };
        render_button(f, btn_rect, "Create account →", true, app, ClickTarget::Btn(BtnId::CreateAcct));
    } else {
        // Existing token
        let input_area = Rect {
            x: rows[4].x + 2,
            y: rows[4].y + 1,
            width: inner.width.saturating_sub(4),
            height: 3,
        };
        render_input(f, input_area, "Token", &app.acct_input, false, true);
        let btn_rect = Rect { x: input_area.x, y: input_area.y + 3, width: 18, height: 3, ..input_area };
        render_button(f, btn_rect, "Verify token →", true, app, ClickTarget::Btn(BtnId::VerifyToken));
    }

    // Show created token if we just created one
    if let Some(token) = &app.created_token.clone() {
        let short = if token.len() > 30 { &token[..30] } else { token };
        let tok_para = Paragraph::new(Line::from(vec![
            Span::styled("  Token: ", muted()),
            Span::styled(format!("{short}…"), accent().add_modifier(Modifier::BOLD)),
            Span::styled("  (save this!)", danger()),
        ]));
        f.render_widget(tok_para, rows[6]);
        let btn_rect = Rect { x: rows[6].x + 2, y: rows[6].y + 1, width: 18, height: 3, ..rows[6] };
        render_button(f, btn_rect, "Continue →", true, app, ClickTarget::Btn(BtnId::Continue));
    }

    if let Some(err) = &app.error.clone() {
        f.render_widget(error_paragraph(err), rows[7]);
    }
}

fn render_account_join(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // heading
            Constraint::Length(2),  // gap
            Constraint::Length(3),  // url field
            Constraint::Length(2),  // gap
            Constraint::Length(3),  // pass field
            Constraint::Length(2),  // gap
            Constraint::Length(3),  // button
            Constraint::Min(0),
            Constraint::Length(3),  // error
        ])
        .split(inner);

    render_heading(f, rows[0], "Connect to existing node");

    let url_rect = rows[2];
    render_input(f, url_rect, "Node URL  (e.g. https://node1.user.demycode.ovh)", &app.join_url, false, app.join_field == 0);
    app.click_areas.push((url_rect, ClickTarget::CfgField(10))); // field 10 = join_url

    let pass_rect = rows[4];
    render_input(f, pass_rect, "Password", &app.join_pass, true, app.join_field == 1);
    app.click_areas.push((pass_rect, ClickTarget::CfgField(11))); // field 11 = join_pass

    render_button(f, rows[6], "Connect →", true, app, ClickTarget::Btn(BtnId::Connect));

    if let Some(err) = &app.error.clone() {
        f.render_widget(error_paragraph(err), rows[8]);
    }
}

// ── Step: Disk ────────────────────────────────────────────────────────────────

fn render_disk(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    if app.disks.is_empty() {
        f.render_widget(
            Paragraph::new("\n  No disks detected.").style(muted()),
            inner,
        );
        return;
    }

    let heading_row = Rect { x: inner.x, y: inner.y, width: inner.width, height: 3 };
    let warn_row = Rect { x: inner.x, y: inner.y + 3, width: inner.width, height: 2 };
    let list_start = inner.y + 5;
    let err_row = Rect { x: inner.x, y: inner.bottom().saturating_sub(3), width: inner.width, height: 3 };

    render_heading(f, heading_row, "Select installation disk");
    f.render_widget(
        Paragraph::new("  ⚠  The selected disk will be completely erased.").style(danger()),
        warn_row,
    );

    for (i, disk) in app.disks.iter().enumerate() {
        let row = Rect {
            x: inner.x,
            y: list_start + i as u16 * 4,
            width: inner.width,
            height: 3,
        };
        if row.bottom() > err_row.y { break; }
        render_disk_row(f, row, disk, i, app.disk_cursor == i);
        app.click_areas.push((row, ClickTarget::DiskOption(i)));
    }

    if let Some(err) = &app.error.clone() {
        f.render_widget(error_paragraph(err), err_row);
    }
}

fn render_disk_row(f: &mut Frame, area: Rect, disk: &DiskInfo, _idx: usize, selected: bool) {
    let border_style = if selected {
        Style::default().fg(PURPLE)
    } else if disk.mounted {
        Style::default().fg(BORDER)
    } else {
        Style::default().fg(BORDER)
    };

    let bg = if selected { SURFACE } else { Color::Reset };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(if selected { "▶ " } else { "  " }, accent()),
        Span::styled(&disk.name, if selected { white().add_modifier(Modifier::BOLD) } else { white() }),
        Span::raw("   "),
        Span::styled(&disk.size, muted()),
        Span::raw("  "),
        Span::styled(&disk.tran, muted()),
    ];
    if disk.recommended {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("★ Recommended", success()));
    }
    if disk.is_usb {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("USB", Style::default().fg(Color::Rgb(129, 140, 248))));
    }
    if disk.mounted {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("in use", danger()));
    }

    let para = Paragraph::new(Line::from(spans))
        .block(Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .bg(bg));
    f.render_widget(para, area);
}

// ── Step: Configure ───────────────────────────────────────────────────────────

fn render_configure(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),   // heading
            Constraint::Length(1),
            Constraint::Length(3),   // timezone
            Constraint::Length(1),
            Constraint::Length(3),   // password
            Constraint::Length(1),
            Constraint::Length(3),   // confirm
            Constraint::Length(1),
            Constraint::Length(3),   // ssh key
            Constraint::Length(1),
            Constraint::Length(3),   // gen ssh btn
            Constraint::Min(0),
            Constraint::Length(3),   // error
        ])
        .split(inner);

    render_heading(f, rows[0], "System configuration");

    let fields: &[(&str, &str, bool, u8)] = &[
        ("Timezone", &app.timezone.clone(), false, 0),
        ("Password", &app.password.clone(), true, 1),
        ("Confirm password", &app.password2.clone(), true, 2),
        ("SSH public key  (optional)", &app.ssh_pub.clone(), false, 3),
    ];

    let field_rows = [rows[2], rows[4], rows[6], rows[8]];
    for ((label, value, secret, field_id), row) in fields.iter().zip(field_rows.iter()) {
        let focused = app.cfg_field == *field_id;
        render_input(f, *row, label, value, *secret, focused);
        app.click_areas.push((*row, ClickTarget::CfgField(*field_id)));
    }

    // Generate SSH key button
    render_button(f, rows[10], "Generate SSH key", true, app, ClickTarget::Btn(BtnId::GenSshKey));

    if let Some(key) = &app.gen_privkey.clone() {
        let short_key = if key.len() > 50 { format!("{}…", &key[..50]) } else { key.clone() };
        let note = Paragraph::new(Line::from(vec![
            Span::styled("  Private key (save it!): ", danger()),
            Span::styled(short_key, muted()),
        ]));
        f.render_widget(note, rows[11]);
    }

    if let Some(err) = &app.error.clone() {
        f.render_widget(error_paragraph(err), rows[12]);
    }
}

// ── Step: Install ─────────────────────────────────────────────────────────────

fn render_install(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 2, 1);

    if app.install_done {
        render_install_done(f, inner, app);
        return;
    }
    if app.install_failed {
        render_install_failed(f, inner, app);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    render_heading(f, rows[0], "Installing YoLab…");

    // Show last N log lines that fit
    let log_height = rows[1].height.saturating_sub(2) as usize;
    let total = app.log_lines.len();
    let start = total.saturating_sub(log_height);
    let visible: Vec<Line> = app.log_lines[start..]
        .iter()
        .map(|l| {
            let style = if l.starts_with("ERROR") { danger() }
                else if l.starts_with("✓") || l.contains("complete") { success() }
                else { muted() };
            Line::from(Span::styled(format!("  {l}"), style))
        })
        .collect();

    let log = Paragraph::new(visible)
        .block(surface_block("Progress"))
        .wrap(Wrap { trim: false });
    f.render_widget(log, rows[1]);
}

fn render_install_done(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(8),
            Constraint::Length(2),
            Constraint::Length(3),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new("  ✓  Installation complete!")
            .style(success().add_modifier(Modifier::BOLD)),
        rows[0],
    );

    if let Some(url) = &app.mgmt_url {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  Management URL: ", muted()),
                Span::styled(url.clone(), accent().add_modifier(Modifier::BOLD)),
            ])),
            rows[1],
        );
    }

    let steps = Paragraph::new(Text::from(vec![
        Line::from(Span::styled("  Next steps:", white())),
        Line::from(""),
        Line::from(Span::styled("  1.  Remove the installation USB drive", muted())),
        Line::from(Span::styled("  2.  Click 'Reboot' or turn off the machine", muted())),
        Line::from(Span::styled("  3.  Your node will be ready in about a minute", muted())),
    ]));
    f.render_widget(steps, rows[2]);

    let btn_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Length(2), Constraint::Length(18), Constraint::Min(0)])
        .split(rows[4]);

    render_button(f, btn_cols[0], "r  Reboot", true, app, ClickTarget::Btn(BtnId::Reboot));
    render_button(f, btn_cols[2], "p  Power off", false, app, ClickTarget::Btn(BtnId::Poweroff));
}

fn render_install_failed(f: &mut Frame, area: Rect, app: &App) {
    let text = vec![
        Line::from(Span::styled("  ✗  Installation failed", danger().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled(
            app.error.as_deref().unwrap_or("Unknown error"),
            muted(),
        )),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

// ── Reusable widgets ──────────────────────────────────────────────────────────

fn render_heading(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(format!("  {text}"))
            .style(white().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_option_card(f: &mut Frame, area: Rect, title: &str, desc: &str, selected: bool) {
    let border = if selected { Style::default().fg(PURPLE) } else { Style::default().fg(BORDER) };
    let icon = if selected { "▶  " } else { "   " };
    let title_style = if selected {
        white().add_modifier(Modifier::BOLD)
    } else {
        white()
    };

    let text = Text::from(vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, accent()),
            Span::styled(title, title_style),
        ]),
        Line::from(vec![
            Span::raw("     "),
            Span::styled(desc, muted()),
        ]),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .bg(if selected { SURFACE } else { Color::Reset });

    f.render_widget(Paragraph::new(text).block(block), area);
}

fn render_input(f: &mut Frame, area: Rect, label: &str, value: &str, secret: bool, focused: bool) {
    let display = if secret {
        "•".repeat(value.len())
    } else {
        value.to_string()
    };
    let cursor = if focused { "▌" } else { "" };
    let border_style = if focused {
        Style::default().fg(PURPLE)
    } else {
        Style::default().fg(BORDER)
    };
    let label_style = if focused { accent() } else { muted() };

    let para = Paragraph::new(format!(" {display}{cursor}"))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(Span::styled(format!(" {label} "), label_style))
                .bg(SURFACE),
        );
    f.render_widget(para, area);
}

fn render_button(
    f: &mut Frame,
    area: Rect,
    label: &str,
    primary: bool,
    app: &mut App,
    target: ClickTarget,
) {
    let (fg, bg) = if primary {
        (Color::Black, PURPLE)
    } else {
        (MUTED, Color::Reset)
    };
    let border = if primary { Style::default().fg(PURPLE) } else { Style::default().fg(BORDER) };

    let btn = Paragraph::new(format!(" {label} "))
        .style(Style::default().fg(fg).bg(bg))
        .block(Block::default().borders(Borders::ALL).border_style(border));
    f.render_widget(btn, area);
    app.click_areas.push((area, target));
}

fn error_paragraph(msg: &str) -> Paragraph<'static> {
    Paragraph::new(format!("  ⚠  {msg}"))
        .style(danger())
        .wrap(Wrap { trim: true })
}

// ── Layout helpers ────────────────────────────────────────────────────────────

fn padded(area: Rect, h: u16, v: u16) -> Rect {
    Rect {
        x: area.x + h,
        y: area.y + v,
        width: area.width.saturating_sub(h * 2),
        height: area.height.saturating_sub(v * 2),
    }
}

fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let w = area.width * width_pct / 100;
    let h = area.height * height_pct / 100;
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
