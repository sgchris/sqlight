//! Startup, terminal lifecycle, and the main event loop.

mod app;
mod cli;
mod config;
mod db;
mod editor;
mod parser;
mod storage;
mod table_view;
mod ui;

use crossterm::event::{self, Event, KeyEventKind};
use std::io::Write;
use std::time::Duration;

use app::App;
use cli::Cli;

fn main() {
    if let Err(code) = run() {
        std::process::exit(code);
    }
}

fn run() -> Result<(), i32> {
    let cli = Cli::parse_args();

    // File IO problems exit *before* entering the TUI, with a message.
    if let Err(msg) = db::preflight_db(&cli.db_path) {
        let _ = writeln!(std::io::stderr(), "{msg}");
        return Err(1);
    }

    // Terminal init. Any failure here is also a plain stderr exit.
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "Error: cannot start terminal UI: {e}");
            return Err(1);
        }
    };

    let mut app = App::new(cli.db_path);
    app.history_file = storage::history_file_path();
    app.load_history();
    app.refresh_schema();

    terminal.draw(|f| ui::render(f, &app)).ok();
    loop {
        // Block for input; repaint only when something happened.
        // The 500ms tick also lets a stale Ctrl+C confirm lapse so the
        // bottom bar reverts even with no further keypresses.
        match event::poll(Duration::from_millis(500)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    terminal.draw(|f| ui::render(f, &app)).ok();
                }
                Ok(_) => {
                    app.expire_quit_arm();
                    terminal.draw(|f| ui::render(f, &app)).ok();
                }
                Err(_) => break,
            },
            Ok(false) => {
                if app.expire_quit_arm() {
                    terminal.draw(|f| ui::render(f, &app)).ok();
                }
            }
            Err(_) => break,
        }
        if app.should_quit {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}
