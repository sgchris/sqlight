//! Application state: input/table modes, scrollback, key dispatch,
//! and glue between the editor, parser and database layers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::time::Instant;

use crate::config::{QUIT_CONFIRM_TIMEOUT, SCROLLBACK_LIMIT};
use crate::db::{self, SchemaCache};
use crate::editor::{Completer, History, InputBuffer};
use crate::parser::{self, DotCommand, StatementKind};
use crate::storage;
use crate::table_view::TableView;

/// Which screen the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Input,
    Table,
}

/// One scrollback entry with its severity (drives color).
#[derive(Debug, Clone)]
pub struct ScrollLine {
    pub text: String,
    pub kind: LineKind,
}

/// Severity of a scrollback line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Echo of the user's command (plain).
    Echo,
    /// Successful result (green).
    Ok,
    /// Failure (light red).
    Err,
    /// Advisory notice, e.g. truncation or empty DB (orange).
    Warn,
}

/// Full application state. Rendered statelessly by `ui::render`.
pub struct App {
    pub db_path: PathBuf,
    pub mode: Mode,
    pub input: InputBuffer,
    pub history: History,
    /// Persistence target for Up/Down history. `None` keeps history
    /// memory-only (used by tests); the real binary sets the per-user
    /// file from `storage::history_file_path` at startup.
    pub history_file: Option<PathBuf>,
    pub completer: Completer,
    pub schema: SchemaCache,
    pub scrollback: Vec<ScrollLine>,
    /// Lines scrolled up from the bottom of the scrollback (0 = follow).
    pub scroll_offset: usize,
    pub table: Option<TableView>,
    pub should_quit: bool,
    /// First Ctrl+C timestamp in input mode; second one within the
    /// timeout confirms quit.
    pub quit_armed_at: Option<Instant>,
}

impl App {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            mode: Mode::Input,
            input: InputBuffer::new(),
            history: History::new(),
            history_file: None,
            completer: Completer::new(),
            schema: SchemaCache::default(),
            scrollback: Vec::new(),
            scroll_offset: 0,
            table: None,
            should_quit: false,
            quit_armed_at: None,
        }
    }

    /// Load persisted Up/Down history into memory. No-op when no
    /// `history_file` is set; missing/corrupt files load as empty.
    pub fn load_history(&mut self) {
        if let Some(path) = self.history_file.clone() {
            self.history = storage::load_history_from_file(&path);
        }
    }

    /// Remember one command in memory and persist it. Best-effort:
    /// file failures never surface to the user.
    fn push_history(&mut self, entry: String) {
        self.history.push(entry);
        if let Some(path) = self.history_file.clone() {
            let _ = storage::save_history_to_file(&self.history, &path);
        }
    }

    /// (Re)load table/column names for autocompletion.
    pub fn refresh_schema(&mut self) {
        self.schema = db::refresh_schema_cache(&self.db_path);
        let s = self.schema.clone();
        self.completer.set_schema(s.tables, s.columns);
    }

    // -- scrollback ---------------------------------------------------------

    pub fn push_line(&mut self, text: impl Into<String>, kind: LineKind) {
        self.scrollback.push(ScrollLine {
            text: text.into(),
            kind,
        });
        if self.scrollback.len() > SCROLLBACK_LIMIT {
            let overflow = self.scrollback.len() - SCROLLBACK_LIMIT;
            self.scrollback.drain(..overflow);
        }
        // New output follows the bottom.
        self.scroll_offset = 0;
    }

    pub fn scroll_output_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    pub fn scroll_output_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    // -- quit confirmation ----------------------------------------------------

    /// Whether a first Ctrl+C is still pending confirmation.
    pub fn is_quit_armed(&self) -> bool {
        self.quit_armed_at
            .is_some_and(|t| t.elapsed() < QUIT_CONFIRM_TIMEOUT)
    }

    /// Clear a stale first-Ctrl+C marker. Returns true if state changed.
    pub fn expire_quit_arm(&mut self) -> bool {
        if let Some(t) = self.quit_armed_at
            && t.elapsed() >= QUIT_CONFIRM_TIMEOUT
        {
            self.quit_armed_at = None;
            return true;
        }
        false
    }

    fn disarm_quit(&mut self) {
        self.quit_armed_at = None;
    }

    // -- key dispatch --------------------------------------------------------

    /// Handle one key event. Returns nothing; check `should_quit` after.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if is_ctrl_c(key) {
            match self.mode {
                Mode::Table => {
                    // Never quit from the grid; just back to the prompt.
                    self.close_table();
                    self.disarm_quit();
                    return;
                }
                Mode::Input => {
                    if self.is_quit_armed() {
                        self.should_quit = true;
                        self.completer.dismiss();
                    } else {
                        self.quit_armed_at = Some(Instant::now());
                        self.completer.dismiss();
                    }
                    return;
                }
            }
        }
        if is_ctrl_d(key) {
            self.should_quit = true;
            self.completer.dismiss();
            return;
        }
        // Stale confirm markers lapse even while the user keeps typing.
        self.expire_quit_arm();
        match self.mode {
            Mode::Input => self.handle_input_key(key),
            Mode::Table => self.handle_table_key(key),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.completer.dismiss();
                self.submit_input();
            }
            KeyCode::Tab => {
                self.tab_complete(false);
            }
            KeyCode::BackTab => {
                self.tab_complete(true);
            }
            KeyCode::Esc => {
                self.completer.dismiss();
            }
            KeyCode::Backspace => {
                self.completer.dismiss();
                self.input.backspace();
            }
            KeyCode::Delete => {
                self.completer.dismiss();
                self.input.delete_forward();
            }
            KeyCode::Left => {
                self.completer.dismiss();
                self.input.move_left();
            }
            KeyCode::Right => {
                self.completer.dismiss();
                self.input.move_right();
            }
            KeyCode::Up => {
                self.completer.dismiss();
                if !self.input.move_up() {
                    let cur = self.input.full_text();
                    if let Some(entry) = self.history.move_up(&cur) {
                        self.input.set_text(&entry);
                    }
                }
            }
            KeyCode::Down => {
                self.completer.dismiss();
                if !self.input.move_down()
                    && self.history.browsing()
                    && let Some(entry) = self.history.move_down()
                {
                    self.input.set_text(&entry);
                }
            }
            KeyCode::Home => {
                self.completer.dismiss();
                self.input.col = 0;
            }
            KeyCode::End => {
                self.completer.dismiss();
                self.input.col = self.input.lines()[self.input.row].chars().count();
            }
            KeyCode::PageUp => self.scroll_output_up(10),
            KeyCode::PageDown => self.scroll_output_down(10),
            KeyCode::Char(ch) => {
                self.completer.dismiss();
                // Plain typing (modifiers other than Shift are shortcuts).
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    self.input.insert_char(ch);
                }
            }
            _ => {}
        }
    }

    fn handle_table_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_table(),
            KeyCode::Char('w' | 'W') if key.modifiers.is_empty() => {
                if let Some(t) = self.table.as_mut() {
                    t.toggle_wrap();
                }
            }
            KeyCode::Char('q' | 'Q') if key.modifiers.is_empty() => self.close_table(),
            KeyCode::Up => self.table.as_mut().map(|t| t.scroll_up(1)).unwrap_or(()),
            KeyCode::Down => self.table.as_mut().map(|t| t.scroll_down(1)).unwrap_or(()),
            KeyCode::Left => self.table.as_mut().map(|t| t.scroll_left(1)).unwrap_or(()),
            KeyCode::Right => self.table.as_mut().map(|t| t.scroll_right(1)).unwrap_or(()),
            KeyCode::PageUp => self.table.as_mut().map(|t| t.scroll_up(10)).unwrap_or(()),
            KeyCode::PageDown => self.table.as_mut().map(|t| t.scroll_down(10)).unwrap_or(()),
            KeyCode::Home => {
                if let Some(t) = self.table.as_mut() {
                    t.offset_y = 0;
                    t.offset_x = 0;
                }
            }
            KeyCode::End => {
                if let Some(t) = self.table.as_mut() {
                    t.offset_y = t.row_count().saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    fn close_table(&mut self) {
        self.mode = Mode::Input;
        self.table = None;
        self.completer.dismiss();
    }

    fn tab_complete(&mut self, shift: bool) {
        let (word, _) = self.input.word_before_cursor();
        if let Some(done) = self.completer.complete(&word, shift) {
            self.input.replace_word_before_cursor(&done);
        }
    }

    // -- execution -----------------------------------------------------------

    fn submit_input(&mut self) {
        let text = self.input.full_text();
        if text.trim().is_empty() {
            return;
        }
        // Dot-command path (no trailing `;` required).
        if let Some(parsed) = parser::parse_dot_command(&text) {
            match parsed {
                Ok(cmd) => {
                    self.push_history(text.trim().to_string());
                    self.push_line(
                        format!("# {}", text.trim().replace('\n', " ")),
                        LineKind::Echo,
                    );
                    self.execute_dot(cmd);
                }
                Err(e) => {
                    // Only treat leading-dot lines as dot errors; other text
                    // falls through to SQL handling below.
                    if text.trim_start().starts_with('.') {
                        self.push_history(text.trim().to_string());
                        self.push_line(
                            format!("# {}", text.trim().replace('\n', " ")),
                            LineKind::Echo,
                        );
                        self.push_line(format!("Error: {e}"), LineKind::Err);
                        self.input.clear();
                    } else {
                        self.submit_sql(&text);
                    }
                }
            }
            return;
        }
        self.submit_sql(&text);
    }

    fn submit_sql(&mut self, text: &str) {
        if !parser::has_trailing_semi(text) {
            // Multiline convenience: keep editing on a new line.
            self.input.insert_newline();
            return;
        }
        let sql = parser::strip_trailing_semi(text);
        if sql.trim().is_empty() {
            self.input.clear();
            return;
        }
        self.push_history(text.trim().to_string());
        // Echo the command back (collapsed to flow in narrow windows is
        // handled by the renderer; keep full text here).
        self.push_line(format!("# {}", text.trim()), LineKind::Echo);
        match parser::classify(&sql) {
            StatementKind::Select => self.execute_select(&sql),
            StatementKind::Write => self.execute_write(&sql),
        }
        self.input.clear();
        // Schema may have changed (CREATE/DROP/ALTER) — refresh cheaply.
        self.refresh_schema();
    }

    fn execute_dot(&mut self, cmd: DotCommand) {
        match cmd {
            DotCommand::Tables => match db::list_tables(&self.db_path) {
                Ok(tables) => {
                    if tables.is_empty() {
                        self.push_line("Warning: no tables in database", LineKind::Warn);
                    } else {
                        for t in tables {
                            self.push_line(t, LineKind::Ok);
                        }
                    }
                }
                Err(e) => self.push_line(db::friendly_query_error(&e), LineKind::Err),
            },
            DotCommand::Schema { table } => match db::get_schema(&self.db_path, &table) {
                Ok(stmts) => {
                    for s in stmts {
                        let stmt = s.trim_end_matches(';').trim().to_string() + ";";
                        self.push_line(stmt, LineKind::Ok);
                    }
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    self.push_line(format!("Error: no such table: {table}"), LineKind::Err);
                }
                Err(e) => self.push_line(db::friendly_query_error(&e), LineKind::Err),
            },
        }
        self.input.clear();
    }

    fn execute_select(&mut self, sql: &str) {
        match db::query_select(&self.db_path, sql) {
            Ok(result) => {
                if result.headers.is_empty() {
                    self.push_line("OK".to_string(), LineKind::Ok);
                    return;
                }
                if result.rows.is_empty() {
                    self.push_line("Warning: 0 rows", LineKind::Warn);
                    return;
                }
                let truncated = result.truncated;
                self.table = Some(TableView::new(result));
                self.mode = Mode::Table;
                if truncated {
                    // Remembered for the table footer; also visible in history.
                    self.push_line(
                        format!(
                            "Warning: showing first {} rows (truncated)",
                            crate::config::MAX_ROWS
                        ),
                        LineKind::Warn,
                    );
                }
            }
            Err(e) => self.push_line(db::friendly_query_error(&e), LineKind::Err),
        }
    }

    fn execute_write(&mut self, sql: &str) {
        match db::execute_write(&self.db_path, sql) {
            Ok(n) => {
                let first = sql
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                match first.as_str() {
                    "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => {
                        let word = if n == 1 { "row" } else { "rows" };
                        self.push_line(format!("Affected {n} {word}"), LineKind::Ok);
                    }
                    _ => self.push_line("OK".to_string(), LineKind::Ok),
                }
            }
            Err(e) => self.push_line(db::friendly_query_error(&e), LineKind::Err),
        }
    }
}

/// Ctrl+C arms/confirms quit in input mode, closes the grid in table mode.
fn is_ctrl_c(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Ctrl+D quits immediately from anywhere.
fn is_ctrl_d(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('d' | 'D')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn quit_keys() {
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        let ctrl_d = KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        assert!(is_ctrl_c(ctrl_c));
        assert!(is_ctrl_d(ctrl_d));
        assert!(!is_ctrl_c(key(KeyCode::Char('c'))));
        assert!(!is_ctrl_c(key(KeyCode::Esc)));
        assert!(!is_ctrl_d(key(KeyCode::Esc)));
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn ctrl_c_twice_quits_from_input() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.handle_key(ctrl_c());
        assert!(!app.should_quit);
        assert!(app.is_quit_armed());
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_timeout_resets() {
        use std::time::Duration;
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.handle_key(ctrl_c());
        assert!(app.is_quit_armed());
        // Simulate the 3s window lapsing.
        app.quit_armed_at = Some(
            std::time::Instant::now()
                - crate::config::QUIT_CONFIRM_TIMEOUT
                - Duration::from_millis(10),
        );
        assert!(!app.is_quit_armed());
        assert!(app.expire_quit_arm());
        assert!(app.quit_armed_at.is_none());
        // Next Ctrl+C re-arms instead of quitting.
        app.handle_key(ctrl_c());
        assert!(!app.should_quit);
        assert!(app.is_quit_armed());
    }

    #[test]
    fn input_stays_usable_after_first_ctrl_c() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.handle_key(ctrl_c());
        assert!(app.is_quit_armed());
        type_text(&mut app, "select 1;");
        assert_eq!(app.input.full_text(), "select 1;");
        assert!(app.is_quit_armed());
        // Still quits on the confirming press.
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_from_table_goes_back_without_quitting() {
        use crate::db::QueryResult;
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec!["1".to_string()]],
            truncated: false,
        }));
        app.mode = Mode::Table;
        app.handle_key(ctrl_c());
        assert_eq!(app.mode, Mode::Input);
        assert!(app.table.is_none());
        assert!(!app.should_quit);
        assert!(!app.is_quit_armed());
    }

    #[test]
    fn table_ctrl_c_does_not_count_towards_quit() {
        use crate::db::QueryResult;
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.handle_key(ctrl_c());
        assert!(app.is_quit_armed());
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec!["1".to_string()]],
            truncated: false,
        }));
        app.mode = Mode::Table;
        app.handle_key(ctrl_c());
        assert_eq!(app.mode, Mode::Input);
        assert!(!app.should_quit);
        assert!(!app.is_quit_armed());
        // Needs two fresh presses to quit now.
        app.handle_key(ctrl_c());
        assert!(!app.should_quit);
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn enter_without_semi_continues_multiline() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        for c in "select 1".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Input);
        assert_eq!(app.input.lines().len(), 2);
        assert!(app.history.is_empty());
    }

    #[test]
    fn up_restores_history_with_caret_at_end() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        app.history.push("line1\nline2".to_string());
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.full_text(), "line1\nline2");
        assert_eq!((app.input.row, app.input.col), (1, 5));
    }

    #[test]
    fn scrollback_is_capped() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        for i in 0..SCROLLBACK_LIMIT + 50 {
            app.push_line(format!("line {i}"), LineKind::Echo);
        }
        assert_eq!(app.scrollback.len(), SCROLLBACK_LIMIT);
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn seed_e2e_db() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("sqlight-e2e-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let conn = rusqlite::Connection::open(&p).expect("create e2e db");
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO users (name) VALUES ('greg'), ('ana');",
        )
        .expect("seed e2e db");
        p
    }

    #[test]
    fn end_to_end_select_table_dot_write_error_history() {
        let path = seed_e2e_db();
        let mut app = App::new(path.clone());
        app.refresh_schema();
        assert!(app.schema.tables.contains(&"users".to_string()));

        // SELECT -> grid mode with 2 rows.
        type_text(&mut app, "select * from users;");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.table.as_ref().expect("table").row_count(), 2);

        // ESC -> back to prompt, history preserved.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Input);
        assert!(app.table.is_none());

        // Internal command, no semicolon needed.
        type_text(&mut app, ".tables");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.text == "users" && l.kind == LineKind::Ok),
            "scrollback: {:?}",
            app.scrollback.iter().map(|l| &l.text).collect::<Vec<_>>()
        );

        // Write -> affected-rows confirmation.
        type_text(&mut app, "delete from users where name = 'ana';");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.text == "Affected 1 row" && l.kind == LineKind::Ok)
        );

        // Bad SQL -> light-red error line, app stays alive.
        type_text(&mut app, "select * from nope;");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Input);
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.kind == LineKind::Err && l.text.starts_with("Error:"))
        );

        // History: Up restores the last command whole, caret at end.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.full_text(), "select * from nope;");

        // Ctrl+C twice quits from input mode (first arms, second confirms).
        app.handle_key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert!(!app.should_quit);
        assert!(app.is_quit_armed());
        app.handle_key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert!(app.should_quit);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multiline_statement_executes_on_terminating_semi() {
        let path = seed_e2e_db();
        let mut app = App::new(path.clone());
        app.refresh_schema();
        type_text(&mut app, "update users set name = 'greg2'");
        app.handle_key(key(KeyCode::Enter)); // no `;` -> newline, no execution
        assert_eq!(app.mode, Mode::Input);
        assert_eq!(app.input.lines().len(), 2);
        type_text(&mut app, "where id = 1;");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.text == "Affected 1 row" && l.kind == LineKind::Ok)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn history_persists_across_sessions_via_file() {
        let dir = std::env::temp_dir().join(format!(
            "sqlight-app-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let history_path = dir.join("history");

        // First session writes (multiline entry included).
        let mut first = App::new(PathBuf::from("dummy.db"));
        first.history_file = Some(history_path.clone());
        first.load_history();
        assert!(first.history.is_empty());
        first.push_history("select 1;".to_string());
        first.push_history("line1\nline2;".to_string());
        assert!(history_path.is_file());

        // Second session loads what the first one stored.
        let mut second = App::new(PathBuf::from("dummy.db"));
        second.history_file = Some(history_path.clone());
        second.load_history();
        assert_eq!(
            second.history.entries(),
            &["select 1;".to_string(), "line1\nline2;".to_string()]
        );
        // Up restores the multiline entry whole, caret at end.
        second.handle_key(key(KeyCode::Up));
        assert_eq!(second.input.full_text(), "line1\nline2;");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_stays_memory_only_without_file() {
        let mut app = App::new(PathBuf::from("dummy.db"));
        assert!(app.history_file.is_none());
        app.push_history("select 1;".to_string());
        assert_eq!(app.history.entries(), &["select 1;".to_string()]);
    }
}
