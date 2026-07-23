//! The native feedback-store engine — one connection to `feedback.db`, corrections-pattern.
//! The Python facade keeps `_txn_owner` over the bare txn control; DDL lives in `literals`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use crate::literals::feedback::EVENT_COLUMNS;
use crate::schema;
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

/// The one exact schema and query policy for a feedback store.
#[derive(Debug, Clone)]
pub struct FeedbackConfig {
    pub schema_identity: String,
    pub schema_ddl: String,
    pub event_columns: Vec<String>,
    pub extension_paths: Vec<String>,
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
    /// Opens or creates exactly one v1 store. Existing databases are never altered.
    pub fn open(path: &Path, config: FeedbackConfig) -> Result<Self, LedgerError> {
        if path.as_os_str().is_empty() {
            return Err(sqlite_error(
                rusqlite::ffi::SQLITE_CANTOPEN,
                "unable to open database file".to_string(),
            ));
        }
        validate_schema_config(&config)?;
        let exact_schema = schema::compile(&config.schema_ddl, &config.extension_paths)?;
        for name in [
            &config.verdict_table,
            &config.accepted_column,
            &config.summary_column,
        ]
        .into_iter()
        .chain(config.event_columns.iter())
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
        schema::load_extensions(&conn, &config.extension_paths)?;
        if config.readonly {
            conn.execute_batch("PRAGMA query_only = ON")?;
            schema::validate(&conn, &config.schema_identity, &exact_schema)?;
        } else {
            conn.execute_batch("PRAGMA journal_mode = WAL")?;
            schema::initialize_or_validate(&conn, &config.schema_identity, &exact_schema)?;
        }
        schema::install_guard(&conn);
        Ok(Self {
            conn,
            verdict_table: config.verdict_table,
            accepted_column: config.accepted_column,
            summary_column: config.summary_column,
            event_filter: config.event_filter,
            event_column_names: config.event_columns,
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

    /// Ranks canonical keys by their closest evidence vector's cosine similarity to the
    /// serialized `query` embedding, scoped to `prompt_version`. `vec_distance_cosine`
    /// resolves on the sqlite-vec extension the facade loads onto this connection.
    // Parity: similar.py suggest_canonical_keys
    pub fn suggest_canonical_keys(
        &self,
        query: &[u8],
        prompt_version: i64,
        k: usize,
    ) -> Result<Vec<(String, f64, Vec<String>)>, LedgerError> {
        let rows = self.sql(
            "SELECT e.canonical_key AS ck, e.evidence_text AS ev, \
             vec_distance_cosine(v.embedding, ?) AS dist \
             FROM verdict_vectors v JOIN verdict_evidence e ON e.vector_id = v.vector_id \
             WHERE e.prompt_version = ? ORDER BY dist",
            &[SqlCell::Blob(query.to_vec()), SqlCell::Int(prompt_version)],
        )?;
        let mut ranked: Vec<(String, f64, Vec<String>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in &rows {
            let ck = cell_text(row, "ck")
                .ok_or_else(|| vec_error("suggest row missing canonical_key"))?;
            let ev = cell_text(row, "ev")
                .ok_or_else(|| vec_error("suggest row missing evidence_text"))?;
            let score = 1.0
                - cell_real(row, "dist")
                    .ok_or_else(|| vec_error("suggest row missing distance"))?;
            match index.get(&ck) {
                Some(&i) => {
                    if ranked[i].2.len() < 3 {
                        ranked[i].2.push(ev);
                    }
                }
                None => {
                    index.insert(ck.clone(), ranked.len());
                    ranked.push((ck, score, vec![ev]));
                }
            }
        }
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        ranked.truncate(k);
        Ok(ranked)
    }

    /// Every pair of distinct canonical keys whose normalized evidence-centroid cosine
    /// similarity strictly exceeds `threshold`, scoped to `prompt_version`. Centroids
    /// accumulate and normalize in f64; pairs iterate over lexicographically sorted keys.
    // Parity: similar.py near_duplicate_keys
    pub fn near_duplicate_keys(
        &self,
        prompt_version: i64,
        threshold: f64,
    ) -> Result<Vec<(String, String, f64)>, LedgerError> {
        let rows = self.sql(
            "SELECT e.canonical_key AS ck, v.embedding AS emb \
             FROM verdict_vectors v JOIN verdict_evidence e ON e.vector_id = v.vector_id \
             WHERE e.prompt_version = ?",
            &[SqlCell::Int(prompt_version)],
        )?;
        let mut sums: HashMap<String, (Vec<f64>, usize)> = HashMap::new();
        let mut dim: Option<usize> = None;
        for row in &rows {
            let ck = cell_text(row, "ck")
                .ok_or_else(|| vec_error("near-duplicate row missing canonical_key"))?;
            let blob = cell_blob(row, "emb")
                .ok_or_else(|| vec_error("near-duplicate row missing embedding"))?;
            if blob.len() % 4 != 0 {
                return Err(vec_error(&format!(
                    "verdict embedding blob length {} is not a multiple of 4",
                    blob.len()
                )));
            }
            let vector: Vec<f32> = blob
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            match dim {
                None => dim = Some(vector.len()),
                Some(d) if d != vector.len() => {
                    return Err(vec_error(&format!(
                        "verdict embedding dimension {} disagrees with sibling dimension {}",
                        vector.len(),
                        d
                    )))
                }
                Some(_) => {}
            }
            let entry = sums
                .entry(ck)
                .or_insert_with(|| (vec![0.0_f64; vector.len()], 0));
            for (sum, x) in entry.0.iter_mut().zip(&vector) {
                *sum += f64::from(*x);
            }
            entry.1 += 1;
        }
        let centroids: HashMap<String, Vec<f64>> = sums
            .into_iter()
            .map(|(ck, (sum, count))| {
                let mean: Vec<f64> = sum.iter().map(|s| s / count as f64).collect();
                let norm = mean.iter().map(|m| m * m).sum::<f64>().sqrt();
                (ck, mean.iter().map(|m| m / norm).collect())
            })
            .collect();
        let mut keys: Vec<String> = centroids.keys().cloned().collect();
        keys.sort();
        let mut overlaps: Vec<(String, String, f64)> = Vec::new();
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                let similarity: f64 = centroids[&keys[i]]
                    .iter()
                    .zip(&centroids[&keys[j]])
                    .map(|(x, y)| x * y)
                    .sum();
                if similarity > threshold {
                    overlaps.push((keys[i].clone(), keys[j].clone(), similarity));
                }
            }
        }
        overlaps.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
        Ok(overlaps)
    }
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

fn validate_schema_config(config: &FeedbackConfig) -> Result<(), LedgerError> {
    if config.schema_identity.is_empty() || config.schema_identity.len() > 256 {
        return Err(config_error(
            "feedback schema identity must contain 1..=256 bytes".to_string(),
        ));
    }
    if config.schema_ddl.trim().is_empty() {
        return Err(config_error("feedback schema DDL is required".to_string()));
    }
    let normalized = config.schema_ddl.to_ascii_uppercase();
    for forbidden in [
        "IF NOT EXISTS",
        "ALTER TABLE",
        "DROP TABLE",
        "DROP INDEX",
        "DROP TRIGGER",
        "DROP VIEW",
        "PRAGMA USER_VERSION",
        "CC_TRANSCRIPT_SCHEMA_V1",
    ] {
        if normalized.contains(forbidden) {
            return Err(config_error(format!(
                "feedback schema DDL contains forbidden '{forbidden}'"
            )));
        }
    }
    for name in &config.event_columns {
        validate_identifier(name)?;
    }
    if config.extension_paths.iter().any(|path| path.is_empty()) {
        return Err(config_error(
            "feedback schema extension path must not be empty".to_string(),
        ));
    }
    Ok(())
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

fn cell_real(row: &SqlRow, name: &str) -> Option<f64> {
    match cell(row, name) {
        Some(SqlCell::Real(r)) => Some(r),
        _ => None,
    }
}

fn cell_blob(row: &SqlRow, name: &str) -> Option<Vec<u8>> {
    match cell(row, name) {
        Some(SqlCell::Blob(b)) => Some(b),
        _ => None,
    }
}

fn vec_error(message: &str) -> LedgerError {
    LedgerError::Sqlite {
        class: SqliteErrorClass::Data,
        message: message.to_string(),
        code: None,
        name: None,
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
    use crate::literals::feedback::{FEEDBACK_DDL, FILE_SCHEMA, VERDICT_DDL_TEMPLATE};

    fn verdict_ddl(table: &str, accepted: &str, summary: &str) -> String {
        VERDICT_DDL_TEMPLATE
            .replace("{table}", table)
            .replace("{accepted}", accepted)
            .replace("{summary}", summary)
    }

    fn config() -> FeedbackConfig {
        FeedbackConfig {
            schema_identity: "cc-transcript-feedback-test".to_string(),
            schema_ddl: format!(
                "{FILE_SCHEMA}{FEEDBACK_DDL}{}",
                verdict_ddl("verdicts", "accepted", "summary")
            ),
            event_columns: vec![],
            extension_paths: vec![],
            verdict_table: "verdicts".to_string(),
            accepted_column: "accepted".to_string(),
            summary_column: "summary".to_string(),
            event_filter: None,
            readonly: false,
            busy_timeout_ms: 5000,
        }
    }

    fn steer_config() -> FeedbackConfig {
        let feedback = FEEDBACK_DDL.replacen(
            "  ingested_at TEXT NOT NULL\n",
            "  ingested_at TEXT NOT NULL,\n  origin_path TEXT,\n  quarantined_reason TEXT\n",
            1,
        );
        FeedbackConfig {
            schema_identity: "cc-transcript-steer-test".to_string(),
            schema_ddl: format!(
                "{FILE_SCHEMA}{feedback}{}",
                verdict_ddl("triage", "is_steering", "what_claude_did")
            ),
            event_columns: vec!["origin_path".to_string(), "quarantined_reason".to_string()],
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

    fn assert_open_schema_error(path: &Path, config: FeedbackConfig) {
        let Err(error) = FeedbackEngine::open(path, config) else {
            panic!("open accepted a non-exact schema");
        };
        assert!(error.to_string().contains("schema"), "{error}");
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
    fn different_exact_schema_is_rejected_without_mutation() {
        let path = temp_path("schema-mismatch");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        let Err(error) = FeedbackEngine::open(&path, steer_config()) else {
            panic!("open accepted a different exact schema");
        };
        assert!(error.to_string().contains("schema"), "{error}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ddl_bytes_are_part_of_the_exact_schema_identity() {
        let path = temp_path("ddl-fingerprint");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        let changed = FeedbackConfig {
            schema_ddl: format!(
                "-- same objects, different authoritative DDL\n{}",
                config().schema_ddl
            ),
            ..config()
        };
        assert_open_schema_error(&path, changed);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extra_missing_and_altered_objects_are_rejected() {
        for (tag, mutation) in [
            ("extra", "CREATE TABLE unexpected(id INTEGER);"),
            ("missing", "DROP INDEX idx_verdicts_dedup;"),
            (
                "altered",
                "DROP INDEX idx_verdicts_dedup; CREATE INDEX idx_verdicts_dedup ON verdicts(role);",
            ),
            (
                "sqlite-prefix",
                "CREATE TABLE sqliteX_not_internal(id INTEGER);",
            ),
        ] {
            let path = temp_path(tag);
            std::fs::remove_file(&path).ok();
            FeedbackEngine::open(&path, config()).unwrap();
            Connection::open(&path)
                .unwrap()
                .execute_batch(mutation)
                .unwrap();
            assert_open_schema_error(&path, config());
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn marker_and_user_version_spoofing_are_rejected() {
        for (tag, mutation) in [
            (
                "marker-spoof",
                "UPDATE cc_transcript_schema_v1 SET ddl_fingerprint = printf('%064d', 0);",
            ),
            ("version-spoof", "PRAGMA user_version = 2;"),
        ] {
            let path = temp_path(tag);
            std::fs::remove_file(&path).ok();
            FeedbackEngine::open(&path, config()).unwrap();
            Connection::open(&path)
                .unwrap()
                .execute_batch(mutation)
                .unwrap();
            assert_open_schema_error(&path, config());
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn sqlite_internal_analyze_objects_do_not_change_application_identity() {
        let path = temp_path("analyze");
        std::fs::remove_file(&path).ok();
        FeedbackEngine::open(&path, config()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch("ANALYZE;")
            .unwrap();
        FeedbackEngine::open(&path, config()).unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn compatibility_ddl_is_rejected() {
        for ddl in [
            "CREATE TABLE IF NOT EXISTS probe(id INTEGER);",
            "ALTER TABLE feedback_events ADD COLUMN probe TEXT;",
            "DROP VIEW probe;",
            "PRAGMA user_version = 1;",
        ] {
            let cfg = FeedbackConfig {
                schema_ddl: ddl.to_string(),
                ..config()
            };
            let Err(error) = FeedbackEngine::open(Path::new(":memory:"), cfg) else {
                panic!("open accepted compatibility DDL: {ddl}");
            };
            assert!(
                matches!(
                    error,
                    LedgerError::Sqlite {
                        class: SqliteErrorClass::Programming,
                        ..
                    }
                ),
                "{ddl}"
            );
        }
    }

    #[test]
    fn open_connection_cannot_mutate_its_exact_schema_attestation() {
        let engine = open_memory(config());
        for statement in [
            "CREATE TABLE probe(id INTEGER)",
            "UPDATE cc_transcript_schema_v1 SET schema_identity = 'spoofed'",
            "PRAGMA user_version = 2",
            "PRAGMA writable_schema = ON",
            "UPDATE sqlite_schema SET sql = 'spoofed' WHERE name = 'files'",
            "ATTACH DATABASE ':memory:' AS attached",
        ] {
            assert!(engine.execute(statement, &[]).is_err(), "{statement}");
        }
        assert!(engine
            .executescript(
                "PRAGMA writable_schema = ON; \
                 UPDATE sqlite_schema SET sql = 'spoofed' WHERE name = 'files';"
            )
            .is_err());
    }

    #[test]
    fn open_connection_cannot_attach_its_owned_database_under_an_alias() {
        let path = temp_path("same-file-attach");
        std::fs::remove_file(&path).ok();
        let engine = FeedbackEngine::open(&path, config()).unwrap();
        let quoted = path.display().to_string().replace('\'', "''");
        let statement = format!("ATTACH DATABASE '{quoted}' AS samefile");
        assert!(engine.execute(&statement, &[]).is_err());
        drop(engine);
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
                event_columns: vec!["a,b".to_string()],
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
                    FeedbackEngine::open(&path, config()).map(|_| ())
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
        engine
            .executescript("INSERT INTO files(path, mtime) VALUES ('/probe', 1);")
            .unwrap();
    }

    #[test]
    fn sql_binds_and_reads_back_bytes() {
        let engine = open_memory(config());
        let row = engine
            .sql("SELECT ? AS v", &[SqlCell::Blob(vec![0u8, 1, 2, 255])])
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
    fn arbitrary_existing_database_is_rejected_as_not_exact_v1() {
        let path = temp_path("foreign-schema");
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
        let Err(error) = FeedbackEngine::open(&path, config()) else {
            panic!("open accepted a foreign database");
        };
        assert!(error.to_string().contains("schema"), "{error}");
        let readonly = FeedbackConfig {
            readonly: true,
            ..config()
        };
        let Err(error) = FeedbackEngine::open(&path, readonly) else {
            panic!("readonly open accepted a foreign database");
        };
        assert!(error.to_string().contains("schema"), "{error}");
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
