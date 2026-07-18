//! The native feedback-store engine — one connection to `feedback.db`, corrections-pattern.
//! The Python facade keeps `_txn_owner` over the bare txn control; DDL lives in `literals`.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use crate::literals::feedback::{EVENT_COLUMNS, FEEDBACK_DDL, FILE_SCHEMA, VERDICT_DDL_TEMPLATE};
use crate::sqlite::{
    exec_changes, query_rows, run_query, sqlite_error, LedgerError, SqlCell, SqlRow,
    SqliteErrorClass,
};

// Parity: mining.store.INSERT_EVENT column list — the base feedback_events append order.
const BASE_EVENT_COLUMNS: [&str; 10] = [
    "dedup_key",
    "source_kind",
    "session_id",
    "event_uuid",
    "occurred_at",
    "text",
    "payload_json",
    "context_json",
    "cc_version",
    "ingested_at",
];

// FEEDBACK_DDL's last field line; event columns splice in after it (cc-steer's inline form).
const FEEDBACK_LAST_FIELD: &str = "  ingested_at TEXT NOT NULL\n";

/// One guard-ALTER migration run once when `column` is absent (captain-hook's pattern).
#[derive(Debug, Clone)]
pub struct Migration {
    pub table: String,
    pub column: String,
    pub ddl: String,
    pub backfill: Option<String>,
}

/// The open-time schema config downstream composes with (Python `StoreSchema`).
/// `event_columns` are full column DDL strings (`"origin_path TEXT"`).
#[derive(Debug, Clone)]
pub struct FeedbackConfig {
    pub extra_ddl: Vec<String>,
    pub event_columns: Vec<String>,
    pub migrations: Vec<Migration>,
    pub verdict_table: String,
    pub accepted_column: String,
    pub summary_column: String,
    pub event_filter: Option<String>,
    pub readonly: bool,
    pub busy_timeout_ms: i64,
}

/// The feedback-store engine over a bundled SQLite behind the `sqlite` feature — the one
/// connection to the database.
pub struct FeedbackEngine {
    conn: Connection,
    verdict_table: String,
    accepted_column: String,
    summary_column: String,
    event_filter: Option<String>,
    event_column_names: Vec<String>,
    in_txn: Cell<bool>,
}

impl FeedbackEngine {
    /// Opens (creating if needed) the store at `path` under `config`. Readonly opens
    /// `SQLITE_OPEN_READ_ONLY` + `query_only`, skipping DDL and migrations — the read-only
    /// v8/v9 verdict-schema check still runs, so a legacy DB fails open in both modes.
    pub fn open(path: &Path, config: FeedbackConfig) -> Result<Self, LedgerError> {
        if path.as_os_str().is_empty() {
            return Err(sqlite_error(
                rusqlite::ffi::SQLITE_CANTOPEN,
                "unable to open database file".to_string(),
            ));
        }
        let event_column_names = parse_column_names(&config.event_columns);
        for name in [
            &config.verdict_table,
            &config.accepted_column,
            &config.summary_column,
        ]
        .into_iter()
        .chain(event_column_names.iter())
        {
            validate_identifier(name)?;
        }
        if !config.readonly {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|error| LedgerError::Io {
                        error,
                        path: parent.to_path_buf(),
                    })?;
                }
            }
        }
        let conn = if config.readonly {
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?
        } else {
            Connection::open(path)?
        };
        conn.execute_batch(&format!("PRAGMA busy_timeout = {}", config.busy_timeout_ms))?;
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        if config.readonly {
            conn.execute_batch("PRAGMA query_only = ON")?;
            validate_verdict_schema(&conn, &config.verdict_table)?;
        } else {
            conn.execute_batch("PRAGMA journal_mode = WAL")?;
            let feedback_ddl = splice_event_columns(FEEDBACK_DDL, &config.event_columns);
            let verdict_ddl = render_verdict_ddl(
                &config.verdict_table,
                &config.accepted_column,
                &config.summary_column,
            );
            conn.execute_batch(&format!("{FILE_SCHEMA}{feedback_ddl}{verdict_ddl}"))?;
            guard_alter(
                &conn,
                "feedback_events",
                &config.event_columns,
                &event_column_names,
            )?;
            for script in &config.extra_ddl {
                conn.execute_batch(script)?;
            }
            run_migrations(&conn, &config.migrations)?;
            validate_verdict_schema(&conn, &config.verdict_table)?;
        }
        Ok(Self {
            conn,
            verdict_table: config.verdict_table,
            accepted_column: config.accepted_column,
            summary_column: config.summary_column,
            event_filter: config.event_filter,
            event_column_names,
            in_txn: Cell::new(false),
        })
    }

    /// One parameterized statement returning rows — single-statement rule, bytes-capable.
    pub fn sql(&self, statement: &str, params: &[SqlCell]) -> Result<Vec<SqlRow>, LedgerError> {
        query_rows(&self.conn, statement, params)
    }

    /// One parameterized write statement; returns the modified-row count.
    pub fn execute(&self, statement: &str, params: &[SqlCell]) -> Result<i64, LedgerError> {
        exec_changes(&self.conn, statement, params)
    }

    /// Runs `statement` once per parameter set; returns the total modified-row count.
    pub fn executemany(&self, statement: &str, seq: &[Vec<SqlCell>]) -> Result<i64, LedgerError> {
        let mut total = 0;
        for params in seq {
            total += exec_changes(&self.conn, statement, params)?;
        }
        Ok(total)
    }

    /// A multi-statement script — refused mid-transaction (its implicit commit would end
    /// the caller's transaction, the `prepare_connection` hazard).
    pub fn executescript(&self, script: &str) -> Result<(), LedgerError> {
        if self.in_txn.get() {
            return Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Programming,
                message: "cannot executescript() while a transaction is open".to_string(),
                code: None,
                name: None,
            });
        }
        self.conn.execute_batch(script)?;
        Ok(())
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.conn.last_insert_rowid()
    }

    /// Loads a loadable extension (sqlite-vec's dylib), enabling extension loading for the
    /// load and disabling it after — mirroring `prepare_connection`.
    pub fn load_extension(&self, path: &str) -> Result<(), LedgerError> {
        unsafe { self.conn.load_extension_enable()? };
        let loaded = unsafe { self.conn.load_extension(Path::new(path), None) };
        self.conn.load_extension_disable()?;
        loaded?;
        Ok(())
    }

    pub fn begin_immediate(&self) -> Result<(), LedgerError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        self.in_txn.set(true);
        Ok(())
    }

    pub fn commit(&self) -> Result<(), LedgerError> {
        self.conn.execute_batch("COMMIT")?;
        self.in_txn.set(false);
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), LedgerError> {
        self.conn.execute_batch("ROLLBACK")?;
        self.in_txn.set(false);
        Ok(())
    }

    /// `INSERT OR IGNORE`s `rows` (each with its `extras` for the configured event columns),
    /// returning the dedup keys actually inserted via `RETURNING` (a dup yields no key).
    pub fn insert_candidates(
        &self,
        rows: &[Vec<SqlCell>],
        extras: Option<&[Vec<SqlCell>]>,
    ) -> Result<Vec<String>, LedgerError> {
        if let Some(ex) = extras {
            if ex.len() != rows.len() {
                return Err(config_error(format!(
                    "insert_candidates: {} extras rows for {} candidate rows",
                    ex.len(),
                    rows.len(),
                )));
            }
            if let Some(row) = ex
                .iter()
                .find(|row| row.len() != self.event_column_names.len())
            {
                return Err(config_error(format!(
                    "insert_candidates: an extras row has {} values for {} event columns",
                    row.len(),
                    self.event_column_names.len(),
                )));
            }
        }
        let columns: Vec<&str> = BASE_EVENT_COLUMNS
            .iter()
            .copied()
            .chain(self.event_column_names.iter().map(String::as_str))
            .collect();
        let sql = format!(
            "INSERT OR IGNORE INTO feedback_events ({}) VALUES ({}) RETURNING dedup_key",
            columns.join(", "),
            vec!["?"; columns.len()].join(", "),
        );
        let mut inserted = Vec::new();
        for (i, base) in rows.iter().enumerate() {
            let mut row = base.clone();
            if let Some(ex) = extras {
                row.extend(ex[i].iter().cloned());
            }
            if let Some((_, SqlCell::Text(key))) = query_rows(&self.conn, &sql, &row)?
                .into_iter()
                .next()
                .and_then(|mut r| r.pop())
            {
                inserted.push(key);
            }
        }
        Ok(inserted)
    }

    /// Upserts a scanned file's mtime (`ON CONFLICT(path)`).
    pub fn record_file(&self, path: &str, mtime: f64) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO files(path, mtime) VALUES(?, ?) ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
            params![path, mtime],
        )?;
        Ok(())
    }

    /// The recorded `path` -> `mtime` rows.
    pub fn file_mtimes(&self) -> Result<Vec<SqlRow>, LedgerError> {
        run_query(&self.conn, "SELECT path, mtime FROM files", params![])
    }

    /// Ingestion counts: `(total events, scanned files, per-source-kind rows)`.
    pub fn stats(&self) -> Result<(i64, i64, Vec<SqlRow>), LedgerError> {
        Ok((
            scalar_i64(&self.conn, "SELECT COUNT(*) FROM feedback_events")?,
            scalar_i64(&self.conn, "SELECT COUNT(*) FROM files")?,
            run_query(
                &self.conn,
                "SELECT source_kind, COUNT(*) AS n FROM feedback_events GROUP BY source_kind ORDER BY source_kind",
                params![],
            )?,
        ))
    }

    /// The most recent events (newest first), optionally one `source_kind`.
    pub fn recent(
        &self,
        source_kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SqlRow>, LedgerError> {
        match source_kind {
            Some(sk) => self.sql(
                "SELECT source_kind, occurred_at, text FROM feedback_events WHERE source_kind = ? ORDER BY occurred_at DESC, id DESC LIMIT ?",
                &[SqlCell::Text(sk.to_string()), SqlCell::Int(limit)],
            ),
            None => self.sql(
                "SELECT source_kind, occurred_at, text FROM feedback_events ORDER BY occurred_at DESC, id DESC LIMIT ?",
                &[SqlCell::Int(limit)],
            ),
        }
    }

    /// Every event (newest first) with the render columns, optionally one `source_kind`.
    pub fn events(&self, source_kind: Option<&str>) -> Result<Vec<SqlRow>, LedgerError> {
        let base = "SELECT id, source_kind, occurred_at, text, payload_json, context_json, event_uuid, session_id FROM feedback_events";
        match source_kind {
            Some(sk) => self.sql(
                &format!("{base} WHERE source_kind = ? ORDER BY occurred_at DESC, id DESC"),
                &[SqlCell::Text(sk.to_string())],
            ),
            None => self.sql(&format!("{base} ORDER BY occurred_at DESC, id DESC"), &[]),
        }
    }

    /// Every stored event's dedup key.
    pub fn dedup_keys(&self) -> Result<Vec<String>, LedgerError> {
        Ok(run_query(
            &self.conn,
            "SELECT dedup_key FROM feedback_events",
            params![],
        )?
        .into_iter()
        .filter_map(|mut r| match r.pop() {
            Some((_, SqlCell::Text(s))) => Some(s),
            _ => None,
        })
        .collect())
    }

    /// Records one verdict keyed `(dedup_key, role, prompt_version)`: a `'full'` replaces a
    /// `'summary'` else no-op. Returns whether a row changed; runs on the caller's txn.
    #[allow(clippy::too_many_arguments)]
    pub fn record_verdict(
        &self,
        dedup_key: &str,
        role: &str,
        prompt_version: i64,
        model: &str,
        category: &str,
        accepted: bool,
        summary: &str,
        confidence: f64,
        rationale: &str,
        canonical_key: Option<&str>,
        fidelity: &str,
        judged_at: &str,
    ) -> Result<bool, LedgerError> {
        let sql = format!(
            "INSERT INTO {t} (dedup_key, role, prompt_version, model, category, {a}, {s}, \
             confidence, rationale, canonical_key, fidelity, judged_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(dedup_key, role, prompt_version) DO UPDATE SET \
             model = excluded.model, category = excluded.category, \
             {a} = excluded.{a}, {s} = excluded.{s}, confidence = excluded.confidence, \
             rationale = excluded.rationale, canonical_key = excluded.canonical_key, \
             fidelity = excluded.fidelity, judged_at = excluded.judged_at \
             WHERE fidelity = 'summary' AND excluded.fidelity = 'full'",
            t = self.verdict_table,
            a = self.accepted_column,
            s = self.summary_column,
        );
        let changed = self.conn.execute(
            &sql,
            params![
                dedup_key,
                role,
                prompt_version,
                model,
                category,
                accepted,
                summary,
                confidence,
                rationale,
                canonical_key,
                fidelity,
                judged_at,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Events lacking a `(role, prompt_version)` verdict, unjudged first. `refresh_summary`
    /// also yields summary rows with `verdict_id`; `limit`/`offset` pass through verbatim.
    pub fn unjudged(
        &self,
        role: &str,
        prompt_version: i64,
        refresh_summary: bool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<SqlRow>, LedgerError> {
        let mut sql = if refresh_summary {
            let prefix = self
                .event_filter
                .as_ref()
                .map(|f| format!("{f} AND "))
                .unwrap_or_default();
            format!(
                "SELECT {EVENT_COLUMNS}, t.id AS verdict_id FROM feedback_events e \
                 LEFT JOIN {} t ON t.dedup_key = e.dedup_key AND t.role = ? AND t.prompt_version = ? \
                 WHERE {prefix}(t.id IS NULL OR t.fidelity = 'summary') ORDER BY (t.id IS NOT NULL), e.id",
                self.verdict_table,
            )
        } else {
            let suffix = self
                .event_filter
                .as_ref()
                .map(|f| format!(" AND {f}"))
                .unwrap_or_default();
            format!(
                "SELECT {EVENT_COLUMNS} FROM feedback_events e \
                 LEFT JOIN {} t ON t.dedup_key = e.dedup_key AND t.role = ? AND t.prompt_version = ? \
                 WHERE t.id IS NULL{suffix} ORDER BY e.id",
                self.verdict_table,
            )
        };
        let mut params = vec![
            SqlCell::Text(role.to_string()),
            SqlCell::Int(prompt_version),
        ];
        if let Some(l) = limit {
            sql.push_str(" LIMIT ?");
            params.push(SqlCell::Int(l));
        } else if offset.is_some() {
            // SQLite's OFFSET needs a LIMIT clause; -1 is its documented "no limit".
            sql.push_str(" LIMIT -1");
        }
        if let Some(o) = offset {
            sql.push_str(" OFFSET ?");
            params.push(SqlCell::Int(o));
        }
        self.sql(&sql, &params)
    }

    /// Events joined with their `(role, prompt_version)` verdicts, oldest first; accepted /
    /// summary aliased to the generic names, `event_filter` ANDed.
    pub fn judged(&self, role: &str, prompt_version: i64) -> Result<Vec<SqlRow>, LedgerError> {
        let suffix = self
            .event_filter
            .as_ref()
            .map(|f| format!(" AND {f}"))
            .unwrap_or_default();
        let sql = format!(
            "SELECT {EVENT_COLUMNS}, t.category, t.{a} AS accepted, t.confidence, t.{s} AS summary, \
             t.rationale, t.model FROM feedback_events e JOIN {t} t ON t.dedup_key = e.dedup_key \
             WHERE t.role = ? AND t.prompt_version = ?{suffix} ORDER BY e.id",
            a = self.accepted_column,
            s = self.summary_column,
            t = self.verdict_table,
        );
        self.sql(
            &sql,
            &[
                SqlCell::Text(role.to_string()),
                SqlCell::Int(prompt_version),
            ],
        )
    }
}

fn render_verdict_ddl(table: &str, accepted: &str, summary: &str) -> String {
    VERDICT_DDL_TEMPLATE
        .replace("{table}", table)
        .replace("{accepted}", accepted)
        .replace("{summary}", summary)
}

// Splices event columns into FEEDBACK_DDL after the last field (cc-steer's inline form).
fn splice_event_columns(base: &str, event_columns: &[String]) -> String {
    if event_columns.is_empty() {
        return base.to_string();
    }
    let spliced: String = event_columns.iter().map(|c| format!(",\n  {c}")).collect();
    base.replacen(
        FEEDBACK_LAST_FIELD,
        &format!("  ingested_at TEXT NOT NULL{spliced}\n"),
        1,
    )
}

fn parse_column_names(columns: &[String]) -> Vec<String> {
    columns
        .iter()
        .map(|c| c.split_whitespace().next().unwrap_or_default().to_string())
        .collect()
}

fn config_error(message: String) -> LedgerError {
    LedgerError::Sqlite {
        class: SqliteErrorClass::Programming,
        message,
        code: None,
        name: None,
    }
}

// Config identifiers splice verbatim into DDL and queries; anything but a strict
// `[A-Za-z_][A-Za-z0-9_]*` identifier refuses the open before any SQL runs.
fn validate_identifier(name: &str) -> Result<(), LedgerError> {
    let mut chars = name.chars();
    if matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Ok(());
    }
    Err(config_error(format!(
        "invalid SQL identifier in store config: '{name}'"
    )))
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>, LedgerError> {
    Ok(
        run_query(conn, &format!("PRAGMA table_info({table})"), params![])?
            .into_iter()
            .filter_map(|row| cell_text(&row, "name"))
            .collect(),
    )
}

// Like run_migrations: lock-free precheck, then one BEGIN IMMEDIATE whose re-check
// serializes concurrent opens so only one ALTERs.
fn guard_alter(
    conn: &Connection,
    table: &str,
    columns_ddl: &[String],
    names: &[String],
) -> Result<(), LedgerError> {
    if columns_ddl.is_empty() {
        return Ok(());
    }
    let existing = table_columns(conn, table)?;
    if names.iter().all(|name| existing.contains(name)) {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match apply_guard_alter(conn, table, columns_ddl, names) {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn apply_guard_alter(
    conn: &Connection,
    table: &str,
    columns_ddl: &[String],
    names: &[String],
) -> Result<(), LedgerError> {
    let existing = table_columns(conn, table)?;
    for (ddl, name) in columns_ddl.iter().zip(names) {
        if !existing.contains(name) {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))?;
        }
    }
    Ok(())
}

// The lifted captain-hook migrate_columns runner: lock-free precheck, then one BEGIN
// IMMEDIATE per table that re-checks and applies each missing column + backfill.
fn run_migrations(conn: &Connection, migrations: &[Migration]) -> Result<(), LedgerError> {
    let mut tables: Vec<&str> = Vec::new();
    for migration in migrations {
        if !tables.contains(&migration.table.as_str()) {
            tables.push(&migration.table);
        }
    }
    for table in tables {
        let group: Vec<&Migration> = migrations.iter().filter(|m| m.table == table).collect();
        let existing = table_columns(conn, table)?;
        if group.iter().all(|m| existing.contains(&m.column)) {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match apply_migrations(conn, table, &group) {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    }
    Ok(())
}

fn apply_migrations(
    conn: &Connection,
    table: &str,
    group: &[&Migration],
) -> Result<(), LedgerError> {
    let existing = table_columns(conn, table)?;
    for migration in group {
        if existing.contains(&migration.column) {
            continue;
        }
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {}", migration.ddl))?;
        if let Some(backfill) = &migration.backfill {
            conn.execute_batch(backfill)?;
        }
    }
    Ok(())
}

// Parity: VerdictStoreMixin.ensure_verdict_schema at open — a canonical_key column and a
// UNIQUE index over exactly (dedup_key, role, prompt_version).
fn validate_verdict_schema(conn: &Connection, table: &str) -> Result<(), LedgerError> {
    let columns = table_columns(conn, table)?;
    let target = ["dedup_key", "role", "prompt_version"];
    let has_unique = unique_index_columns(conn, table)?
        .iter()
        .any(|cols| cols.as_slice() == target);
    if columns.contains("canonical_key") && has_unique {
        return Ok(());
    }
    Err(LedgerError::VerdictSchema {
        message: format!(
            "verdict table '{table}' predates the v9 schema: it needs a canonical_key column and a \
             UNIQUE(dedup_key, role, prompt_version) index. Rebuild it with the manual v8-to-v9 migration \
             (recreate '{table}' from verdicts_ddl() and copy the rows over) before reading or writing."
        ),
    })
}

fn unique_index_columns(conn: &Connection, table: &str) -> Result<Vec<Vec<String>>, LedgerError> {
    let indexes = run_query(conn, &format!("PRAGMA index_list({table})"), params![])?;
    let mut out = Vec::new();
    for index in indexes {
        if !matches!(cell(&index, "unique"), Some(SqlCell::Int(1))) {
            continue;
        }
        let Some(SqlCell::Text(name)) = cell(&index, "name") else {
            continue;
        };
        out.push(
            run_query(conn, &format!("PRAGMA index_info({name})"), params![])?
                .into_iter()
                .filter_map(|row| cell_text(&row, "name"))
                .collect(),
        );
    }
    Ok(out)
}

fn cell(row: &SqlRow, name: &str) -> Option<SqlCell> {
    row.iter().find(|(n, _)| n == name).map(|(_, c)| c.clone())
}

fn cell_text(row: &SqlRow, name: &str) -> Option<String> {
    match cell(row, name) {
        Some(SqlCell::Text(s)) => Some(s),
        _ => None,
    }
}

fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64, LedgerError> {
    match run_query(conn, sql, params![])?
        .first()
        .and_then(|r| r.first())
    {
        Some((_, SqlCell::Int(n))) => Ok(*n),
        _ => Err(LedgerError::Sqlite {
            class: SqliteErrorClass::Database,
            message: "scalar query returned no integer".to_string(),
            code: None,
            name: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FeedbackConfig {
        FeedbackConfig {
            extra_ddl: vec![],
            event_columns: vec![],
            migrations: vec![],
            verdict_table: "verdicts".to_string(),
            accepted_column: "accepted".to_string(),
            summary_column: "summary".to_string(),
            event_filter: None,
            readonly: false,
            busy_timeout_ms: 5000,
        }
    }

    fn steer_config() -> FeedbackConfig {
        FeedbackConfig {
            event_columns: vec![
                "origin_path TEXT".to_string(),
                "quarantined_reason TEXT".to_string(),
            ],
            verdict_table: "triage".to_string(),
            accepted_column: "is_steering".to_string(),
            summary_column: "what_claude_did".to_string(),
            event_filter: Some("e.quarantined_reason IS NULL".to_string()),
            ..config()
        }
    }

    fn open_memory(config: FeedbackConfig) -> FeedbackEngine {
        FeedbackEngine::open(Path::new(":memory:"), config).unwrap()
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cc-feedback-{tag}-{}.db", std::process::id()))
    }

    fn base_row(key: &str) -> Vec<SqlCell> {
        vec![
            SqlCell::Text(key.to_string()),
            SqlCell::Text("transcript_message".to_string()),
            SqlCell::Text("sess".to_string()),
            SqlCell::Text(format!("evt-{key}")),
            SqlCell::Text("2026-01-01T00:00:00+00:00".to_string()),
            SqlCell::Text("some feedback".to_string()),
            SqlCell::Text("{}".to_string()),
            SqlCell::Text("{}".to_string()),
            SqlCell::Text("1.0.0".to_string()),
            SqlCell::Text("2026-01-01T00:00:00+00:00".to_string()),
        ]
    }

    fn schema_sql(engine: &FeedbackEngine, name: &str) -> String {
        match engine
            .sql(
                "SELECT sql FROM sqlite_master WHERE name = ?",
                &[SqlCell::Text(name.to_string())],
            )
            .unwrap()
            .first()
            .and_then(|r| r.first().cloned())
        {
            Some((_, SqlCell::Text(sql))) => sql,
            other => panic!("no schema for {name}: {other:?}"),
        }
    }

    #[test]
    fn platform_open_creates_files_events_and_verdicts() {
        let engine = open_memory(config());
        for table in ["files", "feedback_events", "verdicts"] {
            assert!(schema_sql(&engine, table).contains(table), "{table}");
        }
    }

    #[test]
    fn steer_event_columns_splice_into_create_table() {
        let engine = open_memory(steer_config());
        let sql = schema_sql(&engine, "feedback_events");
        assert!(
            sql.contains(
                "ingested_at TEXT NOT NULL,\n  origin_path TEXT,\n  quarantined_reason TEXT"
            ),
            "{sql}"
        );
    }

    #[test]
    fn verdict_naming_renders_from_config() {
        let engine = open_memory(steer_config());
        let sql = schema_sql(&engine, "triage");
        assert!(sql.contains("is_steering INTEGER NOT NULL"), "{sql}");
        assert!(sql.contains("what_claude_did TEXT NOT NULL"), "{sql}");
    }

    #[test]
    fn guard_alter_adds_event_columns_to_a_pre_existing_db() {
        let path = temp_path("guard-alter");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        let engine = FeedbackEngine::open(&path, steer_config()).unwrap();
        let columns = table_columns(&engine.conn, "feedback_events").unwrap();
        assert!(columns.contains("origin_path"));
        assert!(columns.contains("quarantined_reason"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn migrations_run_once_with_backfill() {
        let mut cfg = config();
        cfg.extra_ddl = vec![
            "CREATE TABLE IF NOT EXISTS candidates (id INTEGER PRIMARY KEY, status TEXT NOT NULL);"
                .to_string(),
        ];
        cfg.migrations = vec![
            Migration {
                table: "candidates".to_string(),
                column: "generation".to_string(),
                ddl: "generation INTEGER NOT NULL DEFAULT 1".to_string(),
                backfill: None,
            },
            Migration {
                table: "candidates".to_string(),
                column: "resolved_at".to_string(),
                ddl: "resolved_at TEXT".to_string(),
                backfill: Some(
                    "UPDATE candidates SET resolved_at = 'x' WHERE status = 'accepted'".to_string(),
                ),
            },
        ];
        let path = temp_path("migrations");
        std::fs::remove_file(&path).ok();
        // A legacy DB: candidates with a row but none of the migration columns yet.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE candidates (id INTEGER PRIMARY KEY, status TEXT NOT NULL);\
                 INSERT INTO candidates (status) VALUES ('accepted');",
            )
            .unwrap();
        }
        let engine = FeedbackEngine::open(&path, cfg.clone()).unwrap();
        let columns = table_columns(&engine.conn, "candidates").unwrap();
        assert!(columns.contains("generation"));
        assert!(columns.contains("resolved_at"));
        let row = engine
            .sql("SELECT generation, resolved_at FROM candidates", &[])
            .unwrap();
        assert!(matches!(cell(&row[0], "generation"), Some(SqlCell::Int(1))));
        assert!(matches!(cell(&row[0], "resolved_at"), Some(SqlCell::Text(s)) if s == "x"));
        FeedbackEngine::open(&path, cfg).unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_rejects_non_identifier_config_names() {
        for cfg in [
            FeedbackConfig {
                verdict_table: "verdicts; DROP TABLE files; --".to_string(),
                ..config()
            },
            FeedbackConfig {
                accepted_column: "accepted, 1 AS pwned".to_string(),
                ..config()
            },
            FeedbackConfig {
                summary_column: "\"summary\"".to_string(),
                ..config()
            },
            FeedbackConfig {
                event_columns: vec!["a,b TEXT".to_string()],
                ..config()
            },
            FeedbackConfig {
                event_columns: vec![String::new()],
                ..config()
            },
        ] {
            let Err(err) = FeedbackEngine::open(Path::new(":memory:"), cfg) else {
                panic!("open accepted an invalid identifier");
            };
            assert!(
                matches!(
                    &err,
                    LedgerError::Sqlite {
                        class: SqliteErrorClass::Programming,
                        ..
                    }
                ),
                "{err}"
            );
            assert!(err.to_string().contains("invalid SQL identifier"), "{err}");
        }
    }

    #[test]
    fn insert_candidates_refuses_misaligned_extras_before_inserting() {
        let engine = open_memory(steer_config());
        engine.begin_immediate().unwrap();
        let short_outer = engine
            .insert_candidates(
                &[base_row("k1"), base_row("k2")],
                Some(&[vec![SqlCell::Null, SqlCell::Null]]),
            )
            .unwrap_err();
        assert!(
            matches!(
                &short_outer,
                LedgerError::Sqlite {
                    class: SqliteErrorClass::Programming,
                    ..
                }
            ),
            "{short_outer}"
        );
        let short_inner = engine
            .insert_candidates(&[base_row("k1")], Some(&[vec![SqlCell::Null]]))
            .unwrap_err();
        assert!(
            short_inner.to_string().contains("event columns"),
            "{short_inner}"
        );
        assert!(engine.dedup_keys().unwrap().is_empty());
        engine.rollback().unwrap();
    }

    #[test]
    fn concurrent_opens_serialize_the_guard_alter() {
        let path = temp_path("guard-race");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let opens: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    FeedbackEngine::open(&path, steer_config()).map(|_| ())
                })
            })
            .collect();
        for open in opens {
            open.join().unwrap().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unjudged_offset_without_limit_emits_valid_sql() {
        let engine = open_memory(config());
        engine.begin_immediate().unwrap();
        engine
            .insert_candidates(&[base_row("k1"), base_row("k2")], None)
            .unwrap();
        engine.commit().unwrap();
        let rows = engine.unjudged("judge", 1, false, None, Some(1)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(cell_text(&rows[0], "dedup_key"), Some("k2".to_string()));
    }

    #[test]
    fn insert_candidates_returns_only_newly_inserted_keys() {
        let engine = open_memory(config());
        engine.begin_immediate().unwrap();
        let keys = engine
            .insert_candidates(&[base_row("k1"), base_row("k2"), base_row("k1")], None)
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(keys, vec!["k1".to_string(), "k2".to_string()]);
        engine.begin_immediate().unwrap();
        let again = engine.insert_candidates(&[base_row("k1")], None).unwrap();
        engine.commit().unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn insert_candidates_appends_event_column_extras() {
        let engine = open_memory(steer_config());
        engine.begin_immediate().unwrap();
        engine
            .insert_candidates(
                &[base_row("k1")],
                Some(&[vec![
                    SqlCell::Text("/scan.jsonl".to_string()),
                    SqlCell::Null,
                ]]),
            )
            .unwrap();
        engine.commit().unwrap();
        let row = engine
            .sql(
                "SELECT origin_path, quarantined_reason FROM feedback_events",
                &[],
            )
            .unwrap();
        assert!(
            matches!(cell(&row[0], "origin_path"), Some(SqlCell::Text(s)) if s == "/scan.jsonl")
        );
        assert!(matches!(
            cell(&row[0], "quarantined_reason"),
            Some(SqlCell::Null)
        ));
    }

    #[test]
    fn event_filter_excludes_quarantined_from_unjudged() {
        let engine = open_memory(steer_config());
        engine.begin_immediate().unwrap();
        engine
            .insert_candidates(
                &[base_row("live"), base_row("dead")],
                Some(&[
                    vec![SqlCell::Text("/s".to_string()), SqlCell::Null],
                    vec![
                        SqlCell::Text("/s".to_string()),
                        SqlCell::Text("quarantined".to_string()),
                    ],
                ]),
            )
            .unwrap();
        engine.commit().unwrap();
        let unjudged = engine.unjudged("judge", 1, false, None, None).unwrap();
        let keys: Vec<_> = unjudged
            .iter()
            .filter_map(|r| cell_text(r, "dedup_key"))
            .collect();
        assert_eq!(keys, vec!["live".to_string()]);
    }

    #[test]
    fn unjudged_passes_limit_zero_through_without_special_casing() {
        let engine = open_memory(config());
        engine.begin_immediate().unwrap();
        engine
            .insert_candidates(&[base_row("k1"), base_row("k2")], None)
            .unwrap();
        engine.commit().unwrap();
        assert!(engine
            .unjudged("judge", 1, true, Some(0), None)
            .unwrap()
            .is_empty());
        assert_eq!(
            engine.unjudged("judge", 1, true, None, None).unwrap().len(),
            2
        );
    }

    #[test]
    fn record_verdict_upgrades_summary_to_full_and_reports_change() {
        let engine = open_memory(config());
        engine.begin_immediate().unwrap();
        engine.insert_candidates(&[base_row("k1")], None).unwrap();
        engine.commit().unwrap();
        engine.begin_immediate().unwrap();
        let first = engine
            .record_verdict(
                "k1", "judge", 1, "sonnet", "c", false, "preview", 0.2, "r", None, "summary", "t1",
            )
            .unwrap();
        engine.commit().unwrap();
        assert!(first);
        engine.begin_immediate().unwrap();
        let upgrade = engine
            .record_verdict(
                "k1", "judge", 1, "opus", "c2", true, "hydrated", 0.9, "r2", None, "full", "t2",
            )
            .unwrap();
        let noop = engine
            .record_verdict(
                "k1", "judge", 1, "opus", "c3", true, "again", 0.9, "r3", None, "full", "t3",
            )
            .unwrap();
        engine.commit().unwrap();
        assert!(upgrade);
        assert!(!noop);
        let judged = engine.judged("judge", 1).unwrap();
        assert!(matches!(cell(&judged[0], "summary"), Some(SqlCell::Text(s)) if s == "hydrated"));
    }

    #[test]
    fn executescript_refused_while_a_transaction_is_open() {
        let engine = open_memory(config());
        engine.begin_immediate().unwrap();
        let refused = engine.executescript("CREATE TABLE probe(x);");
        assert!(matches!(
            refused,
            Err(LedgerError::Sqlite {
                class: SqliteErrorClass::Programming,
                ..
            })
        ));
        engine.rollback().unwrap();
        engine.executescript("CREATE TABLE probe(x);").unwrap();
    }

    #[test]
    fn sql_binds_and_reads_back_bytes() {
        let engine = open_memory(config());
        engine
            .executescript("CREATE TABLE blobs(id INTEGER, v BLOB);")
            .unwrap();
        engine
            .execute(
                "INSERT INTO blobs(id, v) VALUES (?, ?)",
                &[SqlCell::Int(1), SqlCell::Blob(vec![0u8, 1, 2, 255])],
            )
            .unwrap();
        let row = engine
            .sql("SELECT v FROM blobs WHERE id = ?", &[SqlCell::Int(1)])
            .unwrap();
        assert!(matches!(cell(&row[0], "v"), Some(SqlCell::Blob(b)) if b == vec![0u8, 1, 2, 255]));
    }

    #[test]
    fn readonly_open_rejects_writes() {
        let path = temp_path("readonly");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        let mut ro = config();
        ro.readonly = true;
        let engine = FeedbackEngine::open(&path, ro).unwrap();
        assert!(engine
            .sql("SELECT COUNT(*) FROM feedback_events", &[])
            .is_ok());
        assert!(engine
            .execute("INSERT INTO files(path, mtime) VALUES ('x', 1.0)", &[])
            .is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn v8_verdict_table_fails_open_with_verdict_schema_error() {
        let path = temp_path("v8");
        std::fs::remove_file(&path).ok();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE verdicts (dedup_key TEXT, role TEXT, prompt_version INTEGER, \
                 model TEXT, category TEXT, accepted INTEGER, summary TEXT, confidence REAL, \
                 rationale TEXT, fidelity TEXT, judged_at TEXT, \
                 UNIQUE(dedup_key, role, prompt_version, model));",
            )
            .unwrap();
        }
        assert!(matches!(
            FeedbackEngine::open(&path, config()),
            Err(LedgerError::VerdictSchema { .. })
        ));
        let readonly = FeedbackConfig {
            readonly: true,
            ..config()
        };
        assert!(matches!(
            FeedbackEngine::open(&path, readonly),
            Err(LedgerError::VerdictSchema { .. })
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn record_file_and_file_mtimes_roundtrip() {
        let engine = open_memory(config());
        engine.record_file("/a.jsonl", 1.5).unwrap();
        engine.record_file("/a.jsonl", 2.5).unwrap();
        let rows = engine.file_mtimes().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(cell(&rows[0], "mtime"), Some(SqlCell::Real(f)) if f == 2.5));
    }
}
