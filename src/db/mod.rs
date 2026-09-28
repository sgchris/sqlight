//! Database backends behind one `Database` handle used by `App`.

pub mod postgres;
pub mod sqlite;

use std::collections::HashMap;
use std::path::PathBuf;

pub use self::postgres::PgDb;

/// Cached schema names used for autocompletion.
#[derive(Debug, Clone, Default)]
pub struct SchemaCache {
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    /// Lowercased table name -> its column names.
    pub table_columns: HashMap<String, Vec<String>>,
}

/// Grid result for SELECT-family statements.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// True when more rows existed than `MAX_ROWS` and output was cut.
    pub truncated: bool,
    /// Value type per cell, parallel to `rows`. Missing entries are
    /// treated as `CellKind::Untyped`.
    pub kinds: Vec<Vec<CellKind>>,
}

/// Source type of one result cell (drives JSON rendering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
    /// Text of unknown type (PostgreSQL simple protocol); inferred on use.
    Untyped,
}

/// Backend-neutral failure. `Other` holds a ready `Error: ...` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    NoSuchTable,
    Other(String),
}

impl DbError {
    /// User-facing one-line message.
    pub fn message(&self) -> String {
        match self {
            DbError::NoSuchTable => "Error: no such table".to_string(),
            DbError::Other(m) => m.clone(),
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => DbError::NoSuchTable,
            e => DbError::Other(sqlite::friendly_query_error(&e)),
        }
    }
}

/// An open database: a SQLite file (opened per statement) or a live
/// PostgreSQL session.
pub enum Database {
    Sqlite(PathBuf),
    Postgres(Box<PgDb>),
}

impl Database {
    /// Short description for the status bar. Never contains secrets.
    pub fn label(&self) -> String {
        match self {
            Database::Sqlite(p) => p.to_string_lossy().into_owned(),
            Database::Postgres(pg) => pg.label(),
        }
    }

    pub fn list_tables(&mut self) -> Result<Vec<String>, DbError> {
        match self {
            Database::Sqlite(p) => Ok(sqlite::list_tables(p)?),
            Database::Postgres(pg) => pg.list_tables(),
        }
    }

    pub fn get_schema(&mut self, table: &str) -> Result<Vec<String>, DbError> {
        match self {
            Database::Sqlite(p) => Ok(sqlite::get_schema(p, table)?),
            Database::Postgres(pg) => pg.get_schema(table),
        }
    }

    pub fn refresh_schema_cache(&mut self) -> SchemaCache {
        match self {
            Database::Sqlite(p) => sqlite::refresh_schema_cache(p),
            Database::Postgres(pg) => pg.refresh_schema_cache(),
        }
    }

    pub fn query_select(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Database::Sqlite(p) => Ok(sqlite::query_select(p, sql)?),
            Database::Postgres(pg) => pg.query_select(sql),
        }
    }

    pub fn execute_write(&mut self, sql: &str) -> Result<usize, DbError> {
        match self {
            Database::Sqlite(p) => Ok(sqlite::execute_write(p, sql)?),
            Database::Postgres(pg) => pg.execute_write(sql),
        }
    }
}
