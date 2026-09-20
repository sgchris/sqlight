//! Results-grid state: column widths, truncation (`...`), wrapping
//! (max 8 lines), and scroll offsets. Rendering lives in `ui.rs`.

use unicode_width::UnicodeWidthStr;

use crate::config::{MAX_COL_WIDTH, MAX_WRAP_LINES, MIN_COL_WIDTH, TRUNC_SUFFIX};
use crate::db::QueryResult;

/// Full-screen grid state for one SELECT result.
#[derive(Debug, Clone)]
pub struct TableView {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
    pub col_widths: Vec<usize>,
    pub wrapped: bool,
    /// First visible data row.
    pub offset_y: usize,
    /// First visible column.
    pub offset_x: usize,
}

impl TableView {
    pub fn new(result: QueryResult) -> Self {
        let col_widths = compute_widths(&result.headers, &result.rows);
        Self {
            headers: result.headers,
            rows: result.rows,
            truncated: result.truncated,
            col_widths,
            wrapped: false,
            offset_y: 0,
            offset_x: 0,
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn col_count(&self) -> usize {
        self.headers.len()
    }

    pub fn toggle_wrap(&mut self) {
        self.wrapped = !self.wrapped;
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

    /// Rendered lines for one cell (wrapped or single truncated line).
    pub fn cell_lines(&self, row: usize, col: usize) -> Vec<String> {
        let raw = self
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(String::as_str)
            .unwrap_or("");
        let width = self.col_widths.get(col).copied().unwrap_or(MIN_COL_WIDTH);
        if self.wrapped {
            wrap_text(raw, width, MAX_WRAP_LINES)
        } else {
            vec![truncate_text(raw, width)]
        }
    }

    /// Height (in terminal rows) of a data row under the current mode.
    pub fn row_height(&self, row: usize) -> usize {
        if !self.wrapped {
            return 1;
        }
        (0..self.col_count())
            .map(|c| self.cell_lines(row, c).len().max(1))
            .max()
            .unwrap_or(1)
    }
}

/// Column widths clamped to `[MIN_COL_WIDTH, MAX_COL_WIDTH]` using
/// display width (Unicode-aware).
pub fn compute_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<usize> {
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
            w.min(MAX_COL_WIDTH)
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
    fn widths_clamped() {
        let headers = vec!["id".to_string(), "name".to_string()];
        let rows = vec![vec![
            "1".to_string(),
            "a-very-long-value-that-exceeds-maximum-width".to_string(),
        ]];
        let w = compute_widths(&headers, &rows);
        assert_eq!(w[0], MIN_COL_WIDTH);
        assert_eq!(w[1], MAX_COL_WIDTH);
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

    #[test]
    fn view_scroll_clamps() {
        let v = TableView::new(QueryResult {
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["1".to_string(), "2".to_string()]],
            truncated: false,
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
