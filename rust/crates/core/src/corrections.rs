//! The shared code-correction ledger over a bundled SQLite (the Rust port of
//! `cc_transcript.corrections.CorrectionLog` and its `cc_transcript.ledger.SyncLedger`
//! base). The on-disk format is a cross-language contract — cc-review's Go reads this
//! ledger file directly — so the schema, WAL journal mode, and `INSERT OR IGNORE` append
//! mirror the Python reference byte-for-byte.
//!
//! ONE ENGINE PER PROCESS. This crate links its own bundled SQLite; two SQLite libraries
//! in one process cannot coordinate POSIX advisory locks, so a process must never mix this
//! engine with another SQLite (e.g. Python's `sqlite3`) against the same ledger file. The
//! cross-process contract is safe: cc-review's Go and any separate process coordinate
//! through the file locks normally; only same-process, two-library concurrency is unsound.
//! Post-P4 the Python facade delegates fully to this engine and never opens the ledger via
//! `sqlite3` in-process.
//!
//! `detail_json` is stored opaquely: the caller serializes it (`json.dumps` on the Python
//! side), because only Python's own encoder reproduces its NaN/Infinity and lone-surrogate
//! output. Reads return each row's columns by SQLite storage class (`SqlCell`), so the
//! caller projects them exactly as Python's dynamic `row[col]` does. `DDL` is generated
//! the hand-owned `crate::literals::corrections` table (mirrored to Python's
//! `CORRECTIONS_DDL` via `_native.embedded_literals()`).

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;

use rusqlite::types::ValueRef;
use rusqlite::{ffi, params, Connection, Params, Row};

use crate::literals::corrections::DDL;

// Parity: CorrectionLog.COLUMNS, in write order — the append list.
const COLUMNS: [&str; 16] = [
    "ts_ms",
    "session_id",
    "source",
    "anchor_uuid",
    "incorrect_digest",
    "incorrect_file",
    "incorrect_old",
    "incorrect_new",
    "correction_origin",
    "correction_file",
    "correction_old",
    "correction_new",
    "correction_commit",
    "correction_text",
    "overlap",
    "detail_json",
];

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

/// A ledger failure, pre-classified for the py binding's exception mapping. `code`/`name`
/// carry the SQLite extended result code and symbolic name for a genuine SQLite failure,
/// and are None for a Python-side error (multi-statement, UTF-8 decode, NUL byte).
#[derive(Debug)]
pub enum LedgerError {
    Io {
        error: std::io::Error,
        path: PathBuf,
    },
    Sqlite {
        class: SqliteErrorClass,
        message: String,
        code: Option<i32>,
        name: Option<String>,
    },
    MultipleStatements,
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io { error, path } => write!(f, "{}: {error}", path.display()),
            LedgerError::Sqlite { message, .. } => write!(f, "{message}"),
            LedgerError::MultipleStatements => {
                write!(f, "You can only execute one statement at a time.")
            }
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

/// One incorrect edit and the correction that overwrote it — the append DTO mirroring
/// `cc_transcript.corrections.Correction`. `detail_json` is the already-serialized detail
/// object (the caller applies `json.dumps(dict(detail))`); the ledger stores it verbatim.
#[derive(Debug, Clone)]
pub struct Correction {
    pub ts_ms: i64,
    pub session_id: String,
    pub source: String,
    pub anchor_uuid: String,
    pub incorrect_digest: Option<String>,
    pub incorrect_file: String,
    pub incorrect_old: String,
    pub incorrect_new: String,
    pub correction_origin: Option<String>,
    pub correction_file: Option<String>,
    pub correction_old: Option<String>,
    pub correction_new: Option<String>,
    pub correction_commit: Option<String>,
    pub correction_text: Option<String>,
    pub overlap: f64,
    pub detail_json: String,
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

/// The `corrections` ledger — a WAL-mode, `INSERT OR IGNORE` table behind a fixed schema.
///
/// Opened autocommit (each statement its own transaction) with a busy timeout because
/// writers across the family touch the same file concurrently. Durable by convention:
/// rows are never auto-dropped. Requires a local disk — WAL does not work over NFS. See
/// the module docs: one SQLite engine per process against a given ledger file.
#[derive(Debug)]
pub struct CorrectionLog {
    conn: Connection,
}

impl CorrectionLog {
    /// Opens (creating if needed) the ledger at `path`, mirroring `SyncLedger.open`: the
    /// parent directories are created, then WAL journal mode, a 2000 ms busy timeout, and
    /// the schema are applied in that order. An empty path is rejected like an unopenable
    /// one — a private temp database is never a valid ledger.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        if path.as_os_str().is_empty() {
            return Err(sqlite_error(
                ffi::SQLITE_CANTOPEN,
                "unable to open database file".to_string(),
            ));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| LedgerError::Io {
                    error,
                    path: parent.to_path_buf(),
                })?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        conn.execute_batch("PRAGMA busy_timeout = 2000")?;
        conn.execute_batch(DDL)?;
        Ok(Self { conn })
    }

    /// Appends `record` as a single `INSERT OR IGNORE`, idempotent on the table's UNIQUE
    /// key. `detail_json` is stored exactly as given.
    pub fn append(&self, record: &Correction) -> Result<(), LedgerError> {
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO corrections ({}) VALUES ({})",
                COLUMNS.join(", "),
                ["?"; COLUMNS.len()].join(", "),
            ),
            params![
                record.ts_ms,
                record.session_id,
                record.source,
                record.anchor_uuid,
                record.incorrect_digest,
                record.incorrect_file,
                record.incorrect_old,
                record.incorrect_new,
                record.correction_origin,
                record.correction_file,
                record.correction_old,
                record.correction_new,
                record.correction_commit,
                record.correction_text,
                record.overlap,
                record.detail_json,
            ],
        )?;
        Ok(())
    }

    /// All records for `session_id`, ordered by timestamp.
    pub fn for_session(&self, session_id: &str) -> Result<Vec<SqlRow>, LedgerError> {
        self.run_query(
            "SELECT * FROM corrections WHERE session_id = ? ORDER BY ts_ms, id",
            params![session_id],
        )
    }

    /// All corrections whose `detail.repo` is `repo`, ordered by timestamp.
    pub fn for_repo(&self, repo: &str) -> Result<Vec<SqlRow>, LedgerError> {
        self.run_query(
            "SELECT * FROM corrections WHERE json_extract(detail_json, '$.repo') = ? ORDER BY ts_ms, id",
            params![repo],
        )
    }

    /// Corrections with `ts_ms` strictly greater than `ts_ms`, oldest first, optionally
    /// scoped to one `source`.
    pub fn since(&self, ts_ms: i64, source: Option<&str>) -> Result<Vec<SqlRow>, LedgerError> {
        match source {
            None => self.run_query(
                "SELECT * FROM corrections WHERE ts_ms > ? ORDER BY ts_ms, id",
                params![ts_ms],
            ),
            Some(source) => self.run_query(
                "SELECT * FROM corrections WHERE ts_ms > ? AND source = ? ORDER BY ts_ms, id",
                params![ts_ms, source],
            ),
        }
    }

    /// The corrections harvested around one feedback `anchor_uuid`.
    pub fn for_anchor(
        &self,
        session_id: &str,
        anchor_uuid: &str,
    ) -> Result<Vec<SqlRow>, LedgerError> {
        self.run_query(
            "SELECT * FROM corrections WHERE session_id = ? AND anchor_uuid = ? ORDER BY ts_ms, id",
            params![session_id, anchor_uuid],
        )
    }

    /// Corrections of the tool call with `incorrect_digest` in `session_id` — the
    /// cross-consumer join shared with the decisions ledger.
    pub fn by_digest(
        &self,
        session_id: &str,
        incorrect_digest: &str,
    ) -> Result<Vec<SqlRow>, LedgerError> {
        self.run_query(
            "SELECT * FROM corrections WHERE session_id = ? AND incorrect_digest = ? ORDER BY ts_ms, id",
            params![session_id, incorrect_digest],
        )
    }

    /// Runs a raw SQL `statement` — the escape hatch behind `corrections sql`. Mirrors
    /// `sqlite3.Cursor.execute`: exactly one statement (a trailing statement raises before
    /// anything runs; a trailing comment is fine), comment/whitespace-only SQL yields no
    /// rows. Only the first statement is prepared — the tail is never compiled.
    pub fn sql(&self, statement: &str) -> Result<Vec<SqlRow>, LedgerError> {
        let c_sql = CString::new(statement).map_err(|_| LedgerError::Sqlite {
            class: SqliteErrorClass::Programming,
            message: "the query contains a null character".to_string(),
            code: None,
            name: None,
        })?;
        let db = unsafe { self.conn.handle() };
        let mut raw: *mut ffi::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { ffi::sqlite3_prepare_v2(db, c_sql.as_ptr(), -1, &mut raw, &mut tail) };
        if rc != ffi::SQLITE_OK {
            return Err(sqlite_error_from_db(db));
        }
        let handle = StatementHandle(raw);
        if raw.is_null() {
            return Ok(Vec::new());
        }
        if !unsafe { lstrip_sql(tail) }.is_null() {
            return Err(LedgerError::MultipleStatements);
        }
        read_rows(db, handle.0)
    }

    fn run_query(&self, sql: &str, params: impl Params) -> Result<Vec<SqlRow>, LedgerError> {
        let mut stmt = self.conn.prepare(sql)?;
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
}

// Finalizes a raw ffi statement on every exit path of `sql`.
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

fn sqlite_error_from_db(db: *mut ffi::sqlite3) -> LedgerError {
    let extended = unsafe { ffi::sqlite3_extended_errcode(db) };
    let message = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned();
    sqlite_error(extended, message)
}

fn sqlite_error(extended: i32, message: String) -> LedgerError {
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

// The ledger's extended constraint codes resolve exactly; any other extended code falls
// back to its primary name. A None becomes "unknown" in the binding, matching CPython.
fn error_name(extended: i32) -> Option<String> {
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

    fn log() -> CorrectionLog {
        CorrectionLog::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn sql_one_statement_rule_matches_cpython() {
        let log = log();
        // No first statement (a leading ";" prepares to nothing, then the empty input ends).
        for empty in ["", "   ", "-- c", "/* x */", ";", ";;"] {
            assert!(log.sql(empty).unwrap().is_empty(), "{empty}");
        }
        // A single statement; SQLite skips leading empty statements, so "; SELECT 1" runs.
        for single in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1; -- c",
            "SELECT 1; /* c */",
            "SELECT 1;\n\n",
            "; SELECT 1",
            ";; SELECT 1",
        ] {
            assert_eq!(log.sql(single).unwrap().len(), 1, "{single}");
        }
        // A trailing statement (even a bare ";") after the first raises.
        for multi in [
            "SELECT 1; ;",
            "SELECT 1;;",
            "SELECT 1; SELECT 2",
            "SELECT 1; definitely bad",
            "; SELECT 1; SELECT 2",
        ] {
            assert!(
                matches!(log.sql(multi), Err(LedgerError::MultipleStatements)),
                "{multi}"
            );
        }
    }

    #[test]
    fn sql_rejects_multiple_before_executing_the_head() {
        let log = log();
        assert!(matches!(
            log.sql("CREATE TABLE probe(x); INSERT INTO probe VALUES (1)"),
            Err(LedgerError::MultipleStatements)
        ));
        assert!(log
            .sql("SELECT name FROM sqlite_master WHERE name = 'probe'")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sql_null_byte_is_a_programming_error() {
        assert!(matches!(
            log().sql("SELECT 1\0; DROP"),
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Programming,
                code: None,
                ..
            })
        ));
    }

    #[test]
    fn invalid_utf8_text_is_operational_error_not_panic() {
        match log().sql("SELECT CAST(X'80' AS TEXT)") {
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Operational,
                message,
                code: None,
                ..
            }) => {
                assert!(message.contains("Could not decode to UTF-8"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_path_is_cantopen_operational_error() {
        assert!(matches!(
            CorrectionLog::open(Path::new("")),
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Operational,
                code: Some(c),
                ..
            }) if c == ffi::SQLITE_CANTOPEN
        ));
    }

    #[test]
    fn bad_path_is_cantopen() {
        assert!(matches!(
            CorrectionLog::open(Path::new("/")),
            Err(LedgerError::Sqlite { class: SqliteErrorClass::Operational, code: Some(c), .. }) if c & 0xff == ffi::SQLITE_CANTOPEN
        ));
    }

    #[test]
    fn non_database_file_is_database_error() {
        let path = std::env::temp_dir().join(format!("cc-notadb-{}.db", std::process::id()));
        std::fs::write(
            &path,
            b"this is not an sqlite database file, padding padding padding",
        )
        .unwrap();
        let result = CorrectionLog::open(&path);
        std::fs::remove_file(&path).ok();
        match result {
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Database,
                code: Some(c),
                name,
                ..
            }) => {
                assert_eq!(c, ffi::SQLITE_NOTADB);
                assert_eq!(name.as_deref(), Some("SQLITE_NOTADB"));
            }
            other => panic!("{other:?}"),
        }
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

    #[test]
    fn sql_unique_violation_is_integrity_error_with_extended_name() {
        let log = log();
        let insert = "INSERT INTO corrections (ts_ms, session_id, source, anchor_uuid, incorrect_file, \
             incorrect_old, incorrect_new, incorrect_digest) VALUES (1, 's', 'x', 'a', '/f', '', '', 'd')";
        log.sql(insert).unwrap();
        match log.sql(insert) {
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Integrity,
                code: Some(c),
                name,
                ..
            }) => {
                assert_eq!(c, ffi::SQLITE_CONSTRAINT_UNIQUE);
                assert_eq!(name.as_deref(), Some("SQLITE_CONSTRAINT_UNIQUE"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn append_stores_detail_json_verbatim_and_reads_back_by_storage_class() {
        let log = log();
        log.append(&Correction {
            ts_ms: 1_000,
            session_id: "s".to_string(),
            source: "x".to_string(),
            anchor_uuid: "a".to_string(),
            incorrect_digest: None,
            incorrect_file: "/f".to_string(),
            incorrect_old: String::new(),
            incorrect_new: String::new(),
            correction_origin: None,
            correction_file: None,
            correction_old: None,
            correction_new: None,
            correction_commit: None,
            correction_text: None,
            overlap: 0.5,
            detail_json: r#"{"x": NaN}"#.to_string(),
        })
        .unwrap();
        let rows = log.for_session("s").unwrap();
        let by_name: std::collections::HashMap<_, _> = rows[0].iter().cloned().collect();
        assert!(matches!(by_name["ts_ms"], SqlCell::Int(1_000)));
        assert!(matches!(&by_name["detail_json"], SqlCell::Text(s) if s == r#"{"x": NaN}"#));
        assert!(matches!(by_name["overlap"], SqlCell::Real(f) if f == 0.5));
    }
}
