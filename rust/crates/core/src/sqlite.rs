//! Shared SQLite machinery for the family's native store engines (`corrections`,
//! `feedback`): the CPython-faithful error taxonomy (`LedgerError` / `SqliteErrorClass`),
//! the storage-class row projection (`SqlCell` / `SqlRow`), and the single-statement
//! `sql()` rule. Unlike `corrections`' param-less escape hatch, `query_rows` /
//! `exec_changes` bind a `&[SqlCell]` (blobs included) so `feedback` can cross vectors.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;

use rusqlite::types::ValueRef;
use rusqlite::{ffi, Connection, Params, Row};

/// The Python `sqlite3` exception class an error maps to, chosen so the py binding raises
/// the same type callers branch on without linking rusqlite.
#[derive(Debug, Clone, Copy)]
pub enum SqliteErrorClass {
    Operational,
    Integrity,
    Programming,
    Interface,
    Internal,
    Data,
    Database,
    Memory,
}

impl SqliteErrorClass {
    // Memory maps to the builtin MemoryError (raised by the binding, not looked up in sqlite3).
    pub fn name(self) -> &'static str {
        match self {
            SqliteErrorClass::Operational => "OperationalError",
            SqliteErrorClass::Integrity => "IntegrityError",
            SqliteErrorClass::Programming => "ProgrammingError",
            SqliteErrorClass::Interface => "InterfaceError",
            SqliteErrorClass::Internal => "InternalError",
            SqliteErrorClass::Data => "DataError",
            SqliteErrorClass::Database => "DatabaseError",
            SqliteErrorClass::Memory => "MemoryError",
        }
    }
}

/// A store failure, pre-classified for the py binding's exception mapping. `code`/`name`
/// carry the SQLite extended result code and symbolic name for a genuine SQLite failure,
/// and are None for a Python-side error (multi-statement, UTF-8 decode, NUL byte).
/// `VerdictSchema` is the feedback engine's open-time v8/v9 guard.
#[derive(Debug)]
pub enum LedgerError {
    Io {
        error: std::io::Error,
        path: std::path::PathBuf,
    },
    Sqlite {
        class: SqliteErrorClass,
        message: String,
        code: Option<i32>,
        name: Option<String>,
    },
    MultipleStatements,
    VerdictSchema {
        message: String,
    },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io { error, path } => write!(f, "{}: {error}", path.display()),
            LedgerError::Sqlite { message, .. } => write!(f, "{message}"),
            LedgerError::MultipleStatements => {
                write!(f, "You can only execute one statement at a time.")
            }
            LedgerError::VerdictSchema { message } => write!(f, "{message}"),
        }
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(inner, message) => sqlite_error(
                inner.extended_code,
                message.unwrap_or_else(|| inner.to_string()),
            ),
            other => LedgerError::Sqlite {
                class: SqliteErrorClass::Database,
                message: other.to_string(),
                code: None,
                name: None,
            },
        }
    }
}

/// One cell of a read row, mapped from SQLite's dynamic storage class so callers project
/// it exactly as Python's `row[col]` does and never link rusqlite.
#[derive(Debug, Clone)]
pub enum SqlCell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One read row: its columns in select order.
pub type SqlRow = Vec<(String, SqlCell)>;

/// Runs a prepared statement with typed `params`, projecting rows by storage class.
pub fn run_query(
    conn: &Connection,
    sql: &str,
    params: impl Params,
) -> Result<Vec<SqlRow>, LedgerError> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| Ok((name.clone(), cell(row, i, name)?)))
                .collect::<Result<SqlRow, LedgerError>>()?,
        );
    }
    Ok(out)
}

/// The escape-hatch `sql()` behind `corrections sql` / the feedback engine's `sql`.
/// Mirrors `sqlite3.Cursor.execute`: exactly one statement (a trailing statement raises
/// before anything runs; a trailing comment is fine), comment/whitespace-only SQL yields
/// no rows. `params` bind to the statement's placeholders (`corrections` passes none).
pub fn query_rows(
    conn: &Connection,
    statement: &str,
    params: &[SqlCell],
) -> Result<Vec<SqlRow>, LedgerError> {
    let db = unsafe { conn.handle() };
    match prepare_single(db, statement)? {
        None => Ok(Vec::new()),
        Some(handle) => {
            bind_params(handle.0, params)?;
            read_rows(db, handle.0)
        }
    }
}

/// The write counterpart of `query_rows`: runs one bound statement to completion and
/// returns `sqlite3_changes` — the row count the last statement modified.
pub fn exec_changes(
    conn: &Connection,
    statement: &str,
    params: &[SqlCell],
) -> Result<i64, LedgerError> {
    let db = unsafe { conn.handle() };
    match prepare_single(db, statement)? {
        None => Ok(0),
        Some(handle) => {
            bind_params(handle.0, params)?;
            step_to_done(db, handle.0)?;
            Ok(unsafe { ffi::sqlite3_changes(db) } as i64)
        }
    }
}

// Prepares only the first statement under the single-statement rule; None when nothing
// meaningful remains. A trailing statement past the first raises before the head runs.
fn prepare_single(
    db: *mut ffi::sqlite3,
    statement: &str,
) -> Result<Option<StatementHandle>, LedgerError> {
    let c_sql = CString::new(statement).map_err(|_| LedgerError::Sqlite {
        class: SqliteErrorClass::Programming,
        message: "the query contains a null character".to_string(),
        code: None,
        name: None,
    })?;
    let mut raw: *mut ffi::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe { ffi::sqlite3_prepare_v2(db, c_sql.as_ptr(), -1, &mut raw, &mut tail) };
    if rc != ffi::SQLITE_OK {
        return Err(sqlite_error_from_db(db));
    }
    let handle = StatementHandle(raw);
    if raw.is_null() {
        return Ok(None);
    }
    if !unsafe { lstrip_sql(tail) }.is_null() {
        return Err(LedgerError::MultipleStatements);
    }
    Ok(Some(handle))
}

fn bind_params(stmt: *mut ffi::sqlite3_stmt, params: &[SqlCell]) -> Result<(), LedgerError> {
    check_bind_arity(
        unsafe { ffi::sqlite3_bind_parameter_count(stmt) } as usize,
        params.len(),
    )?;
    for (offset, param) in params.iter().enumerate() {
        let index = (offset + 1) as c_int;
        let rc = match param {
            SqlCell::Null => unsafe { ffi::sqlite3_bind_null(stmt, index) },
            SqlCell::Int(v) => unsafe { ffi::sqlite3_bind_int64(stmt, index, *v) },
            SqlCell::Real(f) => unsafe { ffi::sqlite3_bind_double(stmt, index, *f) },
            SqlCell::Text(s) => unsafe {
                ffi::sqlite3_bind_text(
                    stmt,
                    index,
                    s.as_ptr() as *const c_char,
                    bind_len(s.len())?,
                    ffi::SQLITE_TRANSIENT(),
                )
            },
            SqlCell::Blob(b) => unsafe {
                ffi::sqlite3_bind_blob(
                    stmt,
                    index,
                    b.as_ptr() as *const std::os::raw::c_void,
                    bind_len(b.len())?,
                    ffi::SQLITE_TRANSIENT(),
                )
            },
        };
        if rc != ffi::SQLITE_OK {
            return Err(sqlite_error_from_db(unsafe {
                ffi::sqlite3_db_handle(stmt)
            }));
        }
    }
    Ok(())
}

// Parity: pysqlite bind_parameters — a placeholder/param count mismatch raises
// ProgrammingError before anything binds.
fn check_bind_arity(expected: usize, supplied: usize) -> Result<(), LedgerError> {
    if expected == supplied {
        return Ok(());
    }
    Err(LedgerError::Sqlite {
        class: SqliteErrorClass::Programming,
        message: format!(
            "Incorrect number of bindings supplied. The current statement uses {expected}, \
             and there are {supplied} supplied."
        ),
        code: None,
        name: None,
    })
}

// Parity: pysqlite's oversized bind — a length past c_int raises DataError("string or
// blob too big"), never the wrapped-negative UB of sqlite3_bind_text/blob.
fn bind_len(len: usize) -> Result<c_int, LedgerError> {
    c_int::try_from(len)
        .map_err(|_| sqlite_error(ffi::SQLITE_TOOBIG, "string or blob too big".to_string()))
}

// Finalizes a raw ffi statement on every exit path.
struct StatementHandle(*mut ffi::sqlite3_stmt);

impl Drop for StatementHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::sqlite3_finalize(self.0) };
        }
    }
}

fn cell(row: &Row, index: usize, name: &str) -> Result<SqlCell, LedgerError> {
    // get_ref borrows the column, so invalid UTF-8 surfaces as an error here rather than
    // panicking the way rusqlite's ValueRef->Value String conversion would.
    match row.get_ref(index)? {
        ValueRef::Null => Ok(SqlCell::Null),
        ValueRef::Integer(i) => Ok(SqlCell::Int(i)),
        ValueRef::Real(f) => Ok(SqlCell::Real(f)),
        ValueRef::Text(bytes) => text_cell(bytes, name),
        ValueRef::Blob(bytes) => Ok(SqlCell::Blob(bytes.to_vec())),
    }
}

// Parity: sqlite3's UTF-8 decode of a TEXT column — invalid bytes raise OperationalError
// with the column name and the lossy-decoded text, never a panic.
fn text_cell(bytes: &[u8], name: &str) -> Result<SqlCell, LedgerError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(SqlCell::Text(text.to_string())),
        Err(_) => Err(LedgerError::Sqlite {
            class: SqliteErrorClass::Operational,
            message: format!(
                "Could not decode to UTF-8 column '{name}' with text '{}'",
                String::from_utf8_lossy(bytes)
            ),
            code: None,
            name: None,
        }),
    }
}

fn step_to_done(db: *mut ffi::sqlite3, stmt: *mut ffi::sqlite3_stmt) -> Result<(), LedgerError> {
    loop {
        match unsafe { ffi::sqlite3_step(stmt) } {
            ffi::SQLITE_ROW => {}
            ffi::SQLITE_DONE => return Ok(()),
            _ => return Err(sqlite_error_from_db(db)),
        }
    }
}

fn read_rows(
    db: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
) -> Result<Vec<SqlRow>, LedgerError> {
    let count = unsafe { ffi::sqlite3_column_count(stmt) };
    let names: Vec<String> = (0..count)
        .map(|i| {
            let ptr = unsafe { ffi::sqlite3_column_name(stmt, i) };
            if ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .collect();
    let mut out = Vec::new();
    loop {
        match unsafe { ffi::sqlite3_step(stmt) } {
            ffi::SQLITE_ROW => out.push(
                (0..count)
                    .map(|i| {
                        Ok((
                            names[i as usize].clone(),
                            column_cell(stmt, i, &names[i as usize])?,
                        ))
                    })
                    .collect::<Result<SqlRow, LedgerError>>()?,
            ),
            ffi::SQLITE_DONE => return Ok(out),
            _ => return Err(sqlite_error_from_db(db)),
        }
    }
}

fn column_cell(
    stmt: *mut ffi::sqlite3_stmt,
    index: c_int,
    name: &str,
) -> Result<SqlCell, LedgerError> {
    match unsafe { ffi::sqlite3_column_type(stmt, index) } {
        ffi::SQLITE_NULL => Ok(SqlCell::Null),
        ffi::SQLITE_INTEGER => Ok(SqlCell::Int(unsafe {
            ffi::sqlite3_column_int64(stmt, index)
        })),
        ffi::SQLITE_FLOAT => Ok(SqlCell::Real(unsafe {
            ffi::sqlite3_column_double(stmt, index)
        })),
        ffi::SQLITE_TEXT => text_cell(column_bytes(stmt, index, false), name),
        _ => Ok(SqlCell::Blob(column_bytes(stmt, index, true).to_vec())),
    }
}

// The column's raw bytes. sqlite3_column_text must be read before sqlite3_column_bytes so
// the byte count reflects the UTF-8 text; blob reads the blob pointer instead.
fn column_bytes<'a>(stmt: *mut ffi::sqlite3_stmt, index: c_int, blob: bool) -> &'a [u8] {
    let ptr = if blob {
        (unsafe { ffi::sqlite3_column_blob(stmt, index) }) as *const u8
    } else {
        unsafe { ffi::sqlite3_column_text(stmt, index) }
    };
    let len = unsafe { ffi::sqlite3_column_bytes(stmt, index) } as usize;
    if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

// Port of CPython _sqlite `lstrip_sql`: the first meaningful character past leading
// whitespace and SQL comments (-- line, /* */ block), or null if none remains.
unsafe fn lstrip_sql(sql: *const c_char) -> *const c_char {
    let mut pos = sql;
    while unsafe { *pos } != 0 {
        match unsafe { *pos } as u8 {
            b' ' | b'\t' | b'\x0c' | b'\n' | b'\r' => {}
            b'-' if unsafe { *pos.add(1) } as u8 == b'-' => {
                pos = unsafe { pos.add(2) };
                while unsafe { *pos } != 0 && unsafe { *pos } as u8 != b'\n' {
                    pos = unsafe { pos.add(1) };
                }
                if unsafe { *pos } == 0 {
                    return ptr::null();
                }
            }
            b'/' if unsafe { *pos.add(1) } as u8 == b'*' => {
                pos = unsafe { pos.add(2) };
                while unsafe { *pos } != 0
                    && !(unsafe { *pos } as u8 == b'*' && unsafe { *pos.add(1) } as u8 == b'/')
                {
                    pos = unsafe { pos.add(1) };
                }
                if unsafe { *pos } == 0 {
                    return ptr::null();
                }
                pos = unsafe { pos.add(1) };
            }
            _ => return pos,
        }
        pos = unsafe { pos.add(1) };
    }
    ptr::null()
}

pub fn sqlite_error_from_db(db: *mut ffi::sqlite3) -> LedgerError {
    let extended = unsafe { ffi::sqlite3_extended_errcode(db) };
    let message = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned();
    sqlite_error(extended, message)
}

pub fn sqlite_error(extended: i32, message: String) -> LedgerError {
    LedgerError::Sqlite {
        class: class_for(extended & 0xff),
        message,
        code: Some(extended),
        name: error_name(extended),
    }
}

// Parity: pysqlite's get_exception_class over the primary result code.
fn class_for(primary: i32) -> SqliteErrorClass {
    match primary {
        ffi::SQLITE_INTERNAL | ffi::SQLITE_NOTFOUND => SqliteErrorClass::Internal,
        ffi::SQLITE_NOMEM => SqliteErrorClass::Memory,
        ffi::SQLITE_ERROR
        | ffi::SQLITE_PERM
        | ffi::SQLITE_ABORT
        | ffi::SQLITE_BUSY
        | ffi::SQLITE_LOCKED
        | ffi::SQLITE_READONLY
        | ffi::SQLITE_INTERRUPT
        | ffi::SQLITE_IOERR
        | ffi::SQLITE_FULL
        | ffi::SQLITE_CANTOPEN
        | ffi::SQLITE_PROTOCOL
        | ffi::SQLITE_EMPTY
        | ffi::SQLITE_SCHEMA => SqliteErrorClass::Operational,
        ffi::SQLITE_TOOBIG => SqliteErrorClass::Data,
        ffi::SQLITE_CONSTRAINT | ffi::SQLITE_MISMATCH => SqliteErrorClass::Integrity,
        ffi::SQLITE_MISUSE | ffi::SQLITE_RANGE => SqliteErrorClass::Interface,
        _ => SqliteErrorClass::Database,
    }
}

// The store's extended constraint codes resolve exactly; any other extended code falls
// back to its primary name. A None becomes "unknown" in the binding, matching CPython.
pub fn error_name(extended: i32) -> Option<String> {
    let name = match extended {
        ffi::SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK",
        ffi::SQLITE_CONSTRAINT_COMMITHOOK => "SQLITE_CONSTRAINT_COMMITHOOK",
        ffi::SQLITE_CONSTRAINT_DATATYPE => "SQLITE_CONSTRAINT_DATATYPE",
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY",
        ffi::SQLITE_CONSTRAINT_FUNCTION => "SQLITE_CONSTRAINT_FUNCTION",
        ffi::SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL",
        ffi::SQLITE_CONSTRAINT_PINNED => "SQLITE_CONSTRAINT_PINNED",
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY",
        ffi::SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID",
        ffi::SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER",
        ffi::SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
        ffi::SQLITE_CONSTRAINT_VTAB => "SQLITE_CONSTRAINT_VTAB",
        _ => return primary_name(extended & 0xff),
    };
    Some(name.to_string())
}

fn primary_name(primary: i32) -> Option<String> {
    let name = match primary {
        ffi::SQLITE_ERROR => "SQLITE_ERROR",
        ffi::SQLITE_INTERNAL => "SQLITE_INTERNAL",
        ffi::SQLITE_PERM => "SQLITE_PERM",
        ffi::SQLITE_ABORT => "SQLITE_ABORT",
        ffi::SQLITE_BUSY => "SQLITE_BUSY",
        ffi::SQLITE_LOCKED => "SQLITE_LOCKED",
        ffi::SQLITE_NOMEM => "SQLITE_NOMEM",
        ffi::SQLITE_READONLY => "SQLITE_READONLY",
        ffi::SQLITE_INTERRUPT => "SQLITE_INTERRUPT",
        ffi::SQLITE_IOERR => "SQLITE_IOERR",
        ffi::SQLITE_CORRUPT => "SQLITE_CORRUPT",
        ffi::SQLITE_NOTFOUND => "SQLITE_NOTFOUND",
        ffi::SQLITE_FULL => "SQLITE_FULL",
        ffi::SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
        ffi::SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
        ffi::SQLITE_EMPTY => "SQLITE_EMPTY",
        ffi::SQLITE_SCHEMA => "SQLITE_SCHEMA",
        ffi::SQLITE_TOOBIG => "SQLITE_TOOBIG",
        ffi::SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
        ffi::SQLITE_MISMATCH => "SQLITE_MISMATCH",
        ffi::SQLITE_MISUSE => "SQLITE_MISUSE",
        ffi::SQLITE_NOLFS => "SQLITE_NOLFS",
        ffi::SQLITE_AUTH => "SQLITE_AUTH",
        ffi::SQLITE_FORMAT => "SQLITE_FORMAT",
        ffi::SQLITE_RANGE => "SQLITE_RANGE",
        ffi::SQLITE_NOTADB => "SQLITE_NOTADB",
        ffi::SQLITE_NOTICE => "SQLITE_NOTICE",
        ffi::SQLITE_WARNING => "SQLITE_WARNING",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_len_refuses_lengths_past_c_int_without_allocating() {
        assert_eq!(bind_len(0).unwrap(), 0);
        assert_eq!(bind_len(42).unwrap(), 42);
        let err = bind_len(i32::MAX as usize + 1).unwrap_err();
        assert!(matches!(
            &err,
            LedgerError::Sqlite {
                class: SqliteErrorClass::Data,
                code: Some(code),
                ..
            } if *code == ffi::SQLITE_TOOBIG
        ));
        assert_eq!(err.to_string(), "string or blob too big");
    }

    #[test]
    fn bind_arity_mismatch_raises_programming_error_both_directions() {
        let conn = Connection::open_in_memory().unwrap();
        let too_few = query_rows(&conn, "SELECT ?, ?", &[SqlCell::Int(1)]).unwrap_err();
        assert!(matches!(
            &too_few,
            LedgerError::Sqlite {
                class: SqliteErrorClass::Programming,
                code: None,
                ..
            }
        ));
        assert_eq!(
            too_few.to_string(),
            "Incorrect number of bindings supplied. The current statement uses 2, \
             and there are 1 supplied."
        );
        let too_many =
            query_rows(&conn, "SELECT ?", &[SqlCell::Int(1), SqlCell::Int(2)]).unwrap_err();
        assert_eq!(
            too_many.to_string(),
            "Incorrect number of bindings supplied. The current statement uses 1, \
             and there are 2 supplied."
        );
    }

    #[test]
    fn error_name_resolves_extended_constraint_codes_and_nomem_maps_to_memory() {
        assert_eq!(
            error_name(ffi::SQLITE_CONSTRAINT_UNIQUE).as_deref(),
            Some("SQLITE_CONSTRAINT_UNIQUE")
        );
        assert_eq!(
            error_name(ffi::SQLITE_CANTOPEN).as_deref(),
            Some("SQLITE_CANTOPEN")
        );
        assert!(matches!(
            class_for(ffi::SQLITE_NOMEM),
            SqliteErrorClass::Memory
        ));
    }
}
