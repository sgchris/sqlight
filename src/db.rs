//! SQLite access. One short-lived connection per statement so the DB
//! file is never held locked between queries.

use rusqlite::{Connection, types::ValueRef};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{BUSY_TIMEOUT_MS, MAX_ROWS};

/// Cached schema names used for autocompletion.
#[derive(Debug, Clone, Default)]
pub struct SchemaCache {
    pub tables: Vec<String>,
    pub columns: Vec<String>,
}

/// Grid result for SELECT-family statements.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// True when more rows existed than `MAX_ROWS` and output was cut.
    pub truncated: bool,
}

fn open_conn(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    Ok(conn)
}

/// Check the file before entering the TUI. Returns a user-facing message.
pub fn preflight_db(db_path: &Path) -> Result<(), String> {
    if !db_path.exists() {
        return Err(format!(
            "Error: file '{}' doesn't exist. SQLight never creates database files.",
            db_path.display()
        ));
    }
    if !db_path.is_file() {
        return Err(format!(
            "Error: '{}' is not a regular file.",
            db_path.display()
        ));
    }
    match open_conn(db_path) {
        Ok(conn) => match conn.query_row("SELECT 1", [], |_| Ok(())) {
            Ok(()) => Ok(()),
            Err(e) => Err(friendly_rusqlite_error(db_path, &e)),
        },
        Err(e) => Err(friendly_rusqlite_error(db_path, &e)),
    }
}

fn friendly_rusqlite_error(db_path: &Path, e: &rusqlite::Error) -> String {
    use rusqlite::ErrorCode;
    let hint = match e {
        rusqlite::Error::SqliteFailure(err, _) => match err.code {
            ErrorCode::CannotOpen => " (inaccessible — check permissions and path)",
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                " (file is locked by another process)"
            }
            ErrorCode::NotADatabase => " (not a valid SQLite database)",
            _ => "",
        },
        _ => "",
    };
    format!("Error: cannot open '{}': {e}{hint}", db_path.display())
}

/// Map any rusqlite error to a one-line user-facing message.
pub fn friendly_query_error(e: &rusqlite::Error) -> String {
    format!("Error: {e}")
}

/// `SELECT name FROM sqlite_master ...` — user tables only.
pub fn list_tables(db_path: &Path) -> Result<Vec<String>, rusqlite::Error> {
    let conn = open_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY 1",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tables)
}

/// `CREATE TABLE` + index statements for one table. Errors if unknown.
pub fn get_schema(db_path: &Path, table: &str) -> Result<Vec<String>, rusqlite::Error> {
    let conn = open_conn(db_path)?;
    let create: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = ?1 AND type IN ('table','view')",
            [table],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let Some(mut out) = create.map(|s| vec![s]) else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    // Indexes (excluding auto-indexes whose sql is NULL).
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL",
    )?;
    let mut idx_rows = stmt.query([table])?;
    while let Some(row) = idx_rows.next()? {
        let sql: String = row.get(0)?;
        out.push(sql);
    }
    Ok(out)
}

/// Refresh TAB-completion names: tables + union of all columns.
pub fn refresh_schema_cache(db_path: &Path) -> SchemaCache {
    let mut cache = SchemaCache::default();
    let Ok(conn) = open_conn(db_path) else {
        return cache;
    };
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    let mut cols: Vec<String> = Vec::new();
    for t in &tables {
        // Table name is quoted defensively; PRAGMA doesn't take bound params.
        let pragma = format!("PRAGMA table_info({})", quote_ident(t));
        let Ok(mut stmt) = conn.prepare(&pragma) else {
            continue;
        };
        let Ok(mapped) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
            continue;
        };
        for c in mapped.flatten() {
            if !cols.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
                cols.push(c);
            }
        }
    }
    cache.tables = tables;
    cache.columns = cols;
    cache
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Execute INSERT/UPDATE/DELETE/DDL. Returns affected row count.
pub fn execute_write(db_path: &Path, sql: &str) -> Result<usize, rusqlite::Error> {
    let conn = open_conn(db_path)?;
    conn.execute(sql, [])
}

/// Run a SELECT-family statement, capping at `MAX_ROWS` (+1 probe row).
pub fn query_select(db_path: &Path, sql: &str) -> Result<QueryResult, rusqlite::Error> {
    query_select_limited(db_path, sql, MAX_ROWS)
}

fn query_select_limited(
    db_path: &Path,
    sql: &str,
    max_rows: usize,
) -> Result<QueryResult, rusqlite::Error> {
    let conn = open_conn(db_path)?;
    let mut stmt = conn.prepare(sql)?;
    let headers: Vec<String> = stmt
        .column_names()
        .iter()
        .map(ToString::to_string)
        .collect();
    let col_count = headers.len();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut query_rows = stmt.query([])?;
    let mut truncated = false;
    while let Some(row) = query_rows.next()? {
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut out = Vec::with_capacity(col_count);
        for i in 0..col_count {
            out.push(value_to_string(row.get_ref(i)?));
        }
        rows.push(out);
    }
    Ok(QueryResult {
        headers,
        rows,
        truncated,
    })
}

fn value_to_string(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

/// Build a scratch DB path helper for tests.
#[cfg(test)]
pub fn test_db_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("sqlight-test-{name}-{}-{n}.db", std::process::id()));
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed_db() -> PathBuf {
        let path = test_db_path("seed");
        let _ = fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
             CREATE INDEX idx_users_name ON users(name);
             INSERT INTO users (name, age) VALUES ('greg', 30), ('ana', 25);",
        )
        .unwrap();
        path
    }

    #[test]
    fn preflight_rejects_missing_file() {
        let err = preflight_db(Path::new("/definitely/not/here/sqlight-xyz.db")).unwrap_err();
        assert!(err.contains("doesn't exist"), "{err}");
    }

    #[test]
    fn list_and_schema_roundtrip() {
        let path = seed_db();
        let tables = list_tables(&path).unwrap();
        assert_eq!(tables, vec!["users".to_string()]);
        let schema = get_schema(&path, "users").unwrap();
        assert!(schema.iter().any(|s| s.contains("CREATE TABLE users")));
        assert!(schema.iter().any(|s| s.contains("idx_users_name")));
        assert!(matches!(
            get_schema(&path, "nope"),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn select_cap_and_value_mapping() {
        let path = seed_db();
        let res =
            query_select_limited(&path, "SELECT id, name, age FROM users ORDER BY id", 1).unwrap();
        assert_eq!(res.headers, vec!["id", "name", "age"]);
        assert_eq!(res.rows.len(), 1);
        assert!(res.truncated);
        assert_eq!(res.rows[0][1], "greg");
        let full = query_select(&path, "SELECT NULL AS n").unwrap();
        assert_eq!(full.rows[0][0], "NULL");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_returns_affected_count() {
        let path = seed_db();
        let n = execute_write(&path, "DELETE FROM users WHERE age < 30").unwrap();
        assert_eq!(n, 1);
        let _ = fs::remove_file(&path);
    }
}
