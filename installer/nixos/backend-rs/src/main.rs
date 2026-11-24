mod app;
mod effects;
#[cfg(test)]
mod fake;
mod install;
mod lines;
mod machine;
mod seam;
mod ui;
mod wireguard;

use std::io;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio_stream::StreamExt;

use crate::machine::RealMachine;
use crate::seam::Machine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = run(&mut terminal).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    let machine = RealMachine::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app::App::new(app::boot_mode(machine.has_efi()));

    {
        let (m, tx) = (machine.clone(), tx.clone());
        tokio::spawn(async move { effects::wait_for_network(&m, &tx).await });
    }

    let mut events = EventStream::new();
    loop {
        app.click_areas.clear();
        terminal.draw(|f| ui::render(f, &mut app))?;

        tokio::select! {
            maybe = events.next() => {
                let Some(event) = maybe else { return Ok(()) };
                match event? {
                    Event::Key(k) => {
                        if app.handle_key(k) {
                            return Ok(());
                        }
                    }
                    Event::Mouse(m) => app.handle_mouse(m),
                    _ => {}
                }
            }
            Some(ev) = rx.recv() => app.handle_app_event(ev),
        }

        for effect in app.effects.drain(..) {
            let (m, tx) = (machine.clone(), tx.clone());
            tokio::spawn(async move { effects::perform(&m, effect, &tx).await });
        }
    }
}
