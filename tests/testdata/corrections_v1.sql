CREATE TABLE IF NOT EXISTS corrections_v1 (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    anchor_uuid TEXT NOT NULL,
    incorrect_digest TEXT NOT NULL,
    incorrect_file TEXT NOT NULL,
    incorrect_old TEXT NOT NULL,
    incorrect_new TEXT NOT NULL,
    correction_origin TEXT CHECK (correction_origin IN ('session', 'git')),
    correction_file TEXT,
    correction_old TEXT,
    correction_new TEXT,
    correction_commit TEXT,
    overlap REAL NOT NULL DEFAULT 0,
    extractor_version INTEGER NOT NULL,
    detail_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (session_id, anchor_uuid, incorrect_digest, extractor_version)
);

CREATE INDEX IF NOT EXISTS idx_corrections_v1_session_ts ON corrections_v1 (session_id, ts_ms);

CREATE INDEX IF NOT EXISTS idx_corrections_v1_incorrect_digest ON corrections_v1 (incorrect_digest);
