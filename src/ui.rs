//! Ratatui rendering: input mode (scrollback + prompt) and table mode
//! (full-screen grid). Stateless: everything derives from `App` each frame.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, LineKind, Mode};
use crate::config::{
    COLOR_ERROR, COLOR_HINT, COLOR_OK, COLOR_PROMPT, COLOR_WARN, CONT_INDENT, INPUT_HINT, MAX_ROWS,
    PROMPT, QUIT_CONFIRM_MESSAGE,
};

/// Max visual rows the input box may occupy (rest goes to scrollback).
const MAX_INPUT_HEIGHT: u16 = 12;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height < 5 || area.width < 20 {
        frame.render_widget(
            Paragraph::new("Terminal too small — enlarge to use sqlight."),
            area,
        );
        return;
    }
    match app.mode {
        Mode::Input => render_input(frame, app, area),
        Mode::Table => render_table(frame, app, area),
    }
}

// -- input mode ---------------------------------------------------------------

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_h = input_visual_height(app, area.width).clamp(1, MAX_INPUT_HEIGHT);
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .split(area);

    render_scrollback(frame, app, chunks[0]);
    render_prompt(frame, app, chunks[1]);
    if app.is_quit_armed() {
        render_bar_line(
            frame,
            chunks[2],
            QUIT_CONFIRM_MESSAGE,
            Style::default().fg(COLOR_WARN).bold(),
        );
    } else {
        render_status(
            frame,
            chunks[2],
            "Enter run/newline · Tab complete · ↑↓ history · PgUp/PgDn output · Ctrl+C quit",
            &app.db_path.to_string_lossy(),
        );
    }
}

fn render_scrollback(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.scrollback {
        let style = Style::default().fg(kind_color(entry.kind));
        let mut first = true;
        for part in entry.text.split('\n') {
            if first {
                lines.push(Line::from(Span::styled(part.to_string(), style)));
                first = false;
            } else {
                // Continuation of a multiline echo: indent + dimmer prefix.
                lines.push(Line::from(vec![
                    Span::styled(CONT_INDENT, Style::default().fg(COLOR_HINT)),
                    Span::styled(part.to_string(), style),
                ]));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Connected. Type SQL ending with ; — or .tables — Ctrl+C to quit.",
            Style::default().fg(COLOR_HINT),
        )));
    }
    let text = Text::from(lines);
    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(para, area);
}

fn render_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let prompt_style = Style::default().fg(COLOR_PROMPT).bold();
    let mut lines: Vec<Line> = Vec::new();
    let buf_lines = app.input.lines();
    for (i, content) in buf_lines.iter().enumerate() {
        let prefix = if i == 0 { PROMPT } else { CONT_INDENT };
        if i == 0 && app.input.is_blank() {
            lines.push(Line::from(vec![
                Span::styled(prefix, prompt_style),
                Span::styled(INPUT_HINT, Style::default().fg(COLOR_HINT).italic()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(prefix, prompt_style),
                Span::raw(content.clone()),
            ]));
        }
    }
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    // Place the terminal cursor (accounts for visual wrapping).
    let (cx, cy) = cursor_visual_pos(app, area);
    if cy < area.height && cx < area.width {
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    }
}

/// Visual height (terminal rows) the input needs at `width`, with wrapping.
fn input_visual_height(app: &App, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    let mut total = 0usize;
    for (i, line) in app.input.lines().iter().enumerate() {
        let prefix_w = display_w(if i == 0 { PROMPT } else { CONT_INDENT });
        let content_w = if i == 0 && app.input.is_blank() {
            display_w(INPUT_HINT)
        } else {
            display_w(line)
        };
        total += (prefix_w + content_w).max(1).div_ceil(w);
    }
    total.max(1) as u16
}

/// Cursor position within the prompt area, accounting for wrapped lines.
fn cursor_visual_pos(app: &App, area: Rect) -> (u16, u16) {
    let w = area.width.max(1) as usize;
    let mut y = 0usize;
    for i in 0..app.input.row {
        let prefix_w = display_w(if i == 0 { PROMPT } else { CONT_INDENT });
        let line_w = display_w(&app.input.lines()[i]);
        y += (prefix_w + line_w).max(1).div_ceil(w);
    }
    let prefix_w = display_w(if app.input.row == 0 {
        PROMPT
    } else {
        CONT_INDENT
    });
    let upto: String = app.input.lines()[app.input.row]
        .chars()
        .take(app.input.col)
        .collect();
    let total = prefix_w + display_w(&upto);
    y += total / w;
    (u16::try_from(total % w).unwrap_or(0), y as u16)
}

// -- table mode ----------------------------------------------------------------

fn render_table(frame: &mut Frame, app: &App, area: Rect) {
    let Some(table) = app.table.as_ref() else {
        return;
    };
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(area);

    // Compose padded text lines (manual grid: full control over H-scroll +
    // variable row heights, no TableState API drift across versions).
    // Headers, separator and every row share the same visible columns and
    // the same fixed `col_widths`, so `←/→` scrolls them together and all
    // columns stay aligned. Every cell is padded to its column width;
    // wrap mode reflows text inside the same width.
    let visible: Vec<usize> = visible_cols(table).collect();
    let headers: Vec<String> = visible
        .iter()
        .map(|&c| pad_to(table.col_widths[c], &table.headers[c]))
        .collect();
    let sep = visible
        .iter()
        .map(|&c| "─".repeat(table.col_widths[c]))
        .collect::<Vec<_>>()
        .join("─┼─");
    let header_line = Line::from(Span::styled(headers.join(" │ "), Style::default().bold()));
    let sep_line = Line::from(Span::styled(sep, Style::default().fg(COLOR_HINT)));

    let mut body: Vec<Line> = vec![header_line, sep_line];
    let body_h = chunks[0].height.saturating_sub(2) as usize; // minus header+sep
    let mut used = 0usize;
    for r in table.offset_y..table.row_count() {
        let h = table.row_height(r).max(1);
        if used + h > body_h.max(1) {
            break;
        }
        if table.wrapped {
            // Multi-line row: stack wrapped cell lines side by side.
            let cells: Vec<Vec<String>> = visible.iter().map(|&c| table.cell_lines(r, c)).collect();
            for li in 0..h {
                let parts: Vec<String> = cells
                    .iter()
                    .enumerate()
                    .map(|(vi, cell)| {
                        let col = visible[vi];
                        cell.get(li)
                            .map(|s| pad_to(table.col_widths[col], s))
                            .unwrap_or_else(|| " ".repeat(table.col_widths[col]))
                    })
                    .collect();
                let style = if r % 2 == 1 {
                    Style::default().bg(ratatui::style::Color::Rgb(30, 30, 40))
                } else {
                    Style::default()
                };
                body.push(Line::from(Span::styled(parts.join(" │ "), style)));
            }
        } else {
            let parts: Vec<String> = visible
                .iter()
                .map(|&c| {
                    let s = table
                        .cell_lines(r, c)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    pad_to(table.col_widths[c], &s)
                })
                .collect();
            let style = if r % 2 == 1 {
                Style::default().bg(ratatui::style::Color::Rgb(30, 30, 40))
            } else {
                Style::default()
            };
            body.push(Line::from(Span::styled(parts.join(" │ "), style)));
        }
        used += h;
    }

    // Horizontal overflow: drop leading display-columns per offset is
    // column-granular already (offset_x skips whole columns). Clip the right
    // edge to the viewport width.
    let vw = chunks[0].width as usize;
    let clipped: Vec<Line> = body.into_iter().map(|l| clip_line(l, vw)).collect();

    // Show a "«" marker when columns are scrolled off to the left.
    let title = if table.offset_x > 0 {
        format!(
            " « {} rows × {} cols ",
            table.row_count(),
            table.col_count()
        )
    } else {
        format!(" {} rows × {} cols ", table.row_count(), table.col_count())
    };
    let para = Paragraph::new(Text::from(clipped))
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, chunks[0]);

    let wrap_state = if table.wrapped { "on" } else { "off" };
    let trunc_note = if table.truncated {
        format!(" · truncated to first {MAX_ROWS}")
    } else {
        String::new()
    };
    let status = format!(
        "row {}/{} · col {}/{}{} · wrap: {}",
        table.offset_y + 1,
        table.row_count().max(1),
        table.offset_x + 1,
        table.col_count().max(1),
        trunc_note,
        wrap_state,
    );
    // Two-line footer: dynamic status first (never clipped away), key help second.
    let foot = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chunks[1]);
    render_bar_line(
        frame,
        foot[0],
        &crate::table_view::truncate_text(&status, foot[0].width as usize),
        Style::default().bold(),
    );
    render_bar_line(
        frame,
        foot[1],
        "ESC back · w wrap · ↑↓←→ scroll · PgUp/PgDn · Ctrl+C back",
        Style::default().fg(COLOR_HINT),
    );
    // No cursor in table mode (hidden by not setting a position).
}

/// Column indices currently in view (all from offset_x; viewport clips right).
fn visible_cols(table: &crate::table_view::TableView) -> impl Iterator<Item = usize> + '_ {
    table.offset_x..table.col_count()
}

// -- shared bits ----------------------------------------------------------------

fn render_status(frame: &mut Frame, area: Rect, left: &str, right: &str) {
    // The right side (DB name) is always preserved; hints truncate on narrow screens.
    let w = area.width as usize;
    let rw = display_w(right);
    let max_left = w.saturating_sub(rw + 2);
    let left_shown = if display_w(left) > max_left {
        crate::table_view::truncate_text(left, max_left)
    } else {
        left.to_string()
    };
    let gap = w.saturating_sub(display_w(&left_shown) + rw + 2);
    let text = format!("{left_shown} {:gap$} {right}", "", gap = gap);
    render_bar_line(frame, area, &text, Style::default().fg(COLOR_HINT));
}

fn render_bar_line(frame: &mut Frame, area: Rect, text: &str, style: Style) {
    let para = Paragraph::new(Line::from(Span::styled(text.to_string(), style)));
    frame.render_widget(para, area);
}

fn kind_color(kind: LineKind) -> ratatui::style::Color {
    match kind {
        LineKind::Echo => ratatui::style::Color::Reset,
        LineKind::Ok => COLOR_OK,
        LineKind::Err => COLOR_ERROR,
        LineKind::Warn => COLOR_WARN,
    }
}

fn display_w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pad_to(width: usize, s: &str) -> String {
    let w = display_w(s);
    if w >= width {
        return crate::table_view::truncate_text(s, width);
    }
    let mut out = s.to_string();
    out.push_str(&" ".repeat(width - w));
    out
}

/// Clip a composed line to `max_w` display columns (keeps style).
fn clip_line(line: Line<'_>, max_w: usize) -> Line<'_> {
    let mut out_spans = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= max_w {
            break;
        }
        let sw = display_w(span.content.as_ref());
        if used + sw <= max_w {
            used += sw;
            out_spans.push(span);
        } else {
            // Hard viewport clip (no "..." — the grid already marks truncation).
            let mut acc = String::new();
            let mut acc_w = 0usize;
            for ch in span.content.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if acc_w + cw > max_w - used {
                    break;
                }
                acc.push(ch);
                acc_w += cw;
            }
            out_spans.push(Span::styled(acc, span.style));
            used = max_w;
        }
    }
    Line::from(out_spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn screen_text(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_input_mode() {
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(PathBuf::from("demo.db"));
        app.push_line("# select 1;", LineKind::Echo);
        app.push_line("Error: boom", LineKind::Err);
        term.draw(|f| render(f, &app)).expect("draw input");
        let text = screen_text(&term);
        assert!(text.contains("# "), "prompt visible");
        assert!(text.contains("boom"), "error line visible");
        assert!(text.contains("Tab complete"), "hint bar visible");
    }

    #[test]
    fn renders_table_mode_with_wrap() {
        use crate::db::QueryResult;
        use crate::table_view::TableView;

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(PathBuf::from("demo.db"));
        let mut view = TableView::new(QueryResult {
            headers: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec!["1".to_string(), "greg".to_string()],
                vec!["2".to_string(), "a-much-longer-value-here".to_string()],
            ],
            truncated: true,
        });
        view.toggle_wrap();
        app.table = Some(view);
        app.mode = Mode::Table;
        term.draw(|f| render(f, &app)).expect("draw table");
        let text = screen_text(&term);
        assert!(text.contains("greg"), "row visible");
        assert!(text.contains("wrap: on"), "wrap state shown");
        assert!(text.contains("truncated"), "truncation note shown");
        assert!(text.contains("ESC back"), "help shown");
        assert!(text.contains("Ctrl+C back"), "table Ctrl+C goes back");
    }

    #[test]
    fn renders_quit_confirm_and_reverts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        use std::time::Duration;

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("terminal");
        let mut app = App::new(PathBuf::from("demo.db"));
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        app.handle_key(ctrl_c);
        term.draw(|f| render(f, &app)).expect("draw confirm");
        let text = screen_text(&term);
        assert!(
            text.contains(QUIT_CONFIRM_MESSAGE),
            "confirm message visible, got: {text}"
        );
        // After the timeout the normal hint bar comes back.
        app.quit_armed_at = Some(
            std::time::Instant::now()
                - crate::config::QUIT_CONFIRM_TIMEOUT
                - Duration::from_millis(10),
        );
        assert!(app.expire_quit_arm());
        term.draw(|f| render(f, &app)).expect("draw reverted");
        let text = screen_text(&term);
        assert!(
            !text.contains(QUIT_CONFIRM_MESSAGE),
            "confirm message reverted"
        );
        assert!(text.contains("Tab complete"), "hint bar restored");
    }
}
