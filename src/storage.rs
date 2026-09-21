//! Per-user persistent storage (command history file).
//! Best-effort: every IO failure degrades to empty memory history or a
//! skipped write, never a panic or a user-facing error.
//!
//! Location (most common per-user data place per OS):
//! - Windows: `%APPDATA%\sqlight\history`
//! - macOS: `~/Library/Application Support/sqlight/history`
//! - Linux/other: `$XDG_DATA_HOME/sqlight/history`
//!   or `~/.local/share/sqlight/history`
//!
//! File format: one history entry per line. Because entries can be
//! multiline, `\` escapes are used inside a line: `\\` for a literal
//! backslash, `\n` for newline, `\r` for carriage return.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{APP_DATA_DIR, HISTORY_FILE_NAME};
use crate::editor::History;

/// Resolve the per-user history file path, or `None` when no home-like
/// env var is available. Never touches the filesystem.
pub fn history_file_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(dir) = env::var_os("APPDATA")
            && !dir.is_empty()
        {
            return Some(
                PathBuf::from(dir)
                    .join(APP_DATA_DIR)
                    .join(HISTORY_FILE_NAME),
            );
        }
        if let Some(profile) = env::var_os("USERPROFILE")
            && !profile.is_empty()
        {
            // Default location of %APPDATA% when the variable itself is unset.
            return Some(
                PathBuf::from(profile)
                    .join("AppData")
                    .join("Roaming")
                    .join(APP_DATA_DIR)
                    .join(HISTORY_FILE_NAME),
            );
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = env::var_os("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join(APP_DATA_DIR)
                    .join(HISTORY_FILE_NAME),
            );
        }
        None
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if let Some(data_home) = env::var_os("XDG_DATA_HOME")
            && !data_home.is_empty()
        {
            return Some(
                PathBuf::from(data_home)
                    .join(APP_DATA_DIR)
                    .join(HISTORY_FILE_NAME),
            );
        }
        if let Some(home) = env::var_os("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join(APP_DATA_DIR)
                    .join(HISTORY_FILE_NAME),
            );
        }
        None
    }
}

/// Load history from an explicit path (used by `App` and tests).
/// Missing file or any IO problem yields an empty history.
pub fn load_history_from_file(path: &Path) -> History {
    let Ok(text) = fs::read_to_string(path) else {
        return History::new();
    };
    let mut history = History::new();
    for line in text.lines() {
        // `History::push` re-applies blank filtering, consecutive-dup
        // suppression and `HISTORY_LIMIT` trimming.
        history.push(decode_entry(line));
    }
    history
}

/// Persist history to the per-user file. Creates parent dirs as needed.
/// Failures are reported to the caller; `App` ignores them (best-effort).
pub fn save_history_to_file(history: &History, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for entry in history.entries() {
        out.push_str(&encode_entry(entry));
        out.push('\n');
    }
    fs::write(path, out)
}

/// Escape one entry onto a single line: `\` -> `\\`, LF -> `\n`, CR -> `\r`.
fn encode_entry(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for c in entry.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`encode_entry`]. A trailing lone `\` and unknown `\x`
/// sequences are preserved verbatim.
fn decode_entry(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_codec_roundtrip() {
        let cases = [
            "select 1;",
            "line1\nline2;",
            "update users set name = 'a\\b'\nwhere id = 1;",
            "carriage\rreturn",
            "trailing backslash\\",
            "héllo wörld\nnewline",
            "select '\\n' -- literal backslash-n in SQL",
        ];
        for case in cases {
            assert_eq!(decode_entry(&encode_entry(case)), case, "{case:?}");
        }
        // Encoded form never contains a raw newline.
        for case in cases {
            assert!(
                !encode_entry(case).contains('\n'),
                "{case:?} leaked newline"
            );
        }
    }

    #[test]
    fn decode_preserves_unknown_escapes_and_trailing_backslash() {
        assert_eq!(decode_entry("a\\tb"), "a\\tb");
        assert_eq!(decode_entry("a\\"), "a\\");
        assert_eq!(decode_entry("\\\\"), "\\");
        assert_eq!(decode_entry("\\n"), "\n");
    }

    #[test]
    fn save_load_roundtrip_preserves_multiline_and_order() {
        let dir = std::env::temp_dir().join(format!(
            "sqlight-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = dir.join("history");
        let mut history = History::new();
        history.push("select 1;".to_string());
        history.push("line1\nline2;".to_string());
        history.push("select '\\n';".to_string());
        save_history_to_file(&history, &path).expect("save");
        let loaded = load_history_from_file(&path);
        assert_eq!(loaded.entries(), history.entries());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = std::env::temp_dir().join(format!(
            "sqlight-history-missing-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = fs::remove_file(&path);
        assert!(load_history_from_file(&path).is_empty());
    }

    #[test]
    fn save_creates_parent_dirs() {
        let base = std::env::temp_dir().join(format!(
            "sqlight-history-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = base.join("nested").join("deep").join("history");
        let mut history = History::new();
        history.push("select 1;".to_string());
        save_history_to_file(&history, &path).expect("save with parents");
        assert!(path.is_file());
        let _ = fs::remove_dir_all(&base);
    }
}
