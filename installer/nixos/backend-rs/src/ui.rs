use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Stylize,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, BtnId, ClickTarget, ClusterMode, DiskInfo, Step};

const PURPLE: Color = Color::Rgb(167, 139, 250);
const GREEN: Color = Color::Rgb(74, 222, 128);
const RED: Color = Color::Rgb(248, 113, 113);
const MUTED: Color = Color::Rgb(113, 113, 122);
const SURFACE: Color = Color::Rgb(24, 24, 27);
const BORDER: Color = Color::Rgb(39, 39, 42);
const WHITE: Color = Color::Rgb(250, 250, 250);

fn accent() -> Style {
    Style::default().fg(PURPLE)
}
fn muted() -> Style {
    Style::default().fg(MUTED)
}
fn white() -> Style {
    Style::default().fg(WHITE)
}
fn success() -> Style {
    Style::default().fg(GREEN)
}
fn danger() -> Style {
    Style::default().fg(RED)
}
fn surface_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(format!(" {title} "), accent()))
        .bg(SURFACE)
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(f, rows[0], app);
    render_body(f, rows[1], app);
    render_footer(f, rows[2], app);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let step_label = format!("Step {} / {}", app.step.index() + 1, 5);
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
        Span::styled(
            "─".repeat(area.width.saturating_sub(38) as usize),
            Style::default().fg(BORDER),
        ),
        Span::styled(format!("  {step_label}  "), muted()),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(BORDER)),
    );
    f.render_widget(title, area);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let hints = match app.step {
        Step::Mode | Step::Disk => "  ↑↓ / Click  Select     Enter  Confirm     Ctrl-C  Quit",
        Step::Account => "  Tab  Switch field     Enter  Submit     Esc  Back     Ctrl-C  Quit",
        Step::Configure => "  Tab  Next field     Enter  Continue     Esc  Back     Ctrl-C  Quit",
        Step::Install => "  r  Reboot     p  Power off     Ctrl-C  Quit",
    };
    let footer = Paragraph::new(hints).style(muted()).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
    );
    f.render_widget(footer, area);
}

fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(0)])
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

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let steps = [
        (Step::Mode, "Mode"),
        (
            Step::Account,
            if app.mode == Some(ClusterMode::Join) {
                "Connect"
            } else {
                "Account"
            },
        ),
        (Step::Disk, "Disk"),
        (Step::Configure, "Configure"),
        (Step::Install, "Install"),
    ];

    let current_idx = app.step.index();
    let mut lines: Vec<Line> = vec![Line::from(""), Line::from("")];

    for (i, (_step, label)) in steps.iter().enumerate() {
        let (icon, style, step_style) = if i < current_idx {
            ("✓", Style::default().fg(GREEN), Style::default().fg(GREEN))
        } else if i == current_idx {
            (
                "▶",
                accent().add_modifier(Modifier::BOLD),
                white().add_modifier(Modifier::BOLD),
            )
        } else {
            ("○", muted(), muted())
        };

        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(icon, style),
            Span::raw("  "),
            Span::styled(*label, step_style),
        ]));
        lines.push(Line::from(""));
    }

    let sidebar = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(BORDER)),
    );
    f.render_widget(sidebar, area);
}

fn render_loading(f: &mut Frame, area: Rect, msg: &str) {
    let inner = centered_rect(60, 20, area);
    let text = Paragraph::new(format!("\n  ⋯  {msg}"))
        .style(muted())
        .block(surface_block("Please wait"));
    f.render_widget(Clear, inner);
    f.render_widget(text, inner);
}

fn render_mode(f: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(padded(area, 3, 1));

    render_heading(f, rows[0], "Choose your setup");

    let options = [
        (
            "New cluster",
            "First YoLab node — creates a fresh single-node setup",
        ),
        (
            "Join existing cluster",
            "Additional machine — joins a running YoLab cluster",
        ),
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
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(inner);

    render_heading(f, rows[0], "YoLab account");
    f.render_widget(
        Paragraph::new("  Connect an account so each node gets its own public URL.").style(muted()),
        rows[1],
    );

    let tab_row = rows[3];
    let tab_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(tab_row);

    let methods = ["Create new account", "I have an account"];
    for (i, label) in methods.iter().enumerate() {
        let selected = app.acct_cursor == i as u8;
        let style = if selected {
            accent().add_modifier(Modifier::BOLD)
        } else {
            muted()
        };
        let border_style = if selected {
            Style::default().fg(PURPLE)
        } else {
            Style::default().fg(BORDER)
        };
        let w = Paragraph::new(format!("  {label}")).style(style).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(border_style),
        );
        f.render_widget(w, tab_cols[i]);
        app.click_areas
            .push((tab_cols[i], ClickTarget::AcctMethod(i as u8)));
    }

    if app.account_token.is_some() {
        f.render_widget(
            Paragraph::new("\n  ✓ Account connected").style(success()),
            rows[4],
        );
        let btn_rect = Rect {
            x: rows[4].x + 2,
            y: rows[4].y + 4,
            width: 18,
            height: 3,
        };
        render_button(
            f,
            btn_rect,
            "Continue →",
            true,
            app,
            ClickTarget::Btn(BtnId::Continue),
        );
    } else if app.acct_cursor == 0 {
        let input_area = Rect {
            x: rows[4].x + 2,
            y: rows[4].y,
            width: inner.width.saturating_sub(4),
            height: 3,
        };
        render_input(
            f,
            input_area,
            "Referral code (optional)",
            &app.acct_referral,
            false,
            true,
        );
        f.render_widget(
            Paragraph::new("  No email needed. Keep the token safe: you need it to add nodes.")
                .style(muted()),
            Rect {
                x: rows[4].x,
                y: rows[4].y + 3,
                width: rows[4].width,
                height: 1,
            },
        );
        let btn_rect = Rect {
            x: rows[4].x + 2,
            y: rows[4].y + 4,
            width: 22,
            height: 3,
        };
        render_button(
            f,
            btn_rect,
            "Create account →",
            true,
            app,
            ClickTarget::Btn(BtnId::CreateAcct),
        );
    } else {
        let input_area = Rect {
            x: rows[4].x + 2,
            y: rows[4].y + 1,
            width: inner.width.saturating_sub(4),
            height: 3,
        };
        render_input(f, input_area, "Token", &app.acct_input, false, true);
        let btn_rect = Rect {
            x: input_area.x,
            y: input_area.y + 3,
            width: 18,
            height: 3,
        };
        render_button(
            f,
            btn_rect,
            "Verify token →",
            true,
            app,
            ClickTarget::Btn(BtnId::VerifyToken),
        );
    }

    if let Some(token) = &app.created_token.clone() {
        let short = &token[..token.len().min(30)];
        let tok_para = Paragraph::new(Line::from(vec![
            Span::styled("  Token: ", muted()),
            Span::styled(format!("{short}…"), accent().add_modifier(Modifier::BOLD)),
            Span::styled("  (save this!)", danger()),
        ]));
        f.render_widget(tok_para, rows[6]);
        let btn_rect = Rect {
            x: rows[6].x + 2,
            y: rows[6].y + 1,
            width: 18,
            height: 3,
        };
        render_button(
            f,
            btn_rect,
            "Continue →",
            true,
            app,
            ClickTarget::Btn(BtnId::Continue),
        );
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
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(inner);

    render_heading(f, rows[0], "Connect to existing node");

    let url_rect = rows[2];
    render_input(
        f,
        url_rect,
        "Node URL  (e.g. https://node1.user.yolab.io)",
        &app.join_url,
        false,
        app.join_field == 0,
    );
    app.click_areas.push((url_rect, ClickTarget::JoinField(0)));

    let pass_rect = rows[4];
    render_input(
        f,
        pass_rect,
        "Password",
        &app.join_pass,
        true,
        app.join_field == 1,
    );
    app.click_areas.push((pass_rect, ClickTarget::JoinField(1)));

    render_button(
        f,
        rows[6],
        "Connect →",
        true,
        app,
        ClickTarget::Btn(BtnId::Connect),
    );

    if let Some(err) = &app.error.clone() {
        f.render_widget(error_paragraph(err), rows[8]);
    }
}

fn render_disk(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    if app.disks.is_empty() {
        f.render_widget(
            Paragraph::new("\n  No disks detected.").style(muted()),
            inner,
        );
        return;
    }

    let heading_row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 3,
    };
    let warn_row = Rect {
        x: inner.x,
        y: inner.y + 3,
        width: inner.width,
        height: 2,
    };
    let list_start = inner.y + 5;
    let err_row = Rect {
        x: inner.x,
        y: inner.bottom().saturating_sub(3),
        width: inner.width,
        height: 3,
    };

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
        if row.bottom() > err_row.y {
            break;
        }
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
    } else {
        Style::default().fg(BORDER)
    };

    let bg = if selected { SURFACE } else { Color::Reset };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(if selected { "▶ " } else { "  " }, accent()),
        Span::styled(
            &disk.name,
            if selected {
                white().add_modifier(Modifier::BOLD)
            } else {
                white()
            },
        ),
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
        spans.push(Span::styled(
            "USB",
            Style::default().fg(Color::Rgb(129, 140, 248)),
        ));
    }
    if disk.mounted {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("in use", danger()));
    }

    let para = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .bg(bg),
    );
    f.render_widget(para, area);
}

fn render_configure(f: &mut Frame, area: Rect, app: &mut App) {
    let inner = padded(area, 3, 1);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
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
        app.click_areas
            .push((*row, ClickTarget::CfgField(*field_id)));
    }

    render_button(
        f,
        rows[10],
        "Generate SSH key",
        true,
        app,
        ClickTarget::Btn(BtnId::GenSshKey),
    );

    if let Some(key) = &app.gen_privkey.clone() {
        let short_key = if key.len() > 50 {
            format!("{}…", &key[..50])
        } else {
            key.clone()
        };
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
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(inner);

    render_heading(f, rows[0], "Installing YoLab…");

    let log_height = rows[1].height.saturating_sub(2) as usize;
    let total = app.log_lines.len();
    let start = total.saturating_sub(log_height);
    let visible: Vec<Line> = app.log_lines[start..]
        .iter()
        .map(|l| {
            let style = if l.starts_with("ERROR") {
                danger()
            } else if l.starts_with("✓") || l.contains("complete") {
                success()
            } else {
                muted()
            };
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
        Paragraph::new("  ✓  Installation complete!").style(success().add_modifier(Modifier::BOLD)),
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
        Line::from(Span::styled(
            "  1.  Remove the installation USB drive",
            muted(),
        )),
        Line::from(Span::styled(
            "  2.  Click 'Reboot' or turn off the machine",
            muted(),
        )),
        Line::from(Span::styled(
            "  3.  Your node will be ready in about a minute",
            muted(),
        )),
    ]));
    f.render_widget(steps, rows[2]);

    let btn_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(18),
            Constraint::Length(2),
            Constraint::Length(18),
            Constraint::Min(0),
        ])
        .split(rows[4]);

    render_button(
        f,
        btn_cols[0],
        "r  Reboot",
        true,
        app,
        ClickTarget::Btn(BtnId::Reboot),
    );
    render_button(
        f,
        btn_cols[2],
        "p  Power off",
        false,
        app,
        ClickTarget::Btn(BtnId::Poweroff),
    );
}

fn render_install_failed(f: &mut Frame, area: Rect, app: &App) {
    let text = vec![
        Line::from(Span::styled(
            "  ✗  Installation failed",
            danger().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            app.error.as_deref().unwrap_or("Unknown error"),
            muted(),
        )),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

fn render_heading(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(format!("  {text}")).style(white().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_option_card(f: &mut Frame, area: Rect, title: &str, desc: &str, selected: bool) {
    let border = if selected {
        Style::default().fg(PURPLE)
    } else {
        Style::default().fg(BORDER)
    };
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
        Line::from(vec![Span::raw("     "), Span::styled(desc, muted())]),
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

    let para = Paragraph::new(format!(" {display}{cursor}")).block(
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
    let border = if primary {
        Style::default().fg(PURPLE)
    } else {
        Style::default().fg(BORDER)
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    const W: u16 = 120;
    const H: u16 = 50;

    fn ready(step: Step) -> App {
        let mut app = App::new("uefi");
        app.loading = false;
        app.network_ready = true;
        app.mode = Some(ClusterMode::New);
        app.step = step;
        app
    }

    fn draw_sized(app: &mut App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        app.click_areas.clear();
        t.draw(|f| render(f, app)).unwrap();
        t.backend().buffer().clone()
    }

    fn draw(app: &mut App) -> Buffer {
        draw_sized(app, W, H)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    fn find(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
        (0..buf.area.height).find_map(|y| {
            let r = row(buf, y);
            r.find(text).map(|i| (r[..i].chars().count() as u16, y))
        })
    }

    fn at(buf: &Buffer, text: &str) -> (u16, u16) {
        find(buf, text).expect("the text is on screen")
    }

    fn fg(buf: &Buffer, text: &str) -> Color {
        buf.cell(at(buf, text)).unwrap().fg
    }

    fn bg(buf: &Buffer, text: &str) -> Color {
        buf.cell(at(buf, text)).unwrap().bg
    }

    fn shows(buf: &Buffer, text: &str) -> bool {
        find(buf, text).is_some()
    }

    fn target_at(app: &App, (x, y): (u16, u16)) -> Option<ClickTarget> {
        app.click_areas
            .iter()
            .find(|(r, _)| r.contains(ratatui::layout::Position { x, y }))
            .map(|(_, t)| t.clone())
    }

    #[test]
    fn the_header_counts_steps_and_names_the_boot_mode() {
        let mut app = ready(Step::Disk);
        let buf = draw(&mut app);
        assert!(shows(&buf, "Step 3 / 5"));
        assert_eq!(fg(&buf, "UEFI"), GREEN);

        let mut app = ready(Step::Install);
        app.boot_mode = "bios".into();
        let buf = draw(&mut app);
        assert!(shows(&buf, "Step 5 / 5"));
        assert_eq!(fg(&buf, "BIOS"), Color::Rgb(251, 191, 36));
        assert!(!shows(&buf, "UEFI"));
    }

    #[test]
    fn the_header_rule_fills_the_width_left_over_by_the_title() {
        let mut app = ready(Step::Mode);
        let buf = draw(&mut app);
        assert_eq!(row(&buf, 0).matches('─').count(), (W - 38) as usize);
        assert_eq!(fg(&buf, "Yo"), WHITE);
        assert_eq!(fg(&buf, "Lab"), PURPLE);
    }

    #[test]
    fn the_footer_lists_the_keys_each_step_takes() {
        let cases = [
            (Step::Mode, "↑↓ / Click  Select"),
            (Step::Disk, "↑↓ / Click  Select"),
            (Step::Account, "Tab  Switch field"),
            (Step::Configure, "Tab  Next field"),
            (Step::Install, "r  Reboot     p  Power off"),
        ];
        for (step, hint) in cases {
            let mut app = ready(step.clone());
            let buf = draw(&mut app);
            let (_, y) = at(&buf, hint);
            assert!(y >= H - 3, "{step:?}: footer hint drawn at row {y}");
            assert_eq!(fg(&buf, hint), MUTED);
        }
    }

    #[test]
    fn the_sidebar_ticks_finished_steps_and_marks_the_current_one() {
        let mut app = ready(Step::Disk);
        let buf = draw(&mut app);
        assert!(shows(&buf, "✓  Mode"));
        assert!(shows(&buf, "✓  Account"));
        assert!(shows(&buf, "▶  Disk"));
        assert!(shows(&buf, "○  Configure"));
        assert!(shows(&buf, "○  Install"));
        assert_eq!(fg(&buf, "Mode"), GREEN);
        assert_eq!(fg(&buf, "✓  Mode"), GREEN);
        assert_eq!(fg(&buf, "Disk"), WHITE);
        assert_eq!(fg(&buf, "▶  Disk"), PURPLE);
        assert_eq!(fg(&buf, "Configure"), MUTED);
        assert_eq!(fg(&buf, "○  Install"), MUTED);
        let (x, _) = at(&buf, "Configure");
        assert!(x < 22);
    }

    #[test]
    fn the_sidebar_says_connect_when_joining() {
        let mut app = ready(Step::Mode);
        app.mode = Some(ClusterMode::Join);
        let buf = draw(&mut app);
        assert!(shows(&buf, "Connect"));
        assert!(!shows(&buf, "Account"));
    }

    #[test]
    fn while_loading_only_the_wait_message_shows() {
        let mut app = App::new("uefi");
        let buf = draw(&mut app);
        assert!(shows(&buf, "Please wait"));
        assert!(shows(&buf, "⋯  Waiting for network…"));
        assert!(!shows(&buf, "Choose your setup"));
        assert!(app.click_areas.is_empty());
        let (x, y) = at(&buf, "Please wait");
        assert!(
            x > 22 + 10 && y > 3 + 10,
            "the wait box sits in the middle: {x},{y}"
        );
    }

    #[test]
    fn the_wait_box_is_centred_in_the_content_area() {
        let area = Rect {
            x: 20,
            y: 10,
            width: 100,
            height: 50,
        };
        assert_eq!(
            centered_rect(60, 20, area),
            Rect {
                x: 40,
                y: 30,
                width: 60,
                height: 10
            }
        );
    }

    #[test]
    fn padding_shrinks_from_both_sides() {
        let area = Rect {
            x: 5,
            y: 2,
            width: 20,
            height: 10,
        };
        assert_eq!(
            padded(area, 3, 1),
            Rect {
                x: 8,
                y: 3,
                width: 14,
                height: 8
            }
        );
        assert_eq!(
            padded(
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1
                },
                3,
                1
            )
            .width,
            0
        );
    }

    #[test]
    fn the_mode_screen_offers_both_setups_and_each_is_clickable() {
        let mut app = ready(Step::Mode);
        app.mode_cursor = 1;
        let buf = draw(&mut app);
        assert!(shows(&buf, "Choose your setup"));
        assert!(shows(&buf, "   New cluster"));
        assert!(shows(&buf, "▶  Join existing cluster"));
        assert_eq!(fg(&buf, "Join existing cluster"), WHITE);
        assert_eq!(bg(&buf, "Join existing cluster"), SURFACE);
        assert_eq!(bg(&buf, "New cluster"), Color::Reset);
        assert_eq!(
            target_at(&app, at(&buf, "New cluster")),
            Some(ClickTarget::ModeOption(0))
        );
        assert_eq!(
            target_at(&app, at(&buf, "Join existing")),
            Some(ClickTarget::ModeOption(1))
        );
        assert!(app.click_areas.iter().all(|(r, _)| r.x >= 22));
    }

    #[test]
    fn the_mode_screen_shows_an_error() {
        let mut app = ready(Step::Mode);
        app.error = Some("Network not ready".into());
        let buf = draw(&mut app);
        assert_eq!(fg(&buf, "⚠  Network not ready"), RED);
    }

    #[test]
    fn creating_an_account_asks_only_for_an_optional_referral_code() {
        let mut app = ready(Step::Account);
        app.acct_referral = "AB3".into();
        let buf = draw(&mut app);
        assert!(shows(&buf, "YoLab account"));
        assert!(shows(&buf, "Referral code (optional)"));
        assert!(shows(&buf, " AB3▌"));
        assert!(shows(&buf, "No email needed."));
        assert_eq!(fg(&buf, "Create new account"), PURPLE);
        assert_eq!(fg(&buf, "I have an account"), MUTED);
        assert_eq!(
            target_at(&app, at(&buf, "Create account →")),
            Some(ClickTarget::Btn(BtnId::CreateAcct))
        );
        assert_eq!(
            target_at(&app, at(&buf, "Create new account")),
            Some(ClickTarget::AcctMethod(0))
        );
        assert_eq!(
            target_at(&app, at(&buf, "I have an account")),
            Some(ClickTarget::AcctMethod(1))
        );
        assert!(!shows(&buf, "Verify token"));
    }

    #[test]
    fn an_existing_account_is_verified_by_its_token() {
        let mut app = ready(Step::Account);
        app.acct_cursor = 1;
        app.acct_input = "tok-1".into();
        let buf = draw(&mut app);
        assert!(shows(&buf, " Token "));
        assert!(shows(&buf, " tok-1▌"));
        assert_eq!(fg(&buf, "I have an account"), PURPLE);
        assert_eq!(
            target_at(&app, at(&buf, "Verify token →")),
            Some(ClickTarget::Btn(BtnId::VerifyToken))
        );
        assert!(!shows(&buf, "Referral code"));
    }

    #[test]
    fn a_connected_account_offers_to_continue() {
        let mut app = ready(Step::Account);
        app.account_token = Some("tok".into());
        app.error = Some("stale".into());
        let buf = draw(&mut app);
        assert_eq!(fg(&buf, "✓ Account connected"), GREEN);
        assert_eq!(
            target_at(&app, at(&buf, "Continue →")),
            Some(ClickTarget::Btn(BtnId::Continue))
        );
        assert!(!shows(&buf, "Create account →"));
        assert!(shows(&buf, "⚠  stale"));
    }

    #[test]
    fn a_new_token_is_shown_shortened_with_a_warning_to_save_it() {
        let mut app = ready(Step::Account);
        let long = "t".repeat(29) + "XYZ";
        app.account_token = Some(long.clone());
        app.created_token = Some(long);
        let buf = draw(&mut app);
        assert!(shows(&buf, &format!("{}X…", "t".repeat(29))));
        assert!(!shows(&buf, "XY"));
        assert_eq!(fg(&buf, "(save this!)"), RED);
        assert_eq!(fg(&buf, "tttt"), PURPLE);

        let mut app = ready(Step::Account);
        app.account_token = Some("short-token".into());
        app.created_token = Some("short-token".into());
        let buf = draw(&mut app);
        assert!(shows(&buf, "Token: short-token…"));
        let continues = app
            .click_areas
            .iter()
            .filter(|(_, t)| *t == ClickTarget::Btn(BtnId::Continue))
            .count();
        assert_eq!(continues, 2);
    }

    #[test]
    fn the_account_screen_is_empty_before_a_mode_is_chosen() {
        let mut app = ready(Step::Account);
        app.mode = None;
        let buf = draw(&mut app);
        assert!(!shows(&buf, "YoLab account"));
        assert!(!shows(&buf, "Connect to existing node"));
    }

    #[test]
    fn joining_asks_for_the_node_url_and_a_hidden_password() {
        let mut app = ready(Step::Account);
        app.mode = Some(ClusterMode::Join);
        app.join_url = "https://box.example".into();
        app.join_pass = "abc".into();
        let buf = draw(&mut app);
        assert!(shows(&buf, "Connect to existing node"));
        assert!(shows(&buf, " https://box.example▌"));
        assert!(shows(&buf, " •••"));
        assert!(!shows(&buf, "abc"));
        assert!(!shows(&buf, "•••▌"));
        assert_eq!(
            target_at(&app, at(&buf, "https://box.example")),
            Some(ClickTarget::JoinField(0))
        );
        assert_eq!(
            target_at(&app, at(&buf, "•••")),
            Some(ClickTarget::JoinField(1))
        );
        assert_eq!(
            target_at(&app, at(&buf, "Connect →")),
            Some(ClickTarget::Btn(BtnId::Connect))
        );

        app.join_field = 1;
        app.error = Some("authentication failed".into());
        let buf = draw(&mut app);
        assert!(shows(&buf, " •••▌"));
        assert!(!shows(&buf, "example▌"));
        assert!(shows(&buf, "⚠  authentication failed"));
    }

    fn disk(name: &str, recommended: bool, is_usb: bool, mounted: bool) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            size: "500 GB".into(),
            tran: if is_usb { "usb".into() } else { "nvme".into() },
            is_usb,
            mounted,
            recommended,
        }
    }

    #[test]
    fn with_no_disks_the_disk_screen_says_so() {
        let mut app = ready(Step::Disk);
        let buf = draw(&mut app);
        assert!(shows(&buf, "No disks detected."));
        assert!(!shows(&buf, "Select installation disk"));
    }

    #[test]
    fn each_disk_shows_its_size_and_what_makes_it_a_good_or_bad_choice() {
        let mut app = ready(Step::Disk);
        app.disks = vec![
            disk("/dev/nvme0n1", true, false, false),
            disk("/dev/sdb", false, true, true),
        ];
        app.error = Some("That disk is in use".into());
        let buf = draw(&mut app);
        assert_eq!(
            fg(&buf, "⚠  The selected disk will be completely erased."),
            RED
        );
        assert!(shows(&buf, "▶ /dev/nvme0n1   500 GB  nvme   ★ Recommended"));
        assert!(shows(&buf, "  /dev/sdb   500 GB  usb   USB   in use"));
        assert_eq!(fg(&buf, "★ Recommended"), GREEN);
        assert_eq!(fg(&buf, "USB"), Color::Rgb(129, 140, 248));
        assert_eq!(fg(&buf, "in use"), RED);
        assert_eq!(bg(&buf, "/dev/nvme0n1"), SURFACE);
        assert_eq!(bg(&buf, "/dev/sdb"), Color::Reset);
        assert_eq!(
            target_at(&app, at(&buf, "/dev/sdb")),
            Some(ClickTarget::DiskOption(1))
        );
        assert!(shows(&buf, "⚠  That disk is in use"));
    }

    #[test]
    fn disks_that_do_not_fit_above_the_error_line_are_left_out() {
        let mut app = ready(Step::Disk);
        app.disks = (0..5)
            .map(|i| disk(&format!("/dev/sd{i}"), false, false, false))
            .collect();
        let buf = draw_sized(&mut app, W, 30);
        assert!(shows(&buf, "/dev/sd2"));
        assert!(!shows(&buf, "/dev/sd3"));
        assert_eq!(app.click_areas.len(), 3);
        let (_, y0) = at(&buf, "/dev/sd0");
        let (_, y1) = at(&buf, "/dev/sd1");
        assert_eq!(y1 - y0, 4);
    }

    #[test]
    fn the_configure_screen_hides_passwords_and_marks_the_focused_field() {
        let mut app = ready(Step::Configure);
        app.timezone = "Europe/Paris".into();
        app.password = "Secret12".into();
        app.password2 = "Secret1".into();
        app.cfg_field = 2;
        let buf = draw(&mut app);
        assert!(shows(&buf, "System configuration"));
        assert!(shows(&buf, " Europe/Paris "));
        assert!(!shows(&buf, "Europe/Paris▌"));
        assert!(shows(&buf, " ••••••••"));
        assert!(shows(&buf, " •••••••▌"));
        assert!(!shows(&buf, "Secret"));
        assert_eq!(fg(&buf, "Confirm password"), PURPLE);
        assert_eq!(fg(&buf, "Timezone"), MUTED);
        for (label, id) in [
            ("Timezone", 0),
            ("Password", 1),
            ("Confirm password", 2),
            ("SSH public key", 3),
        ] {
            assert_eq!(
                target_at(&app, at(&buf, label)),
                Some(ClickTarget::CfgField(id)),
                "{label}"
            );
        }
        assert_eq!(
            target_at(&app, at(&buf, "Generate SSH key")),
            Some(ClickTarget::Btn(BtnId::GenSshKey))
        );
        assert!(!shows(&buf, "Private key"));
    }

    #[test]
    fn a_generated_private_key_is_shown_once_shortened() {
        let mut app = ready(Step::Configure);
        app.gen_privkey = Some("k".repeat(49) + "LMN");
        app.error = Some("Passwords do not match.".into());
        let buf = draw(&mut app);
        assert_eq!(fg(&buf, "Private key (save it!):"), RED);
        assert!(shows(&buf, &format!("{}L…", "k".repeat(49))));
        assert!(!shows(&buf, "LM"));
        assert!(shows(&buf, "⚠  Passwords do not match."));

        app.gen_privkey = Some("short".into());
        let buf = draw(&mut app);
        assert!(shows(&buf, "(save it!): short"));
        assert!(!shows(&buf, "short…"));
    }

    #[test]
    fn the_install_log_colours_errors_and_successes_and_keeps_the_latest_lines() {
        let mut app = ready(Step::Install);
        app.log_lines = (0..30).map(|i| format!("line {i:02}")).collect();
        app.log_lines.push("ERROR: disko failed".into());
        app.log_lines.push("✓ Disk partitioned".into());
        app.log_lines.push("install complete".into());
        let buf = draw_sized(&mut app, W, 30);
        assert!(shows(&buf, "Installing YoLab…"));
        assert!(shows(&buf, "Progress"));
        assert_eq!(fg(&buf, "ERROR: disko failed"), RED);
        assert_eq!(fg(&buf, "✓ Disk partitioned"), GREEN);
        assert_eq!(fg(&buf, "install complete"), GREEN);
        assert_eq!(fg(&buf, "line 29"), MUTED);
        assert!(!shows(&buf, "line 00"));
        let visible = (0..30)
            .filter(|i| shows(&buf, &format!("line {i:02}")))
            .count();
        assert_eq!(visible, 14);
    }

    #[test]
    fn a_finished_install_shows_the_url_and_the_power_buttons() {
        let mut app = ready(Step::Install);
        app.install_done = true;
        app.mgmt_url = Some("https://node1.alice.yolab.io".into());
        let buf = draw(&mut app);
        assert_eq!(fg(&buf, "✓  Installation complete!"), GREEN);
        assert_eq!(fg(&buf, "https://node1.alice.yolab.io"), PURPLE);
        assert!(shows(&buf, "1.  Remove the installation USB drive"));
        assert_eq!(
            target_at(&app, at(&buf, "r  Reboot ")),
            Some(ClickTarget::Btn(BtnId::Reboot))
        );
        assert_eq!(
            target_at(&app, at(&buf, "p  Power off")),
            Some(ClickTarget::Btn(BtnId::Poweroff))
        );
        assert_eq!(bg(&buf, "r  Reboot "), PURPLE);
        assert_eq!(fg(&buf, "r  Reboot "), Color::Black);
        assert_eq!(fg(&buf, "p  Power off"), MUTED);
        assert_eq!(bg(&buf, "p  Power off"), Color::Reset);

        app.mgmt_url = None;
        let buf = draw(&mut app);
        assert!(!shows(&buf, "Management URL"));
    }

    #[test]
    fn a_failed_install_shows_why() {
        let mut app = ready(Step::Install);
        app.install_failed = true;
        app.error = Some("disko failed".into());
        let buf = draw(&mut app);
        assert_eq!(fg(&buf, "✗  Installation failed"), RED);
        assert!(shows(&buf, "disko failed"));
        assert!(!shows(&buf, "Progress"));

        app.error = None;
        let buf = draw(&mut app);
        assert!(shows(&buf, "Unknown error"));
    }

    #[test]
    fn the_new_cluster_card_sits_above_the_join_card() {
        let mut app = ready(Step::Mode);
        let buf = draw(&mut app);
        assert_eq!(at(&buf, "New cluster"), (30, 8));
        assert_eq!(at(&buf, "Join existing cluster"), (30, 14));
    }

    #[test]
    fn the_connected_account_continue_button_sits_under_the_message() {
        let mut app = ready(Step::Account);
        app.account_token = Some("tok".into());
        let buf = draw(&mut app);
        assert_eq!(at(&buf, "Continue →"), (29, 18));
    }

    #[test]
    fn the_referral_field_its_note_and_the_create_button_are_stacked_in_order() {
        let mut app = ready(Step::Account);
        let buf = draw(&mut app);
        assert_eq!(at(&buf, "Referral code (optional)"), (29, 13));
        assert_eq!(at(&buf, "No email needed."), (27, 16));
        assert_eq!(at(&buf, "Create account →"), (29, 18));
    }

    #[test]
    fn the_token_field_sits_above_the_verify_button() {
        let mut app = ready(Step::Account);
        app.acct_cursor = 1;
        let buf = draw(&mut app);
        assert_eq!(at(&buf, " Token "), (28, 14));
        assert_eq!(at(&buf, "Verify token →"), (29, 18));
    }

    #[test]
    fn a_new_token_of_exactly_thirty_characters_is_shown_whole_above_its_continue_button() {
        let mut app = ready(Step::Account);
        let token = "t".repeat(30);
        app.created_token = Some(token.clone());
        let buf = draw(&mut app);
        assert!(shows(&buf, &format!("Token: {token}…")));
        assert_eq!(at(&buf, "Continue →"), (29, 42));
    }

    #[test]
    fn the_erase_warning_sits_between_the_disk_heading_and_the_first_disk() {
        let mut app = ready(Step::Disk);
        app.disks = vec![disk("/dev/sda", false, false, false)];
        let buf = draw(&mut app);
        assert_eq!(at(&buf, "Select installation disk").1, 4);
        assert_eq!(
            at(&buf, "The selected disk will be completely erased.").1,
            7
        );
        assert_eq!(at(&buf, "/dev/sda").1, 10);
    }

    #[test]
    fn a_disk_that_ends_exactly_at_the_error_line_is_still_shown() {
        let mut app = ready(Step::Disk);
        app.disks = (0..4)
            .map(|i| disk(&format!("/dev/sd{i}"), false, false, false))
            .collect();
        let buf = draw_sized(&mut app, W, 27);
        assert!(shows(&buf, "/dev/sd2"));
        assert!(!shows(&buf, "/dev/sd3"));
        assert_eq!(app.click_areas.len(), 3);
    }

    #[test]
    fn a_private_key_of_exactly_fifty_characters_is_shown_whole() {
        let mut app = ready(Step::Configure);
        let key = "k".repeat(50);
        app.gen_privkey = Some(key.clone());
        let buf = draw(&mut app);
        assert!(shows(&buf, &format!("(save it!): {key}")));
        assert!(!shows(&buf, &format!("{key}…")));
    }

    #[test]
    fn a_focused_input_has_a_purple_border_and_a_cursor() {
        let mut app = ready(Step::Configure);
        let buf = draw(&mut app);
        let (x, y) = at(&buf, " Timezone ");
        assert_eq!(buf.cell((x - 1, y)).unwrap().fg, PURPLE);
        let (x, y) = at(&buf, " Password ");
        assert_eq!(buf.cell((x - 1, y)).unwrap().fg, BORDER);
    }
}
