//! Feedback-store DDL: the `files` / `feedback_events` schema, the naming-templated
//! verdict table, and the `unjudged`/`judged` event-column list. Hand-owned; byte-identical
//! to the Python constants (`store.FILE_SCHEMA`, `mining.store.FEEDBACK_DDL`,
//! `judge.verdicts.VERDICT_DDL_TEMPLATE` / `EVENT_COLUMNS`) so the schema goldens match.

pub const FILE_SCHEMA: &str =
    "\nCREATE TABLE files (\n  path TEXT PRIMARY KEY,\n  mtime REAL NOT NULL\n);\n";

pub const FEEDBACK_DDL: &str = "\nCREATE TABLE feedback_events (\n  id INTEGER PRIMARY KEY AUTOINCREMENT,\n  dedup_key TEXT NOT NULL UNIQUE,\n  source_kind TEXT NOT NULL,\n  session_id TEXT,\n  event_uuid TEXT,\n  occurred_at TEXT NOT NULL,\n  text TEXT NOT NULL,\n  payload_json TEXT,\n  context_json TEXT NOT NULL,\n  cc_version TEXT,\n  ingested_at TEXT NOT NULL\n);\nCREATE INDEX idx_feedback_source ON feedback_events(source_kind);\nCREATE INDEX idx_feedback_session ON feedback_events(session_id);\n";

pub const VERDICT_DDL_TEMPLATE: &str = "\nCREATE TABLE {table} (\n  id INTEGER PRIMARY KEY AUTOINCREMENT,\n  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),\n  role TEXT NOT NULL,\n  prompt_version INTEGER NOT NULL,\n  model TEXT NOT NULL,\n  category TEXT NOT NULL,\n  {accepted} INTEGER NOT NULL,\n  {summary} TEXT NOT NULL,\n  confidence REAL NOT NULL,\n  rationale TEXT NOT NULL,\n  canonical_key TEXT,\n  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),\n  judged_at TEXT NOT NULL,\n  UNIQUE(dedup_key, role, prompt_version)\n);\nCREATE INDEX idx_{table}_dedup ON {table}(dedup_key);\n";

pub const EVENT_COLUMNS: &str = "e.id, e.dedup_key, e.source_kind, e.occurred_at, e.text, e.payload_json, e.context_json, e.session_id, e.event_uuid";
