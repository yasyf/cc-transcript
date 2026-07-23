//! The shared code-correction ledger over a bundled SQLite (the Rust port of
//! `cc_transcript.corrections.CorrectionLog` and its `cc_transcript.ledger.SyncLedger`
//! base). The on-disk format is a cross-language contract — cc-review's Go reads this
//! ledger file directly — so the exact v1 schema, WAL journal mode, and `INSERT OR IGNORE`
//! append form a cross-language contract.
//!
//! ONE ENGINE PER PROCESS. This crate links its own bundled SQLite; two SQLite libraries
//! in one process cannot coordinate POSIX advisory locks, so a process must never mix this
//! engine with another SQLite (e.g. Python's `sqlite3`) against the same ledger file. The
//! cross-process contract is safe: cc-review's Go and any separate process coordinate
//! through the file locks normally; only same-process, two-library concurrency is unsound.
//!
//! `detail_json` is stored opaquely: the caller serializes it (`json.dumps` on the Python
//! side), because only Python's own encoder reproduces its NaN/Infinity and lone-surrogate
//! output. The SQLite error taxonomy, storage-class row projection, and single-statement
//! `sql()` rule live in `crate::sqlite`, shared with the feedback engine.

use std::path::Path;

use rusqlite::{ffi, params, Connection};

use crate::literals::corrections::DDL;
use crate::schema;
use crate::sqlite::{query_rows, run_query, sqlite_error};
pub use crate::sqlite::{LedgerError, SqlCell, SqlRow, SqliteErrorClass};

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
const SCHEMA_IDENTITY: &str = "cc-transcript-corrections";

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
    /// Opens or creates exactly one v1 ledger. Only a truly empty database is initialized;
    /// every existing schema mismatch is rejected without repair or import.
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
        let exact_schema = schema::compile(DDL, &[])?;
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout = 2000")?;
        schema::initialize_or_validate(&conn, SCHEMA_IDENTITY, &exact_schema)?;
        conn.execute_batch("PRAGMA journal_mode = WAL")?;
        schema::install_guard(&conn);
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
        run_query(
            &self.conn,
            "SELECT * FROM corrections WHERE session_id = ? ORDER BY ts_ms, id",
            params![session_id],
        )
    }

    /// All corrections whose `detail.repo` is `repo`, ordered by timestamp.
    pub fn for_repo(&self, repo: &str) -> Result<Vec<SqlRow>, LedgerError> {
        run_query(
            &self.conn,
            "SELECT * FROM corrections WHERE json_extract(detail_json, '$.repo') = ? ORDER BY ts_ms, id",
            params![repo],
        )
    }

    /// Corrections with `ts_ms` strictly greater than `ts_ms`, oldest first, optionally
    /// scoped to one `source`.
    pub fn since(&self, ts_ms: i64, source: Option<&str>) -> Result<Vec<SqlRow>, LedgerError> {
        match source {
            None => run_query(
                &self.conn,
                "SELECT * FROM corrections WHERE ts_ms > ? ORDER BY ts_ms, id",
                params![ts_ms],
            ),
            Some(source) => run_query(
                &self.conn,
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
        run_query(
            &self.conn,
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
        run_query(
            &self.conn,
            "SELECT * FROM corrections WHERE session_id = ? AND incorrect_digest = ? ORDER BY ts_ms, id",
            params![session_id, incorrect_digest],
        )
    }

    /// Runs a raw SQL `statement` — the escape hatch behind `corrections sql`. Mirrors
    /// `sqlite3.Cursor.execute`: exactly one statement, comment/whitespace-only SQL yields
    /// no rows. No parameter binding (the CLI passes literal SQL).
    pub fn sql(&self, statement: &str) -> Result<Vec<SqlRow>, LedgerError> {
        query_rows(&self.conn, statement, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> CorrectionLog {
        CorrectionLog::open(Path::new(":memory:")).unwrap()
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cc-corrections-{tag}-{}.db", std::process::id()))
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
        let insert = "INSERT INTO corrections (ts_ms, session_id, source, anchor_uuid, \
            incorrect_file, incorrect_old, incorrect_new, incorrect_digest) \
            VALUES (1, 's', 'x', 'a', '/f', '', '', 'd')";
        assert!(matches!(
            log.sql(&format!("{insert}; {insert}")),
            Err(LedgerError::MultipleStatements)
        ));
        assert!(log.for_session("s").unwrap().is_empty());
    }

    #[test]
    fn runtime_cannot_mutate_the_schema_or_attestation() {
        let log = log();
        for statement in [
            "CREATE TABLE probe(id INTEGER)",
            "UPDATE cc_transcript_schema_v1 SET schema_identity = 'spoofed'",
            "PRAGMA user_version = 2",
            "PRAGMA writable_schema = ON",
            "UPDATE sqlite_schema SET sql = 'spoofed' WHERE name = 'corrections'",
            "ATTACH DATABASE ':memory:' AS attached",
        ] {
            assert!(log.sql(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn existing_schema_tampering_is_rejected_without_repair() {
        for (tag, mutation) in [
            ("extra", "CREATE TABLE unexpected(id INTEGER);"),
            ("missing", "DROP INDEX idx_corrections_incorrect_digest;"),
            (
                "marker",
                "UPDATE cc_transcript_schema_v1 SET object_fingerprint = printf('%064d', 0);",
            ),
            ("version", "PRAGMA user_version = 2;"),
        ] {
            let path = temp_path(tag);
            std::fs::remove_file(&path).ok();
            CorrectionLog::open(&path).unwrap();
            Connection::open(&path)
                .unwrap()
                .execute_batch(mutation)
                .unwrap();
            let error = CorrectionLog::open(&path).unwrap_err();
            assert!(error.to_string().contains("schema"), "{error}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn foreign_database_rejection_does_not_change_rollback_journal_header() {
        let path = temp_path("foreign-header");
        std::fs::remove_file(&path).ok();
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE foreign_table(id INTEGER);")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(CorrectionLog::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn concurrent_first_opens_converge_on_one_exact_schema() {
        let path = temp_path("create-race");
        std::fs::remove_file(&path).ok();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let opens: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    CorrectionLog::open(&path).map(|_| ())
                })
            })
            .collect();
        for open in opens {
            open.join().unwrap().unwrap();
        }
        std::fs::remove_file(&path).ok();
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
