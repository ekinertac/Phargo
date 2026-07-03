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

/// MySQL-compat scalar functions the WordPress SQLite plugin expects to
/// register via PDO::sqliteCreateFunction. We provide them natively at
/// connection-open instead (the prelude's sqliteCreateFunction is a no-op),
/// sidestepping PHP-callback reentrancy into the evaluator.
fn register_mysql_shims(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    let f = FunctionFlags::SQLITE_UTF8;
    let det = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    fn ts_of(s: &str) -> i64 {
        crate::php_strtotime(s, crate::now_unix()).unwrap_or(0)
    }
    fn datepart(s: &str, fmt: &str) -> i64 {
        crate::php_date(fmt, ts_of(s)).parse().unwrap_or(0)
    }

    conn.create_scalar_function("month", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "n"))
    })?;
    conn.create_scalar_function("monthnum", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "n"))
    })?;
    conn.create_scalar_function("year", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "Y"))
    })?;
    conn.create_scalar_function("day", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "j"))
    })?;
    conn.create_scalar_function("dayofmonth", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "j"))
    })?;
    conn.create_scalar_function("hour", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "G"))
    })?;
    conn.create_scalar_function("minute", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "i"))
    })?;
    conn.create_scalar_function("second", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "s"))
    })?;
    conn.create_scalar_function("week", 1, det, |c| {
        Ok(datepart(&c.get::<String>(0)?, "W"))
    })?;
    conn.create_scalar_function("weekday", 1, det, |c| {
        // MySQL WEEKDAY: 0=Monday
        Ok((datepart(&c.get::<String>(0)?, "N") - 1).max(0))
    })?;
    conn.create_scalar_function("dayofweek", 1, det, |c| {
        // MySQL DAYOFWEEK: 1=Sunday
        Ok(datepart(&c.get::<String>(0)?, "w") + 1)
    })?;
    conn.create_scalar_function("unix_timestamp", 0, f, |_| Ok(crate::now_unix()))?;
    conn.create_scalar_function("unix_timestamp", 1, det, |c| {
        Ok(ts_of(&c.get::<String>(0)?))
    })?;
    conn.create_scalar_function("now", 0, f, |_| {
        Ok(crate::php_date("Y-m-d H:i:s", crate::now_unix()))
    })?;
    conn.create_scalar_function("curdate", 0, f, |_| {
        Ok(crate::php_date("Y-m-d", crate::now_unix()))
    })?;
    conn.create_scalar_function("utc_date", 0, f, |_| {
        Ok(crate::php_date("Y-m-d", crate::now_unix()))
    })?;
    conn.create_scalar_function("utc_time", 0, f, |_| {
        Ok(crate::php_date("H:i:s", crate::now_unix()))
    })?;
    conn.create_scalar_function("utc_timestamp", 0, f, |_| {
        Ok(crate::php_date("Y-m-d H:i:s", crate::now_unix()))
    })?;
    conn.create_scalar_function("from_unixtime", 1, det, |c| {
        Ok(crate::php_date("Y-m-d H:i:s", c.get::<i64>(0)?))
    })?;
    conn.create_scalar_function("datediff", 2, det, |c| {
        let a = ts_of(&c.get::<String>(0)?);
        let b = ts_of(&c.get::<String>(1)?);
        Ok(a.div_euclid(86400) - b.div_euclid(86400))
    })?;
    conn.create_scalar_function("md5", 1, det, |c| {
        Ok(crate::md5_hex(c.get::<String>(0)?.as_bytes()))
    })?;
    conn.create_scalar_function("rand", 0, f, |_| {
        // deterministic-ish is fine for WP's uses
        Ok((crate::now_unix() % 1000) as f64 / 1000.0)
    })?;
    conn.create_scalar_function("isnull", 1, det, |c| {
        Ok(matches!(c.get_raw(0), rusqlite::types::ValueRef::Null) as i64)
    })?;
    conn.create_scalar_function("if", 3, det, |c| {
        use rusqlite::types::{Value as SV, ValueRef};
        let cond = match c.get_raw(0) {
            ValueRef::Null => false,
            ValueRef::Integer(n) => n != 0,
            ValueRef::Real(r) => r != 0.0,
            ValueRef::Text(t) => !t.is_empty() && t != b"0",
            ValueRef::Blob(b) => !b.is_empty(),
        };
        Ok(SV::from(c.get_raw(if cond { 1 } else { 2 })))
    })?;
    conn.create_scalar_function("regexp", 2, det, |c| {
        let pat = c.get::<String>(0)?;
        let subj = c.get::<String>(1)?;
        // MySQL REGEXP is case-insensitive by default
        let full = format!("/{}/i", pat.replace('/', "\\/"));
        let hit = crate::rx_compile(&full)
            .map(|rx| {
                let chars: Vec<char> = subj.chars().collect();
                let mut steps = 0usize;
                (0..=chars.len()).any(|st| rx.exec(&chars, st, &mut steps).is_some())
            })
            .unwrap_or(false);
        Ok(hit as i64)
    })?;
    conn.create_scalar_function("field", -1, det, |c| {
        let needle = c.get::<String>(0)?;
        for i in 1..c.len() {
            if c.get::<String>(i)? == needle {
                return Ok(i as i64);
            }
        }
        Ok(0i64)
    })?;
    conn.create_scalar_function("least", -1, det, |c| {
        let mut best: Option<f64> = None;
        for i in 0..c.len() {
            let v = c.get::<f64>(i)?;
            best = Some(best.map_or(v, |b: f64| b.min(v)));
        }
        Ok(best.unwrap_or(0.0))
    })?;
    conn.create_scalar_function("greatest", -1, det, |c| {
        let mut best: Option<f64> = None;
        for i in 0..c.len() {
            let v = c.get::<f64>(i)?;
            best = Some(best.map_or(v, |b: f64| b.max(v)));
        }
        Ok(best.unwrap_or(0.0))
    })?;
    conn.create_scalar_function("ucase", 1, det, |c| {
        Ok(c.get::<String>(0)?.to_uppercase())
    })?;
    conn.create_scalar_function("lcase", 1, det, |c| {
        Ok(c.get::<String>(0)?.to_lowercase())
    })?;
    conn.create_scalar_function("locate", 2, det, |c| {
        let needle = c.get::<String>(0)?;
        let hay = c.get::<String>(1)?;
        Ok(hay.find(&needle).map(|i| i as i64 + 1).unwrap_or(0))
    })?;
    conn.create_scalar_function("get_lock", 2, f, |_| Ok(1i64))?;
    conn.create_scalar_function("release_lock", 1, f, |_| Ok(1i64))?;
    conn.create_scalar_function("version", 0, f, |_| Ok("8.0.35".to_string()))?;
    Ok(())
}

/// Open a database ("": empty → :memory:; ":memory:"; or a file path).
pub fn open(path: &str) -> Result<i64, String> {
    let conn = if path.is_empty() || path == ":memory:" {
        Connection::open_in_memory()
    } else {
        Connection::open(path)
    }
    .map_err(|e| e.to_string())?;
    register_mysql_shims(&conn).map_err(|e| e.to_string())?;
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
    params: Vec<(Option<String>, SqlVal)>,
) -> Result<(Vec<String>, Vec<Vec<SqlVal>>, usize), String> {
    CONNS.with(|c| {
        let conns = c.borrow();
        let conn = conns.get(&id).ok_or("no such connection")?;
        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let ncols = cols.len();
        // bind: named parameters (":param0" — the SQLite plugin's translator
        // emits these) via parameter_index; positional by order
        let named = params.iter().any(|(n, _)| n.is_some());
        if named {
            for (n, v) in &params {
                if let Some(n) = n {
                    let name = if n.starts_with(':') { n.clone() } else { format!(":{n}") };
                    if let Ok(Some(idx)) = stmt.parameter_index(&name) {
                        stmt.raw_bind_parameter(idx, v).map_err(|e| e.to_string())?;
                    }
                }
            }
        } else {
            for (i, (_, v)) in params.iter().enumerate() {
                stmt.raw_bind_parameter(i + 1, v).map_err(|e| e.to_string())?;
            }
        }
        if ncols == 0 {
            // DML / DDL
            let affected = stmt.raw_execute().map_err(|e| e.to_string())?;
            return Ok((cols, Vec::new(), affected));
        }
        let mut rows_out: Vec<Vec<SqlVal>> = Vec::new();
        let mut rows = stmt.raw_query();
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
