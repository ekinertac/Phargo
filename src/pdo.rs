//! SQLite bridge — the storage layer behind the prelude's PDO / PDOStatement
//! (and later SQLite3) classes. The one place in the engine that touches the
//! one permitted native dependency (rusqlite, bundled; see docs/ROADMAP.md,
//! decision 2026-07-07).
//!
//! Design: connections live in a thread-local registry keyed by handle id
//! (the evaluator is single-threaded per run). Statements are NOT cached
//! across calls — `query` prepares, binds positionally, and buffers all rows,
//! which sidesteps rusqlite's statement-borrows-connection lifetime and is
//! plenty for WordPress-scale workloads. eval.rs exposes this via the
//! `__pdo_*` internal builtins.

use rusqlite::Connection;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static CONNS: RefCell<HashMap<i64, Connection>> = RefCell::new(HashMap::new());
    static NEXT_ID: RefCell<i64> = const { RefCell::new(1) };
}

/// A SQLite value crossing the bridge (maps 1:1 onto engine Values in eval.rs).
pub enum SqlVal {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl rusqlite::ToSql for SqlVal {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(match self {
            SqlVal::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            SqlVal::Int(n) => ToSqlOutput::Borrowed(ValueRef::Integer(*n)),
            SqlVal::Real(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            SqlVal::Text(t) => ToSqlOutput::Borrowed(ValueRef::Text(t)),
            SqlVal::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
        })
    }
}

fn from_ref(v: rusqlite::types::ValueRef<'_>) -> SqlVal {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => SqlVal::Null,
        ValueRef::Integer(n) => SqlVal::Int(n),
        ValueRef::Real(f) => SqlVal::Real(f),
        ValueRef::Text(t) => SqlVal::Text(t.to_vec()),
        ValueRef::Blob(b) => SqlVal::Blob(b.to_vec()),
    }
}

/// Open a database ("": empty → :memory:; ":memory:"; or a file path).
pub fn open(path: &str) -> Result<i64, String> {
    let conn = if path.is_empty() || path == ":memory:" {
        Connection::open_in_memory()
    } else {
        Connection::open(path)
    }
    .map_err(|e| e.to_string())?;
    CONNS.with(|c| {
        let id = NEXT_ID.with(|n| {
            let id = *n.borrow();
            *n.borrow_mut() = id + 1;
            id
        });
        c.borrow_mut().insert(id, conn);
        Ok(id)
    })
}

pub fn close(id: i64) -> bool {
    CONNS.with(|c| c.borrow_mut().remove(&id).is_some())
}

/// Close everything — called between scoreboard tests so handles never leak
/// across runs.
pub fn reset() {
    CONNS.with(|c| c.borrow_mut().clear());
    NEXT_ID.with(|n| *n.borrow_mut() = 1);
}

/// Execute a statement, returning (column names, buffered rows, affected).
/// Works for SELECT (rows non-empty-able) and DML (affected set) alike.
pub fn query(
    id: i64,
    sql: &str,
    params: Vec<SqlVal>,
) -> Result<(Vec<String>, Vec<Vec<SqlVal>>, usize), String> {
    CONNS.with(|c| {
        let conns = c.borrow();
        let conn = conns.get(&id).ok_or("no such connection")?;
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let ncols = cols.len();
        if ncols == 0 {
            // DML / DDL
            let affected = stmt
                .execute(rusqlite::params_from_iter(params.iter()))
                .map_err(|e| e.to_string())?;
            return Ok((cols, Vec::new(), affected));
        }
        let mut rows_out: Vec<Vec<SqlVal>> = Vec::new();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut r = Vec::with_capacity(ncols);
            for i in 0..ncols {
                r.push(from_ref(row.get_ref(i).map_err(|e| e.to_string())?));
            }
            rows_out.push(r);
            if rows_out.len() > 500_000 {
                return Err("result set too large".into());
            }
        }
        let affected = conn.changes() as usize;
        Ok((cols, rows_out, affected))
    })
}

pub fn last_insert_id(id: i64) -> i64 {
    CONNS.with(|c| c.borrow().get(&id).map(|conn| conn.last_insert_rowid()).unwrap_or(0))
}
