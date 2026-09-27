//! PostgreSQL access over one persistent session. Unlike SQLite there is
//! no file lock to avoid, and reconnecting per statement would cost a
//! network round-trip plus re-authentication each time.

use ::postgres::config::SslMode;
use std::collections::HashMap;

use ::postgres::{Client, Config, SimpleQueryMessage};
use postgres_native_tls::MakeTlsConnector;

use super::{DbError, QueryResult, SchemaCache};
use crate::config::{CONNECT_TIMEOUT, MAX_ROWS};
use crate::connections::PgConfig;

const CURSOR: &str = "sqlight_cur";
const SAVEPOINT: &str = "sqlight_sp";
const USER_SCHEMAS_FILTER: &str =
    "NOT IN ('pg_catalog', 'information_schema') AND table_schema NOT LIKE 'pg_toast%'";

/// A live PostgreSQL session plus what the status bar shows about it.
pub struct PgDb {
    label: String,
    client: Client,
    /// Whether the user opened an explicit transaction (`BEGIN`), so our
    /// own cursor bookkeeping must not commit it.
    in_user_tx: bool,
}

impl PgDb {
    /// Connect with TLS preferred (libpq `sslmode=prefer` semantics: encrypt
    /// when the server supports it, without certificate verification).
    pub fn connect(name: &str, cfg: &PgConfig, password: &str) -> Result<Self, String> {
        let fail = |e: &dyn std::fmt::Display| format!("Error: cannot connect to '{name}': {e}");
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| fail(&e))?;
        let mut client = Config::new()
            .host(&cfg.host)
            .port(cfg.port)
            .dbname(&cfg.database)
            .user(&cfg.user)
            .password(password)
            .ssl_mode(SslMode::Prefer)
            .connect_timeout(CONNECT_TIMEOUT)
            .application_name("sqlight")
            .connect(MakeTlsConnector::new(tls))
            .map_err(|e| fail(&error_text(&e)))?;
        client
            .simple_query("SELECT 1")
            .map_err(|e| fail(&error_text(&e)))?;
        Ok(Self {
            label: format!(
                "{name} ({}@{}:{}/{})",
                cfg.user, cfg.host, cfg.port, cfg.database
            ),
            client,
            in_user_tx: false,
        })
    }

    pub fn label(&self) -> String {
        self.label.clone()
    }

    /// User tables and views; `public` ones bare, others `schema.table`.
    pub fn list_tables(&mut self) -> Result<Vec<String>, DbError> {
        let sql = format!(
            "SELECT CASE WHEN table_schema = 'public' THEN table_name::text \
             ELSE table_schema || '.' || table_name END \
             FROM information_schema.tables WHERE table_schema {USER_SCHEMAS_FILTER} ORDER BY 1"
        );
        let rows = self.client.query(&sql, &[]).map_err(pg_err)?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }

    /// Reconstructed DDL: `CREATE TABLE` (or view), then non-constraint indexes.
    pub fn get_schema(&mut self, table: &str) -> Result<Vec<String>, DbError> {
        let head = self
            .client
            .query_opt(
                "SELECT c.relkind::text, format('%I.%I', n.nspname, c.relname) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.oid = to_regclass($1)",
                &[&table],
            )
            .map_err(pg_err)?;
        let Some(head) = head else {
            return Err(DbError::NoSuchTable);
        };
        let kind: String = head.get(0);
        let qualified: String = head.get(1);

        if kind == "v" || kind == "m" {
            let def: String = self
                .client
                .query_one("SELECT pg_get_viewdef(to_regclass($1))", &[&table])
                .map_err(pg_err)?
                .get(0);
            let what = if kind == "m" {
                "MATERIALIZED VIEW"
            } else {
                "VIEW"
            };
            return Ok(vec![format!(
                "CREATE {what} {qualified} AS\n{}",
                def.trim_end().trim_end_matches(';')
            )]);
        }

        let mut parts: Vec<String> = Vec::new();
        let cols = self
            .client
            .query(
                "SELECT quote_ident(a.attname), format_type(a.atttypid, a.atttypmod), \
                        a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
                 FROM pg_attribute a \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&table],
            )
            .map_err(pg_err)?;
        for c in &cols {
            let mut line = format!("{} {}", c.get::<_, String>(0), c.get::<_, String>(1));
            if let Some(default) = c.get::<_, Option<String>>(3) {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            if c.get::<_, bool>(2) {
                line.push_str(" NOT NULL");
            }
            parts.push(line);
        }
        let constraints = self
            .client
            .query(
                "SELECT quote_ident(conname), pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE conrelid = to_regclass($1) ORDER BY contype DESC, conname",
                &[&table],
            )
            .map_err(pg_err)?;
        for c in &constraints {
            parts.push(format!(
                "CONSTRAINT {} {}",
                c.get::<_, String>(0),
                c.get::<_, String>(1)
            ));
        }
        let mut out = vec![format!(
            "CREATE TABLE {qualified} (\n    {}\n)",
            parts.join(",\n    ")
        )];
        let indexes = self
            .client
            .query(
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
                 WHERE i.indrelid = to_regclass($1) AND NOT EXISTS \
                   (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) \
                 ORDER BY 1",
                &[&table],
            )
            .map_err(pg_err)?;
        out.extend(indexes.iter().map(|r| r.get::<_, String>(0)));
        Ok(out)
    }

    /// TAB-completion names. Failures degrade to an empty cache.
    pub fn refresh_schema_cache(&mut self) -> SchemaCache {
        let tables = self.list_tables().unwrap_or_default();
        let sql = format!(
            "SELECT CASE WHEN table_schema = 'public' THEN table_name::text \
             ELSE table_schema || '.' || table_name END, column_name::text \
             FROM information_schema.columns \
             WHERE table_schema {USER_SCHEMAS_FILTER} ORDER BY 1, ordinal_position"
        );
        let rows = self.client.query(&sql, &[]).unwrap_or_default();
        let mut columns: Vec<String> = Vec::new();
        let mut table_columns: HashMap<String, Vec<String>> = HashMap::new();
        for r in &rows {
            let table: String = r.get(0);
            let col: String = r.get(1);
            if !columns.contains(&col) {
                columns.push(col.clone());
            }
            table_columns
                .entry(table.to_lowercase())
                .or_default()
                .push(col);
        }
        columns.sort();
        SchemaCache {
            tables,
            columns,
            table_columns,
        }
    }

    /// Fetch at most `MAX_ROWS` (+1 probe) through a server-side cursor so
    /// big tables are never pulled whole. Statements a cursor can't wrap
    /// (EXPLAIN, SHOW, data-modifying WITH, ...) run directly instead.
    pub fn query_select(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let (open, undo, close) = if self.in_user_tx {
            (
                format!("SAVEPOINT {SAVEPOINT}"),
                format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"),
                format!("CLOSE {CURSOR}; RELEASE SAVEPOINT {SAVEPOINT}"),
            )
        } else {
            (
                "BEGIN".to_string(),
                "ROLLBACK".to_string(),
                format!("CLOSE {CURSOR}; COMMIT"),
            )
        };
        self.client.simple_query(&open).map_err(pg_err)?;
        let declared = self
            .client
            .simple_query(&format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {sql}"));
        if declared.is_err() {
            self.client.simple_query(&undo).map_err(pg_err)?;
            if self.in_user_tx {
                self.client
                    .simple_query(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                    .map_err(pg_err)?;
            }
            let msgs = self.client.simple_query(sql).map_err(pg_err)?;
            self.track_tx(sql);
            return Ok(collect_result(msgs, MAX_ROWS));
        }
        let fetched = self
            .client
            .simple_query(&format!("FETCH {} FROM {CURSOR}", MAX_ROWS + 1));
        match fetched {
            Ok(msgs) => {
                self.client.simple_query(&close).map_err(pg_err)?;
                Ok(collect_result(msgs, MAX_ROWS))
            }
            Err(e) => {
                let _ = self.client.simple_query(&undo);
                if self.in_user_tx {
                    let _ = self
                        .client
                        .simple_query(&format!("RELEASE SAVEPOINT {SAVEPOINT}"));
                }
                Err(pg_err(e))
            }
        }
    }

    /// Run a non-SELECT statement; returns the server's affected-row count.
    pub fn execute_write(&mut self, sql: &str) -> Result<usize, DbError> {
        let msgs = self.client.simple_query(sql).map_err(pg_err)?;
        self.track_tx(sql);
        let n = msgs
            .iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::CommandComplete(n) => Some(*n),
                _ => None,
            })
            .last()
            .unwrap_or(0);
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    }

    fn track_tx(&mut self, sql: &str) {
        if let Some(state) = tx_state_after(sql) {
            self.in_user_tx = state;
        }
    }
}

/// `Some(true)` after BEGIN/START, `Some(false)` after COMMIT/ROLLBACK/END/ABORT
/// (but not `ROLLBACK TO SAVEPOINT`), `None` for anything else.
fn tx_state_after(sql: &str) -> Option<bool> {
    let words: Vec<String> = sql
        .split_whitespace()
        .take(2)
        .map(|w| w.trim_end_matches(';').to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some("BEGIN") => Some(true),
        Some("START") if words.get(1).map(String::as_str) == Some("TRANSACTION") => Some(true),
        Some("ROLLBACK") if words.get(1).map(String::as_str) == Some("TO") => None,
        Some("COMMIT" | "END" | "ROLLBACK" | "ABORT") => Some(false),
        _ => None,
    }
}

/// Build a grid from the first result set in simple-protocol messages.
fn collect_result(msgs: Vec<SimpleQueryMessage>, max_rows: usize) -> QueryResult {
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    for m in msgs {
        match m {
            SimpleQueryMessage::RowDescription(cols) if headers.is_empty() => {
                headers = cols.iter().map(|c| c.name().to_string()).collect();
            }
            SimpleQueryMessage::Row(row) => {
                if headers.is_empty() {
                    headers = row.columns().iter().map(|c| c.name().to_string()).collect();
                }
                if rows.len() >= max_rows {
                    truncated = true;
                    continue;
                }
                rows.push(
                    (0..row.len())
                        .map(|i| row.get(i).unwrap_or("NULL").to_string())
                        .collect(),
                );
            }
            SimpleQueryMessage::CommandComplete(_) if !headers.is_empty() => break,
            _ => {}
        }
    }
    QueryResult {
        headers,
        rows,
        truncated,
    }
}

/// One-line description of a driver error, preferring the server message.
fn error_text(e: &::postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        let mut s = db.message().to_string();
        if let Some(detail) = db.detail() {
            s.push_str(&format!(" ({detail})"));
        }
        s
    } else if e.is_closed() {
        "connection to the server was lost; restart sqlight to reconnect".to_string()
    } else {
        match std::error::Error::source(e) {
            Some(src) => format!("{e}: {src}"),
            None => e.to_string(),
        }
    }
}

fn pg_err(e: ::postgres::Error) -> DbError {
    DbError::Other(format!("Error: {}", error_text(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_tracking_keywords() {
        assert_eq!(tx_state_after("begin"), Some(true));
        assert_eq!(tx_state_after("START TRANSACTION"), Some(true));
        assert_eq!(tx_state_after("commit"), Some(false));
        assert_eq!(tx_state_after("end;"), Some(false));
        assert_eq!(tx_state_after("rollback"), Some(false));
        assert_eq!(tx_state_after("ROLLBACK TO SAVEPOINT a"), None);
        assert_eq!(tx_state_after("select 1"), None);
    }

    /// Needs a live server. Run with
    /// `SQLIGHT_TEST_PG="host=localhost port=5432 dbname=postgres user=postgres password=..."
    /// cargo test -- --ignored`.
    fn test_client() -> Option<PgDb> {
        let spec = std::env::var("SQLIGHT_TEST_PG").ok()?;
        let get = |k: &str| {
            spec.split_whitespace()
                .find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(str::to_string))
        };
        let cfg = PgConfig {
            host: get("host").unwrap_or_else(|| "localhost".into()),
            port: get("port").and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: get("dbname").unwrap_or_else(|| "postgres".into()),
            user: get("user").unwrap_or_else(|| "postgres".into()),
            password: None,
        };
        let password = get("password").unwrap_or_default();
        Some(PgDb::connect("test", &cfg, &password).expect("connect"))
    }

    #[test]
    #[ignore = "requires SQLIGHT_TEST_PG"]
    fn live_roundtrip() {
        let Some(mut db) = test_client() else {
            return;
        };
        let t = format!("sqlight_it_{}", std::process::id());
        db.execute_write(&format!("DROP TABLE IF EXISTS {t}"))
            .unwrap();
        db.execute_write(&format!(
            "CREATE TABLE {t} (id serial PRIMARY KEY, name text NOT NULL)"
        ))
        .unwrap();
        db.execute_write(&format!("CREATE INDEX {t}_name ON {t}(name)"))
            .unwrap();
        let n = db
            .execute_write(&format!(
                "INSERT INTO {t} (name) SELECT 'n' || g FROM generate_series(1, {}) g",
                MAX_ROWS + 5
            ))
            .unwrap();
        assert_eq!(n, MAX_ROWS + 5);

        assert!(db.list_tables().unwrap().contains(&t));
        let schema = db.get_schema(&t).unwrap();
        assert!(schema[0].contains("CREATE TABLE") && schema[0].contains("PRIMARY KEY"));
        assert!(schema.iter().any(|s| s.contains(&format!("{t}_name"))));
        assert_eq!(db.get_schema("sqlight_nope_xyz"), Err(DbError::NoSuchTable));

        let res = db.query_select(&format!("SELECT * FROM {t}")).unwrap();
        assert_eq!(res.headers, vec!["id", "name"]);
        assert_eq!(res.rows.len(), MAX_ROWS);
        assert!(res.truncated);
        let res = db.query_select("SELECT NULL AS n").unwrap();
        assert_eq!(res.rows[0][0], "NULL");
        let empty = db
            .query_select(&format!("SELECT * FROM {t} WHERE false"))
            .unwrap();
        assert_eq!(empty.headers.len(), 2);
        assert!(empty.rows.is_empty());
        assert!(
            !db.query_select("SHOW server_version")
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(matches!(
            db.query_select("SELECT * FROM sqlight_nope_xyz"),
            Err(DbError::Other(_))
        ));

        // A user transaction survives our cursor bookkeeping.
        db.execute_write("BEGIN").unwrap();
        db.execute_write(&format!("DELETE FROM {t}")).unwrap();
        db.query_select(&format!("SELECT * FROM {t}")).unwrap();
        db.execute_write("ROLLBACK").unwrap();
        let back = db
            .query_select(&format!("SELECT count(*) FROM {t}"))
            .unwrap();
        assert_eq!(back.rows[0][0], (MAX_ROWS + 5).to_string());

        db.execute_write(&format!("DROP TABLE {t}")).unwrap();
    }
}
