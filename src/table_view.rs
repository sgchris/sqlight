//! Results-grid state: column widths, truncation (`...`), wrapping
//! (max 8 lines), and scroll offsets. Rendering lives in `ui.rs`.

use unicode_width::UnicodeWidthStr;

use crate::config::{MAX_COL_WIDTH, MAX_WRAP_LINES, MIN_COL_WIDTH, TRUNC_SUFFIX};
use crate::db::QueryResult;
use crate::json_view;

/// How long cell values are displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WrapMode {
    /// One truncated line per cell.
    #[default]
    Off,
    /// Wrapped, at most `MAX_WRAP_LINES` lines per cell.
    Capped,
    /// Wrapped with no line cap.
    Full,
}

impl WrapMode {
    pub fn label(self) -> &'static str {
        match self {
            WrapMode::Off => "off",
            WrapMode::Capped => "on",
            WrapMode::Full => "full",
        }
    }
}

/// How a result is shown: the grid (default) or pretty JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Table,
    Json,
}

impl ViewMode {
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::Table => "table",
            ViewMode::Json => "json",
        }
    }
}

/// Full-screen grid state for one SELECT result.
#[derive(Debug, Clone)]
pub struct TableView {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
    /// Width each column needs to show every value untruncated
    /// (screen fitting happens per frame via `fit_widths`).
    pub natural_widths: Vec<usize>,
    pub wrap: WrapMode,
    /// First visible data row.
    pub offset_y: usize,
    /// First visible column.
    pub offset_x: usize,
    pub view: ViewMode,
    /// Pretty JSON of the whole result, one entry per logical line.
    pub json_lines: Vec<String>,
    /// First visible JSON line; clamped against the wrapped height at
    /// render time, like the scrollback.
    pub json_offset: usize,
}

impl TableView {
    pub fn new(result: QueryResult) -> Self {
        let natural_widths = natural_widths(&result.headers, &result.rows);
        let json_lines = json_view::pretty_rows(&result.headers, &result.rows, &result.kinds);
        Self {
            headers: result.headers,
            rows: result.rows,
            truncated: result.truncated,
            natural_widths,
            wrap: WrapMode::Off,
            offset_y: 0,
            offset_x: 0,
            view: ViewMode::Table,
            json_lines,
            json_offset: 0,
        }
    }

    pub fn show_json(&mut self) {
        self.view = ViewMode::Json;
    }

    pub fn show_table(&mut self) {
        self.view = ViewMode::Table;
    }

    pub fn is_json(&self) -> bool {
        self.view == ViewMode::Json
    }

    pub fn json_scroll_up(&mut self, n: usize) {
        self.json_offset = self.json_offset.saturating_sub(n);
    }

    /// Unclamped here; the renderer caps it at the last screenful.
    pub fn json_scroll_down(&mut self, n: usize) {
        self.json_offset = self.json_offset.saturating_add(n);
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn col_count(&self) -> usize {
        self.headers.len()
    }

    /// `w`: Off <-> Capped; Full goes back to Off.
    pub fn toggle_wrap(&mut self) {
        self.wrap = match self.wrap {
            WrapMode::Off => WrapMode::Capped,
            WrapMode::Capped | WrapMode::Full => WrapMode::Off,
        };
    }

    /// `W`: Off/Capped -> Full; Full goes back to Off.
    pub fn toggle_full_wrap(&mut self) {
        self.wrap = match self.wrap {
            WrapMode::Full => WrapMode::Off,
            WrapMode::Off | WrapMode::Capped => WrapMode::Full,
        };
    }

    pub fn is_wrapped(&self) -> bool {
        self.wrap != WrapMode::Off
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.offset_y = self.offset_y.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        if !self.rows.is_empty() {
            self.offset_y = (self.offset_y + n).min(self.rows.len() - 1);
        }
    }

    pub fn scroll_left(&mut self, n: usize) {
        self.offset_x = self.offset_x.saturating_sub(n);
    }

    pub fn scroll_right(&mut self, n: usize) {
        if !self.headers.is_empty() {
            self.offset_x = (self.offset_x + n).min(self.headers.len() - 1);
        }
    }

    /// Rendered lines for one cell (wrapped or single truncated line)
    /// at the given on-screen column `width`.
    pub fn cell_lines(&self, row: usize, col: usize, width: usize) -> Vec<String> {
        let raw = self
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(String::as_str)
            .unwrap_or("");
        match self.wrap {
            WrapMode::Off => vec![truncate_text(raw, width)],
            WrapMode::Capped => wrap_text(raw, width, MAX_WRAP_LINES),
            WrapMode::Full => wrap_text(raw, width, usize::MAX),
        }
    }

    /// Height (in terminal rows) of a data row under the current mode.
    pub fn row_height(&self, row: usize, widths: &[usize]) -> usize {
        if !self.is_wrapped() {
            return 1;
        }
        (0..self.col_count())
            .map(|c| {
                let w = widths.get(c).copied().unwrap_or(MIN_COL_WIDTH);
                self.cell_lines(row, c, w).len().max(1)
            })
            .max()
            .unwrap_or(1)
    }
}

/// Display width of the ` │ ` column separator.
pub const COL_SEP_WIDTH: usize = 3;

/// On-screen widths for `natural` columns within `available` display
/// columns. Columns start capped at `MAX_COL_WIDTH`; any leftover width
/// is shared evenly among columns that are still truncated, and a column
/// that becomes fully visible hands its unused share to the others. When
/// the capped columns already overflow, they are returned unchanged (the
/// grid scrolls horizontally).
pub fn fit_widths(natural: &[usize], available: usize) -> Vec<usize> {
    let mut widths: Vec<usize> = natural.iter().map(|&w| w.min(MAX_COL_WIDTH)).collect();
    let used = widths.iter().sum::<usize>() + COL_SEP_WIDTH * widths.len().saturating_sub(1);
    let mut budget = available.saturating_sub(used);
    while budget > 0 {
        let needy: Vec<usize> = (0..widths.len())
            .filter(|&c| widths[c] < natural[c])
            .collect();
        if needy.is_empty() {
            break;
        }
        let share = budget / needy.len();
        if share == 0 {
            for &c in needy.iter().take(budget) {
                widths[c] += 1;
            }
            break;
        }
        for c in needy {
            let add = share.min(natural[c] - widths[c]);
            widths[c] += add;
            budget -= add;
        }
    }
    widths
}

/// Width each column needs to show every value in full (at least
/// `MIN_COL_WIDTH`), using display width (Unicode-aware).
pub fn natural_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    headers
        .iter()
        .enumerate()
        .map(|(c, h)| {
            let mut w = UnicodeWidthStr::width(h.as_str()).max(MIN_COL_WIDTH);
            for row in rows {
                if let Some(cell) = row.get(c) {
                    // Only the first visual line matters for width; wrapped
                    // mode reflows within the same width.
                    let first = cell.split('\n').next().unwrap_or(cell);
                    w = w.max(UnicodeWidthStr::width(first));
                }
            }
            w
        })
        .collect()
}

/// Truncate to `width` display columns, appending `...` when cut.
/// Values that already fit are returned unchanged.
pub fn truncate_text(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    truncate_forced(s, width)
}

/// Truncate to `width` display columns, always reserving room for `...`.
/// Used when the caller already knows content was cut (e.g. wrap cap).
fn truncate_forced(s: &str, width: usize) -> String {
    let suffix_w = UnicodeWidthStr::width(TRUNC_SUFFIX);
    if width <= suffix_w {
        return TRUNC_SUFFIX.chars().take(width).collect();
    }
    let target = width - suffix_w;
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > target {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(TRUNC_SUFFIX);
    out
}

/// Greedy char-wrap into lines of at most `width` display columns,
/// capped at `max_lines` (last line gets `...` when text remains).
/// Newlines in the source force line breaks.
pub fn wrap_text(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    for paragraph in s.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0;
        for ch in paragraph.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(0)
                .max(1);
            if cur_w + cw > width {
                lines.push(cur);
                cur = String::new();
                cur_w = 0;
                if lines.len() == max_lines {
                    break;
                }
            }
            cur.push(ch);
            cur_w += cw;
        }
        lines.push(cur);
        if lines.len() >= max_lines {
            break;
        }
    }
    lines.truncate(max_lines);
    // If the source was longer than what we emitted, mark truncation.
    let emitted: usize = lines.iter().map(|l| l.chars().count()).sum();
    let total: usize = s.chars().filter(|&c| c != '\n').count();
    if total > emitted
        && let Some(last) = lines.last_mut()
    {
        *last = truncate_forced(last, width);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_widths_uncapped() {
        let headers = vec!["id".to_string(), "name".to_string()];
        let long = "a-very-long-value-that-exceeds-maximum-width";
        let rows = vec![vec!["1".to_string(), long.to_string()]];
        let w = natural_widths(&headers, &rows);
        assert_eq!(w, vec![MIN_COL_WIDTH, long.len()]);
    }

    #[test]
    fn fit_uses_natural_widths_when_they_fit() {
        assert_eq!(fit_widths(&[8, 45], 100), vec![8, 45]);
    }

    #[test]
    fn fit_shares_free_space_between_long_columns() {
        // Capped: 8 + 30 + 30 + 2*3 = 74; 26 left for two truncated columns.
        assert_eq!(fit_widths(&[8, 100, 100], 100), vec![8, 43, 43]);
        // A column satisfied early hands its surplus to the other one.
        assert_eq!(fit_widths(&[8, 35, 100], 100), vec![8, 35, 51]);
    }

    #[test]
    fn fit_keeps_capped_widths_when_overflowing() {
        assert_eq!(fit_widths(&[50, 50, 50], 60), vec![30, 30, 30]);
    }

    #[test]
    fn truncate_adds_dots() {
        assert_eq!(truncate_text("hello world", 20), "hello world");
        assert_eq!(truncate_text("hello world", 8), "hello...");
        assert!(UnicodeWidthStr::width(truncate_text("hello world", 8).as_str()) <= 8);
    }

    #[test]
    fn truncate_wide_chars() {
        let s = "héllo wörld";
        let t = truncate_text(s, 8);
        assert!(UnicodeWidthStr::width(t.as_str()) <= 8);
        assert!(t.ends_with(TRUNC_SUFFIX));
    }

    #[test]
    fn wrap_respects_width_and_cap() {
        let lines = wrap_text("abcdefghij", 4, 8);
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
        let capped = wrap_text("abcdefghij", 4, 2);
        assert_eq!(capped.len(), 2);
        assert!(capped[1].ends_with(TRUNC_SUFFIX));
    }

    #[test]
    fn wrap_newlines_force_breaks() {
        let lines = wrap_text("ab\ncdefgh", 4, 8);
        assert_eq!(lines[0], "ab");
    }

    fn one_cell_view(value: &str) -> TableView {
        TableView::new(QueryResult {
            headers: vec!["a".to_string()],
            rows: vec![vec![value.to_string()]],
            truncated: false,
            kinds: Vec::new(),
        })
    }

    #[test]
    fn full_wrap_has_no_line_cap() {
        let mut v = one_cell_view(&"x".repeat(4 * (MAX_WRAP_LINES + 3)));
        v.toggle_wrap();
        let capped = v.cell_lines(0, 0, 4);
        assert_eq!(capped.len(), MAX_WRAP_LINES);
        assert!(capped.last().unwrap().ends_with(TRUNC_SUFFIX));
        v.toggle_full_wrap();
        let full = v.cell_lines(0, 0, 4);
        assert_eq!(full.len(), MAX_WRAP_LINES + 3);
        assert!(full.iter().all(|l| !l.contains(TRUNC_SUFFIX)));
        assert_eq!(v.row_height(0, &[4]), MAX_WRAP_LINES + 3);
    }

    #[test]
    fn wrap_mode_transitions() {
        let mut v = one_cell_view("1");
        assert_eq!(v.wrap, WrapMode::Off);
        v.toggle_wrap();
        assert_eq!(v.wrap, WrapMode::Capped);
        v.toggle_full_wrap();
        assert_eq!(v.wrap, WrapMode::Full);
        v.toggle_wrap();
        assert_eq!(v.wrap, WrapMode::Off);
        v.toggle_full_wrap();
        assert_eq!(v.wrap, WrapMode::Full);
        v.toggle_full_wrap();
        assert_eq!(v.wrap, WrapMode::Off);
    }

    #[test]
    fn view_scroll_clamps() {
        let v = TableView::new(QueryResult {
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["1".to_string(), "2".to_string()]],
            truncated: false,
            kinds: Vec::new(),
        });
        let mut v = v;
        v.scroll_down(99);
        assert_eq!(v.offset_y, 0);
        v.scroll_right(99);
        assert_eq!(v.offset_x, 1);
        v.scroll_up(5);
        v.scroll_left(5);
        assert_eq!((v.offset_y, v.offset_x), (0, 0));
    }
}
