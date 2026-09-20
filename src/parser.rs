//! Pure SQL/dot-command parsing helpers (no IO, fully unit-tested).
//!
//! - `has_trailing_semi`: string/comment-aware check for a terminating `;`.
//! - `classify`: grid (SELECT-family) vs textual (write) execution.
//! - `parse_dot_command`: `.tables` / `.schema TABLE` parsing.

/// How a complete statement should be executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// Opens the full-screen results grid.
    Select,
    /// Prints a textual confirmation (`Affected N rows` / `OK`).
    Write,
}

/// A supported internal command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotCommand {
    Tables,
    Schema { table: String },
}

/// Error for unknown or malformed dot-commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotError {
    Unknown(String),
    MissingTable,
    TooManyArgs,
    Empty,
}

impl std::fmt::Display for DotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(cmd) => write!(
                f,
                "Unknown command: {cmd}. Supported: .tables, .schema TABLE"
            ),
            Self::MissingTable => write!(f, "Usage: .schema TABLE_NAME"),
            Self::TooManyArgs => write!(f, "Too many arguments. Usage: .schema TABLE_NAME"),
            Self::Empty => write!(f, "Empty command"),
        }
    }
}

/// Returns true if the text ends with a `;` that is actual SQL terminator
/// (not inside a string literal, quoted identifier, or comment).
///
/// Trailing whitespace and trailing `--` / `/* */` comments are ignored.
pub fn has_trailing_semi(sql: &str) -> bool {
    // Scan tracking lexical state; remember the last *significant* char.
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let n = chars.len();
    let mut last_sig: Option<char> = None;
    let mut in_single = false; // '...'
    let mut in_double = false; // "..." (quoted identifier)
    let mut in_block = false; // /* ... */
    while i < n {
        let c = chars[i];
        if in_block {
            if c == '*' && i + 1 < n && chars[i + 1] == '/' {
                in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if c == '\'' {
                // '' is an escaped quote inside a string literal.
                if i + 1 < n && chars[i + 1] == '\'' {
                    i += 2;
                } else {
                    in_single = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        if in_double {
            if c == '"' {
                if i + 1 < n && chars[i + 1] == '"' {
                    i += 2;
                } else {
                    in_double = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        // Not inside string/comment.
        if c == '\'' {
            in_single = true;
            i += 1;
        } else if c == '"' {
            in_double = true;
            i += 1;
        } else if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            // Line comment: skip to end of line.
            i += 2;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            in_block = true;
            i += 2;
        } else if c.is_whitespace() {
            i += 1;
        } else {
            last_sig = Some(c);
            i += 1;
        }
    }
    last_sig == Some(';')
}

/// Remove one trailing `;` plus surrounding whitespace. Idempotent-ish:
/// only strips a single terminator at the end.
pub fn strip_trailing_semi(sql: &str) -> String {
    let trimmed = sql.trim_end();
    if let Some(without) = trimmed.strip_suffix(';') {
        without.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Decide grid vs textual execution from the first keyword.
pub fn classify(sql: &str) -> StatementKind {
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('(')
        .to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "EXPLAIN" | "PRAGMA" | "TABLE" => StatementKind::Select,
        _ => StatementKind::Write,
    }
}

/// Parse a dot-command line (leading whitespace allowed).
/// Returns `None` when the line is not a dot-command at all.
pub fn parse_dot_command(line: &str) -> Option<Result<DotCommand, DotError>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Some(Err(DotError::Empty));
    }
    if !trimmed.starts_with('.') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        ".tables" => {
            if parts.next().is_some() {
                Some(Err(DotError::TooManyArgs))
            } else {
                Some(Ok(DotCommand::Tables))
            }
        }
        ".schema" => match parts.next() {
            None => Some(Err(DotError::MissingTable)),
            Some(table) => {
                if parts.next().is_some() {
                    Some(Err(DotError::TooManyArgs))
                } else {
                    Some(Ok(DotCommand::Schema {
                        table: table.to_string(),
                    }))
                }
            }
        },
        other => Some(Err(DotError::Unknown(other.to_string()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_terminated() {
        assert!(has_trailing_semi("select * from users;"));
        assert!(has_trailing_semi("select * from users;  \n  "));
    }

    #[test]
    fn unterminated_and_empty() {
        assert!(!has_trailing_semi("select * from users"));
        assert!(!has_trailing_semi(""));
        assert!(!has_trailing_semi("   \n  "));
    }

    #[test]
    fn semicolon_inside_string_does_not_count() {
        assert!(!has_trailing_semi("insert into t values ('a;b')"));
        assert!(has_trailing_semi("insert into t values ('a;b');"));
        assert!(!has_trailing_semi("select \"we;ird\" from t"));
    }

    #[test]
    fn escaped_quotes() {
        assert!(has_trailing_semi("insert into t values ('it''s; fine');"));
        assert!(!has_trailing_semi("insert into t values ('it''s; fine')"));
    }

    #[test]
    fn trailing_comments_ignored() {
        assert!(has_trailing_semi("select 1; -- done"));
        assert!(has_trailing_semi("select 1; /* done */"));
        assert!(!has_trailing_semi("select 1 -- ;"));
        assert!(!has_trailing_semi("/* ; */ select 1"));
    }

    #[test]
    fn multiline_terminated() {
        let sql = "update users set\n    name = \"greg\",\n    age = 30\n  where id = 10;";
        assert!(has_trailing_semi(sql));
        assert!(!has_trailing_semi("update users set\n    name = \"greg\""));
    }

    #[test]
    fn strip_semi() {
        assert_eq!(strip_trailing_semi("select 1;  "), "select 1");
        assert_eq!(strip_trailing_semi("select 1"), "select 1");
    }

    #[test]
    fn classify_select_family() {
        for q in [
            "select 1",
            "  SELECT * FROM t",
            "with x as (select 1) select * from x",
            "VALUES (1),(2)",
            "explain select 1",
            "pragma table_info(users)",
        ] {
            assert_eq!(classify(q), StatementKind::Select, "{q}");
        }
    }

    #[test]
    fn classify_writes() {
        for q in [
            "insert into t values (1)",
            "UPDATE t SET a=1",
            "delete from t",
            "create table t (a)",
            "drop table t",
        ] {
            assert_eq!(classify(q), StatementKind::Write, "{q}");
        }
    }

    #[test]
    fn dot_tables() {
        assert_eq!(parse_dot_command(".tables"), Some(Ok(DotCommand::Tables)));
        assert_eq!(
            parse_dot_command("  .tables  "),
            Some(Ok(DotCommand::Tables))
        );
        assert!(matches!(
            parse_dot_command(".tables extra"),
            Some(Err(DotError::TooManyArgs))
        ));
    }

    #[test]
    fn dot_schema() {
        assert_eq!(
            parse_dot_command(".schema users"),
            Some(Ok(DotCommand::Schema {
                table: "users".to_string()
            }))
        );
        assert_eq!(
            parse_dot_command(".schema"),
            Some(Err(DotError::MissingTable))
        );
        assert!(parse_dot_command("select 1").is_none());
        assert!(matches!(
            parse_dot_command(".foo"),
            Some(Err(DotError::Unknown(_)))
        ));
    }
}
