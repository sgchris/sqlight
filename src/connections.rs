//! Startup target resolution: an existing SQLite file first, otherwise a
//! named connection from the per-user connections file
//! (`~/.config/sqlight/connections.json`).
//!
//! File format: a JSON object mapping connection names to objects with a
//! `type` key. Only the requested entry is deserialized, so entries of
//! types not supported yet (e.g. `mysql`) never break the others.

use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::CONNECTIONS_FILE_REL;

/// PostgreSQL connection settings. `password: None` means "ask the user".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PgConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
}

/// What `sqlight ARG` should open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Sqlite(PathBuf),
    Postgres { name: String, cfg: PgConfig },
}

/// Absolute path of the connections file, or `None` without a home dir.
pub fn connections_file_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").filter(|h| !h.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| env::var_os("USERPROFILE").filter(|h| !h.is_empty()));
    home.map(|h| PathBuf::from(h).join(CONNECTIONS_FILE_REL))
}

/// Connections file path for messages; `~/...` when home is unknown.
pub fn connections_file_display() -> String {
    connections_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("~/{CONNECTIONS_FILE_REL}"))
}

/// Usage text shown when `sqlight` is run without a target.
pub fn missing_target_message(config_path: &str) -> String {
    format!(
        "Usage: sqlight <TARGET>\n\n\
         TARGET is either:\n  \
         - a path to an existing SQLite database file, or\n  \
         - the name of a connection defined in {config_path}\n\n\
         Run `sqlight --help` for more options."
    )
}

/// Resolve the CLI argument against the filesystem and connections file.
pub fn resolve_target(arg: &str) -> Result<Target, String> {
    let path = Path::new(arg);
    if path.exists() {
        return Ok(Target::Sqlite(path.to_path_buf()));
    }
    let display = connections_file_display();
    let text = match connections_file_path() {
        Some(p) => fs::read_to_string(p),
        None => Err(io::Error::new(io::ErrorKind::NotFound, "no home directory")),
    };
    resolve_from_config(arg, &display, text)
}

/// Lookup for an `arg` that is not an existing file. Pure over its inputs.
fn resolve_from_config(
    arg: &str,
    config_path: &str,
    config_text: io::Result<String>,
) -> Result<Target, String> {
    let not_found = || {
        format!(
            "Error: '{arg}' is neither an existing SQLite file nor a connection defined in {config_path}"
        )
    };
    let text = match config_text {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_found()),
        Err(e) => return Err(format!("Error: cannot read {config_path}: {e}")),
    };
    let entries: HashMap<String, serde_json::Value> = serde_json::from_str(&text)
        .map_err(|e| format!("Error: cannot parse {config_path}: {e}"))?;
    let Some(entry) = entries.get(arg) else {
        return Err(not_found());
    };
    let kind = entry.get("type").and_then(|t| t.as_str()).ok_or_else(|| {
        format!("Error: connection '{arg}' in {config_path} has no string \"type\" field")
    })?;
    match kind {
        "postgresql" => {
            let cfg: PgConfig = serde_json::from_value(entry.clone())
                .map_err(|e| format!("Error: invalid connection '{arg}' in {config_path}: {e}"))?;
            Ok(Target::Postgres {
                name: arg.to_string(),
                cfg,
            })
        }
        other => Err(format!(
            "Error: connection '{arg}' has type '{other}', which is not supported yet (supported: postgresql)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "/home/u/.config/sqlight/connections.json";

    fn resolve(arg: &str, json: &str) -> Result<Target, String> {
        resolve_from_config(arg, CFG, Ok(json.to_string()))
    }

    const SAMPLE: &str = r#"{
        "prod1": {"type": "postgresql", "host": "db.example.test", "port": 5432,
                  "database": "app", "user": "reader", "password": "secret"},
        "nopw": {"type": "postgresql", "host": "h", "port": 1, "database": "d", "user": "u"},
        "emptypw": {"type": "postgresql", "host": "h", "port": 1, "database": "d", "user": "u", "password": ""},
        "strport": {"type": "postgresql", "host": "h", "port": "5432", "database": "d", "user": "u"},
        "prod2": {"type": "mysql", "host": "h", "port": 3306, "database": "d", "user": "u"}
    }"#;

    #[test]
    fn existing_file_wins_over_connection_name() {
        let dir = std::env::temp_dir().join(format!("sqlight-conn-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("prod1");
        fs::write(&file, b"").unwrap();
        let arg = file.to_string_lossy().to_string();
        assert_eq!(resolve_target(&arg), Ok(Target::Sqlite(file.clone())));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn postgres_entry_parses_next_to_unsupported_type() {
        let Ok(Target::Postgres { name, cfg }) = resolve("prod1", SAMPLE) else {
            panic!("expected postgres target");
        };
        assert_eq!(name, "prod1");
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.password.as_deref(), Some("secret"));
    }

    #[test]
    fn unsupported_type_is_rejected_by_name() {
        let err = resolve("prod2", SAMPLE).unwrap_err();
        assert!(
            err.contains("'mysql'") && err.contains("not supported"),
            "{err}"
        );
    }

    #[test]
    fn absent_vs_empty_password() {
        let Ok(Target::Postgres { cfg, .. }) = resolve("nopw", SAMPLE) else {
            panic!()
        };
        assert_eq!(cfg.password, None);
        let Ok(Target::Postgres { cfg, .. }) = resolve("emptypw", SAMPLE) else {
            panic!()
        };
        assert_eq!(cfg.password.as_deref(), Some(""));
    }

    #[test]
    fn port_must_be_integer() {
        let err = resolve("strport", SAMPLE).unwrap_err();
        assert!(err.contains("invalid connection 'strport'"), "{err}");
    }

    #[test]
    fn unknown_name_and_missing_file_mention_config_path() {
        let err = resolve("nope", SAMPLE).unwrap_err();
        assert!(err.contains("'nope'") && err.contains(CFG), "{err}");
        let missing = resolve_from_config(
            "nope",
            CFG,
            Err(io::Error::new(io::ErrorKind::NotFound, "x")),
        )
        .unwrap_err();
        assert!(missing.contains(CFG), "{missing}");
    }

    #[test]
    fn invalid_json_is_reported() {
        let err = resolve("x", "{not json").unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
    }

    #[test]
    fn missing_target_message_lists_both_options_and_path() {
        let msg = missing_target_message(CFG);
        assert!(msg.contains("SQLite database file"));
        assert!(msg.contains("name of a connection"));
        assert!(msg.contains(CFG));
    }
}
