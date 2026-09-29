//! Application state: input/table modes, scrollback, key dispatch,
//! and glue between the editor, parser and database layers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{
    OUTPUT_SCROLL_LINE, OUTPUT_SCROLL_PAGE, QUIT_CONFIRM_TIMEOUT, REFRESH_FLASH, SCROLLBACK_LIMIT,
    SPINNER_DELAY,
};
use crate::db::{Database, DbError, QueryResult, SchemaCache};
use crate::editor::{Completer, History, InputBuffer};
use crate::parser::{self, DotCommand, StatementKind};
use crate::storage;
use crate::table_view::{self, TableView};

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

/// Statement executed on the worker thread.
enum Job {
    Select(String),
    Write(String),
    /// Re-run of the grid's SELECT (`r` in table mode).
    Refresh(String),
}

/// What the worker sends back for a `Job`.
enum Outcome {
    Select(String, Result<QueryResult, DbError>),
    Write(String, Result<usize, DbError>),
    Refresh(Result<QueryResult, DbError>),
}

impl Job {
    fn run(self, db: &mut Database) -> Outcome {
        match self {
            Job::Select(sql) => {
                let r = db.query_select(&sql);
                Outcome::Select(sql, r)
            }
            Job::Write(sql) => {
                let r = db.execute_write(&sql);
                Outcome::Write(sql, r)
            }
            Job::Refresh(sql) => Outcome::Refresh(db.query_select(&sql)),
        }
    }
}

/// A statement in flight on the worker thread.
struct RunningQuery {
    started: Instant,
    label: &'static str,
    rx: mpsc::Receiver<(Outcome, Duration)>,
}

/// Full application state. Rendered by `ui::render` (which clamps
/// `scroll_offset` to the current viewport so overscroll can't accumulate).
pub struct App {
    /// Shared with the worker thread; the UI thread only locks it while
    /// no statement is running, so rendering never blocks on the database.
    db: Arc<Mutex<Database>>,
    /// Status-bar description of `db`, cached so rendering needs no lock.
    pub db_label: String,
    running: Option<RunningQuery>,
    /// When the grid was last refreshed successfully (drives the
    /// short-lived "Refreshed" badge).
    pub refreshed_at: Option<Instant>,
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
    /// SELECT that produced `table`; re-run by `r` in table mode.
    pub table_sql: Option<String>,
    pub should_quit: bool,
    /// First Ctrl+C timestamp in input mode; second one within the
    /// timeout confirms quit.
    pub quit_armed_at: Option<Instant>,
}

impl App {
    pub fn new(db: Database) -> Self {
        Self {
            db_label: db.label(),
            db: Arc::new(Mutex::new(db)),
            running: None,
            refreshed_at: None,
            mode: Mode::Input,
            input: InputBuffer::new(),
            history: History::new(),
            history_file: None,
            completer: Completer::new(),
            schema: SchemaCache::default(),
            scrollback: Vec::new(),
            scroll_offset: 0,
            table: None,
            table_sql: None,
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
        self.schema = lock_db(&self.db).refresh_schema_cache();
        let s = self.schema.clone();
        self.completer
            .set_schema(s.tables, s.columns, s.table_columns);
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

    /// Scroll the output viewport toward older lines (up).
    /// Clamped lazily at render time against the current viewport height.
    pub fn scroll_output_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    /// Scroll the output viewport toward newer lines (down, back to follow).
    pub fn scroll_output_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Drop every scrollback line and snap the viewport back to the bottom.
    pub fn clear_output(&mut self) {
        self.scrollback.clear();
        self.scroll_offset = 0;
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
    /// Every input-buffer change made by the key becomes an undo step.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.is_busy() {
            // Only quitting is possible until the statement finishes.
            if is_ctrl_d(key) {
                self.should_quit = true;
            }
            return;
        }
        if self.mode == Mode::Input && self.handle_undo_key(key) {
            return;
        }
        let before = self.input.snapshot();
        self.dispatch_key(key);
        self.input.record_edit(before, typed_char(key));
    }

    /// Cmd/Ctrl+Z undoes; Cmd/Ctrl+Shift+Z and Ctrl+Y redo (legacy
    /// terminals send Ctrl+Shift+Z as plain Ctrl+Z). Returns true when consumed.
    fn handle_undo_key(&mut self, key: KeyEvent) -> bool {
        let m = key.modifiers;
        let cmd = m.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Char('z') if cmd && !m.contains(KeyModifiers::SHIFT) => {
                self.input.undo();
            }
            KeyCode::Char('z' | 'Z') if cmd => {
                self.input.redo();
            }
            KeyCode::Char('y' | 'Y') if m.contains(KeyModifiers::CONTROL) => {
                self.input.redo();
            }
            _ => return false,
        }
        self.completer.dismiss();
        true
    }

    fn dispatch_key(&mut self, key: KeyEvent) {
        if is_ctrl_c(key) {
            match self.mode {
                Mode::Table => {
                    // Never quit from the grid; just back to the prompt.
                    self.close_table();
                    self.disarm_quit();
                    return;
                }
                Mode::Input => {
                    // Non-empty input: abort the draft (all lines), stay alive.
                    // This is the escape hatch for unterminated multiline
                    // statements (e.g. an unbalanced quote swallowing `;`).
                    if !self.input.is_blank() {
                        self.input.clear();
                        self.completer.dismiss();
                        self.disarm_quit();
                        return;
                    }
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

    /// Word/line navigation and deletion shortcuts. Covers the variants
    /// macOS terminals emit: Option as Alt (or `ESC b`/`ESC f`), Cmd as
    /// Super (kitty protocol) or translated to Home/End, Ctrl+A/E/U/K.
    /// Returns true when the key was consumed.
    fn handle_edit_shortcut(&mut self, key: KeyEvent) -> bool {
        let m = key.modifiers;
        let alt = m.contains(KeyModifiers::ALT);
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let sup = m.contains(KeyModifiers::SUPER);
        let input = &mut self.input;
        match key.code {
            KeyCode::Left if sup => input.move_line_start(),
            KeyCode::Right if sup => input.move_line_end(),
            KeyCode::Left if alt || ctrl => input.move_word_left(),
            KeyCode::Right if alt || ctrl => input.move_word_right(),
            KeyCode::Char('b' | 'B') if alt => input.move_word_left(),
            KeyCode::Char('f' | 'F') if alt => input.move_word_right(),
            KeyCode::Char('a' | 'A') if ctrl => input.move_line_start(),
            KeyCode::Char('e' | 'E') if ctrl => input.move_line_end(),
            KeyCode::Backspace if sup => {
                input.delete_to_line_start();
            }
            KeyCode::Backspace if alt || ctrl => {
                input.delete_word_before();
            }
            KeyCode::Char('w' | 'W') if ctrl => {
                input.delete_word_before();
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                input.delete_to_line_start();
            }
            KeyCode::Delete if sup => {
                input.delete_to_line_end();
            }
            KeyCode::Delete if alt || ctrl => {
                input.delete_word_after();
            }
            KeyCode::Char('d' | 'D') if alt => {
                input.delete_word_after();
            }
            KeyCode::Char('k' | 'K') if ctrl => {
                input.delete_to_line_end();
            }
            _ => return false,
        }
        self.completer.dismiss();
        true
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        if self.handle_edit_shortcut(key) {
            return;
        }
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
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.scroll_output_up(OUTPUT_SCROLL_LINE);
                } else if !self.input.move_up() {
                    let cur = self.input.full_text();
                    if let Some(entry) = self.history.move_up(&cur) {
                        self.input.set_text(&entry);
                    }
                }
            }
            KeyCode::Down => {
                self.completer.dismiss();
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.scroll_output_down(OUTPUT_SCROLL_LINE);
                } else if !self.input.move_down()
                    && self.history.browsing()
                    && let Some(entry) = self.history.move_down()
                {
                    self.input.set_text(&entry);
                }
            }
            KeyCode::Home => {
                self.completer.dismiss();
                self.input.move_line_start();
            }
            KeyCode::End => {
                self.completer.dismiss();
                self.input.move_line_end();
            }
            KeyCode::PageUp => {
                self.completer.dismiss();
                self.scroll_output_up(OUTPUT_SCROLL_PAGE);
            }
            KeyCode::PageDown => {
                self.completer.dismiss();
                self.scroll_output_down(OUTPUT_SCROLL_PAGE);
            }
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
        let shift_or_none = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        if let Some(t) = self.table.as_mut() {
            match key.code {
                KeyCode::Char('J') if shift_or_none => return t.show_json(),
                KeyCode::Char('T') if shift_or_none => return t.show_table(),
                _ if t.is_json()
                    && !matches!(
                        key.code,
                        KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'r' | 'R')
                    ) =>
                {
                    return Self::handle_json_key(t, key);
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.close_table(),
            KeyCode::Char('w') if key.modifiers.is_empty() => {
                if let Some(t) = self.table.as_mut() {
                    t.toggle_wrap();
                }
            }
            KeyCode::Char('W')
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(t) = self.table.as_mut() {
                    t.toggle_full_wrap();
                }
            }
            KeyCode::Char('q' | 'Q') if key.modifiers.is_empty() => self.close_table(),
            KeyCode::Char('r' | 'R') if key.modifiers.is_empty() => self.refresh_table(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.table.as_mut().map(|t| t.scroll_up(1)).unwrap_or(())
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.table.as_mut().map(|t| t.scroll_down(1)).unwrap_or(())
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.table.as_mut().map(|t| t.scroll_left(1)).unwrap_or(())
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.table.as_mut().map(|t| t.scroll_right(1)).unwrap_or(())
            }
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

    /// JSON view keys: line scrolling only (Esc/q/r are handled as in the grid).
    fn handle_json_key(t: &mut TableView, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => t.json_scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') => t.json_scroll_down(1),
            KeyCode::PageUp => t.json_scroll_up(10),
            KeyCode::PageDown => t.json_scroll_down(10),
            KeyCode::Home => t.json_offset = 0,
            KeyCode::End => t.json_offset = usize::MAX,
            _ => {}
        }
    }

    fn close_table(&mut self) {
        self.mode = Mode::Input;
        self.table = None;
        self.table_sql = None;
        self.refreshed_at = None;
        self.completer.dismiss();
    }

    /// Whether the "Refreshed" badge should still be shown.
    pub fn is_refresh_flash(&self) -> bool {
        self.refreshed_at
            .is_some_and(|t| t.elapsed() < REFRESH_FLASH)
    }

    /// Drop a lapsed "Refreshed" badge. Returns true if state changed.
    pub fn expire_refresh_flash(&mut self) -> bool {
        if self.refreshed_at.is_some() && !self.is_refresh_flash() {
            self.refreshed_at = None;
            return true;
        }
        false
    }

    /// Re-run the grid's SELECT, keeping wrap mode and (clamped) scroll.
    /// On failure the error goes to scrollback and the grid closes.
    fn refresh_table(&mut self) {
        if let Some(sql) = self.table_sql.clone() {
            self.start_job(Job::Refresh(sql), "Refreshing");
        }
    }

    fn apply_refresh(&mut self, outcome: Result<QueryResult, DbError>, elapsed_ms: u128) {
        match outcome {
            Ok(result) => {
                let mut view = TableView::new(result);
                view.elapsed_ms = elapsed_ms;
                if let Some(old) = &self.table {
                    view.wrap = old.wrap;
                    view.view = old.view;
                    view.json_offset = old.json_offset;
                    view.offset_y = old.offset_y.min(view.row_count().saturating_sub(1));
                    view.offset_x = old.offset_x.min(view.headers.len().saturating_sub(1));
                }
                self.table = Some(view);
                self.refreshed_at = Some(Instant::now());
            }
            Err(e) => {
                self.close_table();
                self.push_line(e.message(), LineKind::Err);
            }
        }
    }

    fn tab_complete(&mut self, shift: bool) {
        let (word, _) = self.input.word_before_cursor();
        let query = self.input.full_text();
        if let Some(done) = self.completer.complete(&word, shift, &query) {
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
        self.input.clear();
        let job = match parser::classify(&sql) {
            StatementKind::Select => Job::Select(sql),
            StatementKind::Write => Job::Write(sql),
        };
        self.start_job(job, "Running query");
    }

    /// Whether a statement is executing on the worker thread.
    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// Label and elapsed time of the running statement, for the spinner.
    pub fn busy_status(&self) -> Option<(&'static str, Duration)> {
        self.running
            .as_ref()
            .map(|r| (r.label, r.started.elapsed()))
    }

    /// Apply the running statement's result if it has finished.
    /// Returns true when something changed on screen.
    pub fn poll_query(&mut self) -> bool {
        self.wait_query(Duration::ZERO)
    }

    /// Execute `job` on a worker thread so the UI keeps redrawing. Fast
    /// statements are awaited briefly, so they never flash the spinner.
    fn start_job(&mut self, job: Job, label: &'static str) {
        let db = Arc::clone(&self.db);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut db = lock_db(&db);
            let started = Instant::now();
            let outcome = job.run(&mut db);
            let _ = tx.send((outcome, started.elapsed()));
        });
        self.running = Some(RunningQuery {
            started: Instant::now(),
            label,
            rx,
        });
        self.wait_query(SPINNER_DELAY);
    }

    /// Wait up to `timeout` for the running statement and apply its result.
    /// Returns true when the statement finished (or its worker died).
    fn wait_query(&mut self, timeout: Duration) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        let received = match running.rx.recv_timeout(timeout) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => None,
        };
        self.running = None;
        match received {
            Some((outcome, elapsed)) => {
                self.apply_outcome(outcome, table_view::ceil_millis(elapsed));
            }
            None => self.push_line("Error: query stopped unexpectedly", LineKind::Err),
        }
        true
    }

    fn apply_outcome(&mut self, outcome: Outcome, elapsed_ms: u128) {
        match outcome {
            Outcome::Select(sql, r) => self.show_select(sql, r, elapsed_ms),
            Outcome::Write(sql, r) => self.show_write(&sql, r),
            Outcome::Refresh(r) => return self.apply_refresh(r, elapsed_ms),
        }
        // Schema may have changed (CREATE/DROP/ALTER) — refresh cheaply.
        self.refresh_schema();
    }

    fn execute_dot(&mut self, cmd: DotCommand) {
        match cmd {
            DotCommand::Tables => {
                let tables = lock_db(&self.db).list_tables();
                match tables {
                    Ok(tables) => {
                        if tables.is_empty() {
                            self.push_line("Warning: no tables in database", LineKind::Warn);
                        } else {
                            for t in tables {
                                self.push_line(t, LineKind::Ok);
                            }
                        }
                    }
                    Err(e) => self.push_line(e.message(), LineKind::Err),
                }
            }
            DotCommand::Schema { table } => {
                let schema = lock_db(&self.db).get_schema(&table);
                self.show_schema(&table, schema);
            }
            DotCommand::Clear => {
                // `submit_input` already echoed `# .clear`; drop everything
                // (history entry is kept) so the output is fully empty.
                self.clear_output();
            }
        }
        self.input.clear();
    }

    fn show_schema(&mut self, table: &str, schema: Result<Vec<String>, DbError>) {
        match schema {
            Ok(stmts) => {
                for s in stmts {
                    let stmt = s.trim_end_matches(';').trim().to_string() + ";";
                    self.push_line(stmt, LineKind::Ok);
                }
            }
            Err(DbError::NoSuchTable) => {
                self.push_line(format!("Error: no such table: {table}"), LineKind::Err);
            }
            Err(e) => self.push_line(e.message(), LineKind::Err),
        }
    }

    fn show_select(
        &mut self,
        sql: String,
        outcome: Result<QueryResult, DbError>,
        elapsed_ms: u128,
    ) {
        match outcome {
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
                let mut view = TableView::new(result);
                view.elapsed_ms = elapsed_ms;
                self.table = Some(view);
                self.table_sql = Some(sql);
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
            Err(e) => self.push_line(e.message(), LineKind::Err),
        }
    }

    fn show_write(&mut self, sql: &str, outcome: Result<usize, DbError>) {
        match outcome {
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
            Err(e) => self.push_line(e.message(), LineKind::Err),
        }
    }
}

/// Lock the shared database; a worker panic must not wedge the app.
fn lock_db(db: &Mutex<Database>) -> MutexGuard<'_, Database> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The char a key inserts as plain typing (no modifiers besides Shift).
fn typed_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some(ch)
        }
        _ => None,
    }
}

/// Ctrl+C clears non-empty input, arms/confirms quit on empty input,
/// and closes the grid in table mode.
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
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.handle_key(ctrl_c());
        assert!(!app.should_quit);
        assert!(app.is_quit_armed());
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_timeout_resets() {
        use std::time::Duration;
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
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
    fn ctrl_c_clears_nonempty_input_instead_of_quitting() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        type_text(&mut app, "select 1;");
        app.handle_key(ctrl_c());
        assert_eq!(app.input.full_text(), "", "input cleared");
        assert!(!app.should_quit, "must not quit with non-empty input");
        assert!(!app.is_quit_armed(), "clear disarms a pending quit");
        // Now empty: two presses quit as usual.
        app.handle_key(ctrl_c());
        assert!(!app.should_quit);
        assert!(app.is_quit_armed());
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_clears_all_lines_of_multiline_input() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        // Simulate an unterminated draft (e.g. unbalanced quote swallowing `;`).
        app.input
            .set_text("select * from users\nwhere name = 'oops");
        assert_eq!(app.input.lines().len(), 2);
        app.handle_key(ctrl_c());
        assert!(app.input.full_text().is_empty());
        assert_eq!(app.input.lines().len(), 1);
        assert!(!app.should_quit);
        assert!(!app.is_quit_armed());
    }

    #[test]
    fn ctrl_c_on_blank_input_arms_quit() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.input.set_text("  \n ");
        assert!(app.input.is_blank());
        app.handle_key(ctrl_c());
        assert!(app.is_quit_armed());
        assert!(!app.should_quit);
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
    }

    #[test]
    fn dot_clear_empties_scrollback_and_resets_scroll() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.push_line("# select 1;", LineKind::Echo);
        app.push_line("Error: boom", LineKind::Err);
        app.scroll_output_up(10);
        assert_eq!(app.scrollback.len(), 2);
        app.input.set_text(".clear");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.scrollback.is_empty(), "all output cleared");
        assert_eq!(app.scroll_offset, 0);
        assert!(app.input.full_text().is_empty());
        assert!(
            app.history.entries().contains(&".clear".to_string()),
            "history kept"
        );
    }

    #[test]
    fn ctrl_c_from_table_goes_back_without_quitting() {
        use crate::db::QueryResult;
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec!["1".to_string()]],
            truncated: false,
            kinds: Vec::new(),
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
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.handle_key(ctrl_c());
        assert!(app.is_quit_armed());
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec!["1".to_string()]],
            truncated: false,
            kinds: Vec::new(),
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
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
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
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.history.push("line1\nline2".to_string());
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.full_text(), "line1\nline2");
        assert_eq!((app.input.row, app.input.col), (1, 5));
    }

    #[test]
    fn scrollback_is_capped() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        for i in 0..SCROLLBACK_LIMIT + 50 {
            app.push_line(format!("line {i}"), LineKind::Echo);
        }
        assert_eq!(app.scrollback.len(), SCROLLBACK_LIMIT);
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn shift_up_down_scrolls_output_without_touching_input() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.history.push("select 1;".to_string());
        app.handle_key(shift_key(KeyCode::Up));
        assert_eq!(app.scroll_offset, crate::config::OUTPUT_SCROLL_LINE);
        assert!(app.input.full_text().is_empty(), "input untouched");
        assert!(!app.history.browsing(), "history untouched");
        app.handle_key(shift_key(KeyCode::Down));
        assert_eq!(app.scroll_offset, 0);
        assert!(app.input.full_text().is_empty());
    }

    #[test]
    fn pgup_pgdn_scroll_output_page() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, crate::config::OUTPUT_SCROLL_PAGE);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn new_output_resets_manual_scroll_to_follow() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.scroll_output_up(25);
        assert_eq!(app.scroll_offset, 25);
        app.push_line("latest", LineKind::Ok);
        assert_eq!(
            app.scroll_offset, 0,
            "live follow regardless of manual scroll"
        );
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn completion_popup_closes_when_leaving_tab_loop() {
        for exit in [
            KeyCode::Char('x'),
            KeyCode::Backspace,
            KeyCode::Esc,
            KeyCode::PageUp,
        ] {
            let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
            type_text(&mut app, "se");
            app.handle_key(key(KeyCode::Tab));
            assert!(app.completer.popup().is_some(), "popup open after TAB");
            app.handle_key(key(exit));
            assert!(app.completer.popup().is_none(), "popup closed by {exit:?}");
        }
    }

    #[test]
    fn table_shift_w_toggles_full_wrap() {
        use crate::db::QueryResult;
        use crate::table_view::WrapMode;
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec!["1".to_string()]],
            truncated: false,
            kinds: Vec::new(),
        }));
        app.mode = Mode::Table;
        let wrap = |app: &App| app.table.as_ref().expect("table").wrap;
        let mut shift_w = key(KeyCode::Char('W'));
        shift_w.modifiers = KeyModifiers::SHIFT;
        app.handle_key(shift_w);
        assert_eq!(wrap(&app), WrapMode::Full);
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(wrap(&app), WrapMode::Off);
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(wrap(&app), WrapMode::Capped);
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

    /// Press a key that may start a statement and wait until it finishes.
    fn run_key(app: &mut App, code: KeyCode) {
        app.handle_key(key(code));
        app.wait_query(Duration::from_secs(30));
        assert!(!app.is_busy(), "statement still running");
    }

    /// Recursive CTE that keeps SQLite busy for a noticeable while.
    const SLOW_SQL: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                            WHERE x < 20000000) SELECT count(*) AS n FROM c;";

    #[test]
    fn slow_statement_runs_in_background_and_ignores_keys() {
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        type_text(&mut app, SLOW_SQL);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_busy(), "slow statement outlives the spinner delay");
        assert!(app.input.is_blank(), "prompt cleared while running");
        let (label, _) = app.busy_status().expect("busy status");
        assert_eq!(label, "Running query");
        assert!(!app.poll_query(), "not finished yet");

        type_text(&mut app, "x");
        app.handle_key(ctrl_c());
        assert!(app.input.is_blank(), "typing ignored while busy");
        assert!(!app.is_quit_armed(), "Ctrl+C ignored while busy");

        app.wait_query(Duration::from_secs(60));
        assert!(!app.is_busy());
        assert_eq!(app.mode, Mode::Table);
        let t = app.table.as_ref().expect("table");
        assert_eq!(t.rows[0][0], "20000000");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ctrl_d_quits_while_busy() {
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        type_text(&mut app, SLOW_SQL);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_busy());
        app.handle_key(mod_key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
        app.wait_query(Duration::from_secs(60));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn end_to_end_select_table_dot_write_error_history() {
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        app.refresh_schema();
        assert!(app.schema.tables.contains(&"users".to_string()));

        // SELECT -> grid mode with 2 rows.
        type_text(&mut app, "select * from users;");
        run_key(&mut app, KeyCode::Enter);
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
        run_key(&mut app, KeyCode::Enter);
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.text == "Affected 1 row" && l.kind == LineKind::Ok)
        );

        // Bad SQL -> light-red error line, app stays alive.
        type_text(&mut app, "select * from nope;");
        run_key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Input);
        assert!(
            app.scrollback
                .iter()
                .any(|l| l.kind == LineKind::Err && l.text.starts_with("Error:"))
        );

        // History: Up restores the last command whole, caret at end.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.full_text(), "select * from nope;");

        // Ctrl+C with non-empty input clears it instead of quitting;
        // two more presses on the now-empty input quit (arm + confirm).
        app.handle_key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert!(!app.should_quit);
        assert!(!app.is_quit_armed());
        assert!(app.input.full_text().is_empty());
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

    fn mod_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn mac_style_word_and_line_shortcuts() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        type_text(&mut app, "select name from users");
        app.handle_key(mod_key(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.input.col, 17);
        app.handle_key(mod_key(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.input.col, 12);
        app.handle_key(mod_key(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.input.col, 16);
        app.handle_key(mod_key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.input.col, 0);
        app.handle_key(mod_key(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(app.input.col, 22);
        app.handle_key(mod_key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(app.input.full_text(), "select name from ");
        app.handle_key(mod_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.input.full_text(), "");
    }

    #[test]
    fn undo_redo_keys_restore_killed_line() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        type_text(&mut app, "select name");
        app.handle_key(mod_key(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(app.input.full_text(), "");
        app.handle_key(mod_key(KeyCode::Char('z'), KeyModifiers::SUPER));
        assert_eq!(app.input.full_text(), "select name");
        app.handle_key(mod_key(
            KeyCode::Char('z'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        ));
        assert_eq!(app.input.full_text(), "");
        app.handle_key(mod_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.input.full_text(), "select name");
        app.handle_key(mod_key(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(app.input.full_text(), "");
        app.handle_key(mod_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        app.handle_key(mod_key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(app.input.full_text(), "");
    }

    #[test]
    fn undo_brings_back_draft_cleared_by_ctrl_c() {
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        type_text(&mut app, "select 1");
        app.handle_key(ctrl_c());
        assert!(app.input.is_blank());
        app.handle_key(mod_key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.input.full_text(), "select 1");
    }

    #[test]
    fn table_vim_keys_scroll() {
        use crate::db::QueryResult;
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        app.table = Some(TableView::new(QueryResult {
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["1".to_string(); 2], vec!["2".to_string(); 2]],
            truncated: false,
            kinds: Vec::new(),
        }));
        app.mode = Mode::Table;
        let pos = |app: &App| {
            let t = app.table.as_ref().expect("table");
            (t.offset_y, t.offset_x)
        };
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(pos(&app), (1, 1));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(pos(&app), (0, 0));
    }

    #[test]
    fn table_refresh_reruns_query() {
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        type_text(&mut app, "select * from users;");
        run_key(&mut app, KeyCode::Enter);
        assert_eq!(app.table.as_ref().expect("table").row_count(), 2);
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.execute("INSERT INTO users (name) VALUES ('zed')", [])
            .expect("insert");
        run_key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.table.as_ref().expect("table").row_count(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shift_j_and_t_switch_views_and_refresh_keeps_json() {
        use crate::table_view::ViewMode;
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        type_text(&mut app, "select * from users;");
        run_key(&mut app, KeyCode::Enter);
        let view = |app: &App| app.table.as_ref().expect("table").view;
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(view(&app), ViewMode::Table, "lowercase j only scrolls");
        assert_eq!(app.table.as_ref().expect("table").offset_y, 1);
        app.handle_key(shift_key(KeyCode::Char('J')));
        assert_eq!(view(&app), ViewMode::Json);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.table.as_ref().expect("table").json_offset, 1);
        run_key(&mut app, KeyCode::Char('r'));
        assert_eq!(view(&app), ViewMode::Json, "refresh keeps the JSON view");
        assert!(
            app.table
                .as_ref()
                .expect("table")
                .json_lines
                .iter()
                .any(|l| l.contains("\"name\": \"greg\""))
        );
        app.handle_key(shift_key(KeyCode::Char('T')));
        assert_eq!(view(&app), ViewMode::Table);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Input);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multiline_statement_executes_on_terminating_semi() {
        let path = seed_e2e_db();
        let mut app = App::new(Database::Sqlite(path.clone()));
        app.refresh_schema();
        type_text(&mut app, "update users set name = 'greg2'");
        app.handle_key(key(KeyCode::Enter)); // no `;` -> newline, no execution
        assert_eq!(app.mode, Mode::Input);
        assert_eq!(app.input.lines().len(), 2);
        type_text(&mut app, "where id = 1;");
        run_key(&mut app, KeyCode::Enter);
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
        let mut first = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        first.history_file = Some(history_path.clone());
        first.load_history();
        assert!(first.history.is_empty());
        first.push_history("select 1;".to_string());
        first.push_history("line1\nline2;".to_string());
        assert!(history_path.is_file());

        // Second session loads what the first one stored.
        let mut second = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
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
        let mut app = App::new(Database::Sqlite(PathBuf::from("dummy.db")));
        assert!(app.history_file.is_none());
        app.push_history("select 1;".to_string());
        assert_eq!(app.history.entries(), &["select 1;".to_string()]);
    }
}
