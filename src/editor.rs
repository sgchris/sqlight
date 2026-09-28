//! Multiline input buffer, command history, and TAB completion.
//! All pure logic (no Ratatui IO) so it can be unit-tested.

use std::collections::HashMap;

use crate::config::{COMPLETE_MIN_CHARS, HISTORY_LIMIT};

/// SQL keywords offered by autocompletion (uppercase by convention).
pub const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "INDEX",
    "DROP",
    "ALTER",
    "ADD",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "ON",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "ORDER",
    "BY",
    "GROUP",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "SET",
    "PRAGMA",
    "EXPLAIN",
    "WITH",
    "UNION",
    "ALL",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "CHECK",
    "UNIQUE",
    "VIEW",
    "TRIGGER",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "VACUUM",
    "REINDEX",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "IN",
    "IS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
];

/// Multiline text with a `char`-based cursor (never byte indices).
#[derive(Debug, Clone, Default)]
pub struct InputBuffer {
    lines: Vec<String>,
    /// Cursor row (line index).
    pub row: usize,
    /// Cursor col (char index within the line).
    pub col: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Full buffer text with `\n` separators.
    pub fn full_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_blank(&self) -> bool {
        self.full_text().trim().is_empty()
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    /// Replace entire contents (used for history recall); caret to end.
    pub fn set_text(&mut self, text: &str) {
        if text.is_empty() {
            self.clear();
        } else {
            self.lines = text.split('\n').map(ToString::to_string).collect();
            self.move_to_end();
        }
    }

    pub fn move_to_end(&mut self) {
        self.row = self.line_count().saturating_sub(1);
        self.col = self.lines[self.row].chars().count();
    }

    fn char_count(&self, row: usize) -> usize {
        self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
    }

    fn clamp_col(&mut self) {
        let max = self.char_count(self.row);
        self.col = self.col.min(max);
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.insert_newline();
            return;
        }
        let col = self.col;
        let line = &mut self.lines[self.row];
        let byte: usize = line.chars().take(col).map(|c| c.len_utf8()).sum();
        line.insert(byte, ch);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        let col = self.col;
        let byte: usize = self.lines[self.row]
            .chars()
            .take(col)
            .map(|c| c.len_utf8())
            .sum();
        let tail = self.lines[self.row][byte..].to_string();
        self.lines[self.row].truncate(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// Delete char before cursor (Backspace). Returns true if anything changed.
    pub fn backspace(&mut self) -> bool {
        if self.col > 0 {
            let col = self.col - 1;
            let line = &mut self.lines[self.row];
            let start: usize = line.chars().take(col).map(|c| c.len_utf8()).sum();
            let end = start
                + line[start..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
            line.drain(start..end);
            self.col = col;
            true
        } else if self.row > 0 {
            // Join with previous line.
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.char_count(self.row);
            self.lines[self.row].push_str(&tail);
            true
        } else {
            false
        }
    }

    /// Delete char under cursor (Delete). Returns true if anything changed.
    pub fn delete_forward(&mut self) -> bool {
        let max = self.char_count(self.row);
        if self.col < max {
            let line = &mut self.lines[self.row];
            let start: usize = line.chars().take(self.col).map(|c| c.len_utf8()).sum();
            let end = start
                + line[start..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
            line.drain(start..end);
            true
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
            true
        } else {
            false
        }
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.char_count(self.row);
        }
    }

    pub fn move_right(&mut self) {
        if self.col < self.char_count(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_line_start(&mut self) {
        self.col = 0;
    }

    pub fn move_line_end(&mut self) {
        self.col = self.char_count(self.row);
    }

    /// Char column of the previous word start (readline `M-b`) on this line.
    fn prev_word_col(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col.min(chars.len());
        while i > 0 && !is_nav_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_nav_word_char(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    /// Char column just past the next word end (readline `M-f`) on this line.
    fn next_word_col(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col.min(chars.len());
        while i < chars.len() && !is_nav_word_char(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_nav_word_char(chars[i]) {
            i += 1;
        }
        i
    }

    /// Move to the previous word start; at line start, wrap to previous line end.
    pub fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
        } else {
            self.col = self.prev_word_col();
        }
    }

    /// Move past the next word end; at line end, wrap to next line start.
    pub fn move_word_right(&mut self) {
        if self.col >= self.char_count(self.row) {
            self.move_right();
        } else {
            self.col = self.next_word_col();
        }
    }

    /// Remove chars `[from, to)` on the current line and park the cursor at `from`.
    fn delete_range(&mut self, from: usize, to: usize) {
        let line = &mut self.lines[self.row];
        let chars: Vec<char> = line.chars().collect();
        let to = to.min(chars.len());
        *line = chars[..from].iter().chain(&chars[to..]).collect();
        self.col = from;
    }

    /// Delete the word before the cursor (Option/Alt+Backspace, Ctrl+W).
    pub fn delete_word_before(&mut self) -> bool {
        if self.col == 0 {
            return self.backspace();
        }
        let from = self.prev_word_col();
        self.delete_range(from, self.col);
        true
    }

    /// Delete the word after the cursor (Option/Alt+Delete, Alt+D).
    pub fn delete_word_after(&mut self) -> bool {
        if self.col >= self.char_count(self.row) {
            return self.delete_forward();
        }
        let to = self.next_word_col();
        self.delete_range(self.col, to);
        true
    }

    /// Delete from line start to the cursor (Cmd+Backspace, Ctrl+U).
    pub fn delete_to_line_start(&mut self) -> bool {
        if self.col == 0 {
            return self.backspace();
        }
        self.delete_range(0, self.col);
        true
    }

    /// Delete from the cursor to line end (Cmd+Delete, Ctrl+K).
    pub fn delete_to_line_end(&mut self) -> bool {
        let max = self.char_count(self.row);
        if self.col >= max {
            return self.delete_forward();
        }
        self.delete_range(self.col, max);
        true
    }

    /// Returns false when already at the first line (caller may use history).
    pub fn move_up(&mut self) -> bool {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
            true
        } else {
            false
        }
    }

    /// Returns false when already at the last line (caller may use history).
    pub fn move_down(&mut self) -> bool {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
            true
        } else {
            false
        }
    }

    /// Word fragment directly before the cursor on the current line.
    /// Returns (word, start_char_col). Word chars: alnum + `_`, `.`, `"`.
    pub fn word_before_cursor(&self) -> (String, usize) {
        let line = &self.lines[self.row];
        let chars: Vec<char> = line.chars().collect();
        let mut start = self.col.min(chars.len());
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        (
            chars[start..self.col.min(chars.len())].iter().collect(),
            start,
        )
    }

    /// Replace the word before the cursor with `replacement`.
    pub fn replace_word_before_cursor(&mut self, replacement: &str) {
        let (_, start) = self.word_before_cursor();
        let line = &mut self.lines[self.row];
        let chars: Vec<char> = line.chars().collect();
        let end = self.col.min(chars.len());
        let new_line: String = chars[..start].iter().collect::<String>()
            + replacement
            + &chars[end..].iter().collect::<String>();
        *line = new_line;
        self.col = start + replacement.chars().count();
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '"'
}

/// Word chars for cursor movement / word deletion (stops at `.` and quotes).
fn is_nav_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Known tables mentioned anywhere in `query` (FROM, JOIN, UNION, ...),
/// found by matching word tokens against `known` case-insensitively,
/// in full (`public.users`) or by the part after the last dot. Tokens
/// equal to `skip` (the word being completed) are ignored.
pub fn tables_in_query(query: &str, known: &[String], skip: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let tokens = query
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '"' | '`' | '[' | ']')))
        .map(|t| t.replace(['"', '`', '[', ']'], ""))
        .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(skip));
    for tok in tokens {
        let last = tok.rsplit('.').next().unwrap_or(&tok);
        for t in known {
            let hit = t.eq_ignore_ascii_case(&tok) || t.eq_ignore_ascii_case(last);
            if hit && !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

/// Up/Down command history. Multiline entries stored whole.
#[derive(Debug, Clone, Default)]
pub struct History {
    entries: Vec<String>,
    /// None = live editing; Some(i) = browsing entry i.
    cursor: Option<usize>,
    /// Unsent draft preserved while browsing.
    draft: String,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrow all remembered entries, oldest first (for persistence).
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn push(&mut self, entry: String) {
        if entry.trim().is_empty() {
            return;
        }
        // Avoid consecutive duplicates.
        if self.entries.last().is_some_and(|last| last == &entry) {
            self.cursor = None;
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > HISTORY_LIMIT {
            let overflow = self.entries.len() - HISTORY_LIMIT;
            self.entries.drain(..overflow);
        }
        self.cursor = None;
        self.draft.clear();
    }

    /// Move older (Up). Returns the entry to display, if any.
    pub fn move_up(&mut self, current: &str) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        match self.cursor {
            None => {
                self.draft = current.to_string();
                self.cursor = Some(self.entries.len() - 1);
                self.entries.last().cloned()
            }
            Some(0) => self.entries.first().cloned(),
            Some(i) => {
                self.cursor = Some(i - 1);
                self.entries.get(i - 1).cloned()
            }
        }
    }

    /// Move newer (Down). Returns text to display (draft when back at live).
    pub fn move_down(&mut self) -> Option<String> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.cursor = None;
                Some(self.draft.clone())
            }
            Some(i) => {
                self.cursor = Some(i + 1);
                self.entries.get(i + 1).cloned()
            }
        }
    }

    pub fn browsing(&self) -> bool {
        self.cursor.is_some()
    }
}

/// TAB completion over keywords + schema names with cycling.
#[derive(Debug, Clone, Default)]
pub struct Completer {
    tables: Vec<String>,
    columns: Vec<String>,
    /// Lowercased table name -> its columns (scopes column candidates).
    table_columns: HashMap<String, Vec<String>>,
    active_prefix: Option<String>,
    candidates: Vec<String>,
    index: usize,
}

impl Completer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_schema(
        &mut self,
        tables: Vec<String>,
        columns: Vec<String>,
        table_columns: HashMap<String, Vec<String>>,
    ) {
        self.tables = tables;
        self.columns = columns;
        self.table_columns = table_columns;
        self.dismiss();
    }

    pub fn dismiss(&mut self) {
        self.active_prefix = None;
        self.candidates.clear();
        self.index = 0;
    }

    /// Items of the active session (typed word first) and the selected
    /// index, or `None` when no TAB session is running.
    pub fn popup(&self) -> Option<(&[String], usize)> {
        self.active_prefix
            .as_ref()
            .map(|_| (self.candidates.as_slice(), self.index))
    }

    /// Advance one completion step. Returns the replacement word, if any.
    /// `shift` cycles backwards (Shift+Tab).
    ///
    /// TAB cycles through all matches for the originally typed prefix,
    /// and back to the prefix itself:
    /// `te` -> `text_id` -> `texts` -> `te` -> `text_id` ... The session continues
    /// while the current word is the original prefix or one of the
    /// previously offered candidates (i.e. a word TAB itself inserted).
    /// Any other word starts a fresh session.
    ///
    /// `query` is the whole input buffer: column candidates are limited to
    /// the tables it mentions (all columns when it mentions none).
    pub fn complete(&mut self, word: &str, shift: bool, query: &str) -> Option<String> {
        if word.chars().count() < COMPLETE_MIN_CHARS {
            self.dismiss();
            return None;
        }
        let same_session = match &self.active_prefix {
            None => false,
            Some(prefix) => {
                word.eq_ignore_ascii_case(prefix)
                    || self.candidates.iter().any(|c| c.eq_ignore_ascii_case(word))
            }
        };
        if !same_session {
            let matches: Vec<String> = self
                .build_candidates(word, query)
                .into_iter()
                .filter(|m| m != word)
                .collect();
            if matches.is_empty() {
                self.dismiss();
                return None;
            }
            self.active_prefix = Some(word.to_string());
            self.candidates = std::iter::once(word.to_string()).chain(matches).collect();
            self.index = if shift { self.candidates.len() - 1 } else { 1 };
        } else if shift {
            self.index = self
                .index
                .checked_sub(1)
                .unwrap_or(self.candidates.len() - 1);
        } else {
            self.index = (self.index + 1) % self.candidates.len();
        }
        self.candidates.get(self.index).cloned()
    }

    fn build_candidates(&self, prefix: &str, query: &str) -> Vec<String> {
        let mentioned = tables_in_query(query, &self.tables, prefix);
        let scoped: Vec<&String> = mentioned
            .iter()
            .filter_map(|t| self.table_columns.get(&t.to_lowercase()))
            .flatten()
            .collect();
        let columns: Vec<&String> = if mentioned.is_empty() {
            self.columns.iter().collect()
        } else {
            scoped
        };
        // If prefix contains a dot, complete the part after the last dot.
        let sub = prefix.rsplit('.').next().unwrap_or(prefix);
        let mut out: Vec<String> = Vec::new();
        let mut push_unique = |s: &str| {
            if !out.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                out.push(s.to_string());
            }
        };
        for kw in SQL_KEYWORDS {
            if kw.len() >= COMPLETE_MIN_CHARS && kw.starts_with(&sub.to_ascii_uppercase()) {
                push_unique(kw);
            }
        }
        for t in &self.tables {
            if t.len() >= COMPLETE_MIN_CHARS && t.to_lowercase().starts_with(&sub.to_lowercase()) {
                push_unique(t);
            }
        }
        for c in columns {
            if c.len() >= COMPLETE_MIN_CHARS && c.to_lowercase().starts_with(&sub.to_lowercase()) {
                push_unique(c);
            }
        }
        // Dot-commands when completing from line start.
        for dot in [".tables", ".schema", ".clear"] {
            if dot.starts_with(&prefix.to_lowercase()) && prefix.starts_with('.') {
                push_unique(dot);
            }
        }
        out.sort_by_key(|s| s.to_lowercase());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_insert_and_newline() {
        let mut b = InputBuffer::new();
        for c in "select 1".chars() {
            b.insert_char(c);
        }
        b.insert_newline();
        for c in "from t".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.full_text(), "select 1\nfrom t");
        assert_eq!(b.line_count(), 2);
    }

    #[test]
    fn buffer_unicode_cursor() {
        let mut b = InputBuffer::new();
        for c in "héllo".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.col, 5);
        b.move_left();
        b.backspace();
        assert_eq!(b.full_text(), "hélo");
    }

    #[test]
    fn buffer_backspace_joins_lines() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd");
        b.row = 1;
        b.col = 0;
        assert!(b.backspace());
        assert_eq!(b.full_text(), "abcd");
    }

    #[test]
    fn history_push_navigate_caret_to_end() {
        let mut h = History::new();
        h.push("select 1;".to_string());
        h.push("line1\nline2;".to_string());
        let mut b = InputBuffer::new();
        let up = h.move_up(&b.full_text()).unwrap();
        b.set_text(&up);
        assert_eq!(b.full_text(), "line1\nline2;");
        // Caret forced to end of whole entry.
        assert_eq!((b.row, b.col), (1, 6));
        let up2 = h.move_up(&b.full_text()).unwrap();
        assert_eq!(up2, "select 1;");
        let down = h.move_down().unwrap();
        assert_eq!(down, "line1\nline2;");
    }

    #[test]
    fn completer_gate_and_cycle() {
        let mut c = Completer::new();
        c.set_schema(
            vec!["employees".to_string()],
            vec!["age".to_string()],
            HashMap::new(),
        );
        assert_eq!(c.complete("u", false, ""), None);
        // "em" matches only the table (no keyword starts with EM).
        assert_eq!(c.complete("em", false, ""), Some("employees".to_string()));
        // "se" matches SELECT + SET — repeated TAB cycles.
        let a = c.complete("se", false, "").unwrap();
        let b = c.complete("se", false, "").unwrap();
        assert_ne!(a, b);
        assert!(["SELECT", "SET"].contains(&a.as_str()));
        assert!(["SELECT", "SET"].contains(&b.as_str()));
        // Third TAB returns to the typed word, fourth wraps to the first match.
        assert_eq!(c.complete(&b, false, ""), Some("se".to_string()));
        assert_eq!(c.complete("se", false, ""), Some(a));
    }

    #[test]
    fn completer_popup_lists_prefix_first() {
        let mut c = Completer::new();
        c.set_schema(
            vec!["table_a".to_string(), "tariffs".to_string()],
            vec![],
            HashMap::new(),
        );
        assert!(c.popup().is_none());
        let first = c.complete("ta", false, "").unwrap();
        let (items, sel) = c.popup().unwrap();
        assert_eq!(items, ["ta", "TABLE", "table_a", "tariffs"]);
        assert_eq!(sel, 1);
        assert_eq!(first, "TABLE");
        // Shift+TAB from the first match goes back to the typed word.
        assert_eq!(c.complete(&first, true, ""), Some("ta".to_string()));
        assert_eq!(c.popup().unwrap().1, 0);
        c.dismiss();
        assert!(c.popup().is_none());
    }

    #[test]
    fn completer_cycles_through_replaced_word() {
        let mut c = Completer::new();
        c.set_schema(
            vec!["text_id".to_string(), "texts".to_string()],
            vec![],
            HashMap::new(),
        );
        // First TAB completes "te" to the first match.
        let first = c.complete("te", false, "").unwrap();
        assert_eq!(first, "text_id");
        // Second TAB sees the already-completed word and advances
        // to the next match instead of restarting the session.
        let second = c.complete(&first, false, "").unwrap();
        assert_eq!(second, "texts");
        // Third TAB restores the typed word, fourth wraps around.
        assert_eq!(c.complete(&second, false, ""), Some("te".to_string()));
        let fourth = c.complete("te", false, "").unwrap();
        assert_eq!(fourth, "text_id");
        // Shift+TAB cycles backwards.
        let back = c.complete(&fourth, true, "").unwrap();
        assert_eq!(back, "te");
        // An unrelated word restarts the session (no match -> None).
        assert_eq!(c.complete("zz", false, ""), None);
        // Original prefix starts a fresh session again.
        assert_eq!(c.complete("te", false, ""), Some("text_id".to_string()));
    }

    #[test]
    fn word_before_cursor() {
        let mut b = InputBuffer::new();
        b.set_text("select us");
        let (w, _) = b.word_before_cursor();
        assert_eq!(w, "us");
        b.replace_word_before_cursor("users");
        assert_eq!(b.full_text(), "select users");
    }

    #[test]
    fn word_navigation_and_deletion() {
        let mut b = InputBuffer::new();
        b.set_text("select  u.name from");
        b.move_word_left();
        assert_eq!(b.col, 15);
        b.move_word_left();
        assert_eq!(b.col, 10);
        b.move_word_left();
        assert_eq!(b.col, 8);
        b.move_word_right();
        assert_eq!(b.col, 9);
        b.move_line_start();
        b.move_word_right();
        assert_eq!(b.col, 6);
        b.move_line_end();
        assert!(b.delete_word_before());
        assert_eq!(b.full_text(), "select  u.name ");
        b.move_line_start();
        assert!(b.delete_word_after());
        assert_eq!(b.full_text(), "  u.name ");
    }

    #[test]
    fn word_moves_cross_line_boundaries() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd");
        b.row = 1;
        b.col = 0;
        b.move_word_left();
        assert_eq!((b.row, b.col), (0, 2));
        b.move_word_right();
        assert_eq!((b.row, b.col), (1, 0));
    }

    #[test]
    fn line_kill_commands() {
        let mut b = InputBuffer::new();
        b.set_text("héllo wörld");
        b.col = 6;
        assert!(b.delete_to_line_start());
        assert_eq!((b.full_text().as_str(), b.col), ("wörld", 0));
        b.col = 2;
        assert!(b.delete_to_line_end());
        assert_eq!(b.full_text(), "wö");
    }

    fn scoped_completer() -> Completer {
        let cols = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        let mut map = HashMap::new();
        map.insert("my_table".to_string(), cols(&["user_id", "user_content"]));
        map.insert("credits".to_string(), cols(&["user_credits_available"]));
        let mut c = Completer::new();
        c.set_schema(
            cols(&["my_table", "credits"]),
            cols(&["user_id", "user_content", "user_credits_available"]),
            map,
        );
        c
    }

    fn cycle(c: &mut Completer, word: &str, query: &str) -> Vec<String> {
        let first = c.complete(word, false, query).unwrap();
        let mut out = vec![first];
        loop {
            let next = c.complete(out.last().unwrap(), false, query).unwrap();
            if next == word {
                return out;
            }
            out.push(next);
        }
    }

    #[test]
    fn completion_scoped_to_query_tables() {
        let mut c = scoped_completer();
        let q = "select * from my_table where user_";
        assert_eq!(cycle(&mut c, "user_", q), vec!["user_content", "user_id"]);
    }

    #[test]
    fn completion_unions_all_mentioned_tables() {
        let mut c = scoped_completer();
        let q = "select * from my_table m join credits c on c.x = m.y where user_";
        assert_eq!(cycle(&mut c, "user_", q).len(), 3);
        c.dismiss();
        let q = "select user_id from my_table union select 1 from \"CREDITS\" where user_";
        assert_eq!(cycle(&mut c, "user_", q).len(), 3);
    }

    #[test]
    fn completion_falls_back_to_all_columns() {
        let mut c = scoped_completer();
        assert_eq!(cycle(&mut c, "user_", "select user_").len(), 3);
    }

    #[test]
    fn tables_in_query_quoted_and_qualified() {
        let known = vec!["users".to_string(), "sales.orders".to_string()];
        let found = tables_in_query("select * from main.\"Users\", sales.orders", &known, "");
        assert_eq!(found, vec!["users".to_string(), "sales.orders".to_string()]);
        assert!(tables_in_query("select * from users", &known, "users").is_empty());
    }

    #[test]
    fn completer_suggests_clear_dot_command() {
        let mut c = Completer::new();
        c.set_schema(vec![], vec![], HashMap::new());
        assert_eq!(c.complete(".cl", false, ""), Some(".clear".to_string()));
    }
}
