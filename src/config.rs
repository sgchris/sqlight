//! Compile-time configuration. Change values here and rebuild.
//! Nothing in this file is read at runtime from disk or env.

/// Prompt shown on the first input line.
pub const PROMPT: &str = "# ";
/// Indentation for continuation lines of a multiline statement.
pub const CONT_INDENT: &str = "    ";
/// Placeholder shown when the input buffer is empty.
pub const INPUT_HINT: &str = "SQL + ;  or  .tables  —  Enter: run/newline  Tab: complete";

/// Minimum on-screen column width in the results grid.
pub const MIN_COL_WIDTH: usize = 8;
/// Maximum on-screen column width in the results grid.
pub const MAX_COL_WIDTH: usize = 30;
/// Default maximum number of rows fetched/displayed for a SELECT.
pub const MAX_ROWS: usize = 100;
/// Maximum wrapped lines per cell when wrap mode is on.
pub const MAX_WRAP_LINES: usize = 8;
/// Suffix appended to truncated values.
pub const TRUNC_SUFFIX: &str = "...";

/// Maximum commands remembered for Up/Down history.
pub const HISTORY_LIMIT: usize = 200;
/// Maximum scrollback lines kept in command mode.
pub const SCROLLBACK_LIMIT: usize = 500;
/// SQLite busy timeout per statement, in milliseconds.
pub const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Minimum prefix length that triggers autocompletion.
pub const COMPLETE_MIN_CHARS: usize = 2;

/// Status/error color palette (Ratatui colors).
pub const COLOR_OK: ratatui::style::Color = ratatui::style::Color::Green;
pub const COLOR_ERROR: ratatui::style::Color = ratatui::style::Color::LightRed;
pub const COLOR_WARN: ratatui::style::Color = ratatui::style::Color::Rgb(255, 165, 0);
pub const COLOR_PROMPT: ratatui::style::Color = ratatui::style::Color::Cyan;
pub const COLOR_HINT: ratatui::style::Color = ratatui::style::Color::DarkGray;
