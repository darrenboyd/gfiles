//! The interactive TUI: `gfiles` with no subcommand.
//!
//! It draws to stderr, not stdout, so that `$(gfiles)` captures only the path chosen with
//! Enter, as with fzf. Crossterm reads keys from the terminal even when stdout is a pipe.

mod app;
mod ui;

use std::io::{self, Stderr};

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use rusqlite::Connection;

use app::{App, Outcome};

/// Runs the TUI until the user quits. Returns the path chosen with Enter, if any.
pub fn run(conn: Connection) -> Result<Option<String>> {
    let mut app = App::new(conn)?;
    let mut terminal = start()?;
    let result = event_loop(&mut terminal, &mut app);
    stop();
    result?;
    Ok(match app.outcome {
        Some(Outcome::Selected(path)) => Some(path),
        _ => None,
    })
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stderr>>, app: &mut App) -> Result<()> {
    while app.outcome.is_none() {
        terminal.draw(|frame| ui::draw(frame, app))?;
        // On Windows a key produces both press and release events; act on presses only.
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }
    }
    Ok(())
}

fn start() -> Result<Terminal<CrosstermBackend<Stderr>>> {
    // Put the terminal back if anything panics, then report the panic as usual.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        stop();
        previous(info);
    }));
    enable_raw_mode()?;
    execute!(io::stderr(), EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(io::stderr()))?)
}

fn stop() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stderr(),
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
}
