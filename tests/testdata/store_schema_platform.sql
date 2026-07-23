-- index idx_feedback_session (on feedback_events)
CREATE INDEX idx_feedback_session ON feedback_events(session_id);

-- index idx_feedback_source (on feedback_events)
CREATE INDEX idx_feedback_source ON feedback_events(source_kind);

-- index idx_verdicts_dedup (on verdicts)
CREATE INDEX idx_verdicts_dedup ON verdicts(dedup_key);

-- table cc_transcript_schema_v1 (on cc_transcript_schema_v1)
CREATE TABLE cc_transcript_schema_v1 (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  schema_identity TEXT NOT NULL,
  schema_version INTEGER NOT NULL CHECK (schema_version = 1),
  ddl_fingerprint TEXT NOT NULL CHECK (length(ddl_fingerprint) = 64),
  object_fingerprint TEXT NOT NULL CHECK (length(object_fingerprint) = 64)
) STRICT;

-- table feedback_events (on feedback_events)
CREATE TABLE feedback_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL UNIQUE,
  source_kind TEXT NOT NULL,
  session_id TEXT,
  event_uuid TEXT,
  occurred_at TEXT NOT NULL,
  text TEXT NOT NULL,
  payload_json TEXT,
  context_json TEXT NOT NULL,
  cc_version TEXT,
  ingested_at TEXT NOT NULL
);

-- table files (on files)
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  mtime REAL NOT NULL
);

-- table verdicts (on verdicts)
CREATE TABLE verdicts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  accepted INTEGER NOT NULL,
  summary TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  canonical_key TEXT,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version)
);

