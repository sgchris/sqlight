//! Startup, terminal lifecycle, and the main event loop.

mod app;
mod cli;
mod config;
mod connections;
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
use connections::Target;
use db::Database;

fn main() {
    if let Err(code) = run() {
        std::process::exit(code);
    }
}

fn run() -> Result<(), i32> {
    let cli = Cli::parse_args();

    let Some(target) = cli.target else {
        let msg = connections::missing_target_message(&connections::connections_file_display());
        let _ = writeln!(std::io::stderr(), "{msg}");
        return Err(2);
    };

    // Resolution, file IO and connection problems exit *before* entering
    // the TUI, with a message.
    let database = match open_database(&target) {
        Ok(d) => d,
        Err(msg) => {
            let _ = writeln!(std::io::stderr(), "{msg}");
            return Err(1);
        }
    };

    // Terminal init. Any failure here is also a plain stderr exit.
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "Error: cannot start terminal UI: {e}");
            return Err(1);
        }
    };

    let mut app = App::new(database);
    app.history_file = storage::history_file_path();
    app.load_history();
    app.refresh_schema();

    terminal.draw(|f| ui::render(f, &mut app)).ok();
    loop {
        // Block for input; repaint only when something happened.
        // The 500ms tick also lets a stale Ctrl+C confirm lapse so the
        // bottom bar reverts even with no further keypresses.
        match event::poll(Duration::from_millis(500)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    terminal.draw(|f| ui::render(f, &mut app)).ok();
                }
                Ok(_) => {
                    app.expire_quit_arm();
                    terminal.draw(|f| ui::render(f, &mut app)).ok();
                }
                Err(_) => break,
            },
            Ok(false) => {
                if app.expire_quit_arm() {
                    terminal.draw(|f| ui::render(f, &mut app)).ok();
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

/// Resolve `target` (SQLite file first, then named connection) and open it,
/// prompting for a password when the connection entry has none.
fn open_database(target: &str) -> Result<Database, String> {
    match connections::resolve_target(target)? {
        Target::Sqlite(path) => {
            db::sqlite::preflight_db(&path)?;
            Ok(Database::Sqlite(path))
        }
        Target::Postgres { name, cfg } => {
            let password = match &cfg.password {
                Some(p) => p.clone(),
                None => rpassword::prompt_password(format!(
                    "Password for {}@{} ({name}): ",
                    cfg.user, cfg.host
                ))
                .map_err(|e| format!("Error: cannot read password: {e}"))?,
            };
            let pg = db::PgDb::connect(&name, &cfg, &password)?;
            Ok(Database::Postgres(Box::new(pg)))
        }
    }
}
