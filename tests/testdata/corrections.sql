CREATE TABLE corrections (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    anchor_uuid TEXT NOT NULL,
    incorrect_digest TEXT,
    incorrect_file TEXT NOT NULL,
    incorrect_old TEXT NOT NULL,
    incorrect_new TEXT NOT NULL,
    correction_origin TEXT CHECK (correction_origin IN ('session', 'git', 'review')),
    correction_file TEXT,
    correction_old TEXT,
    correction_new TEXT,
    correction_commit TEXT,
    correction_text TEXT,
    overlap REAL NOT NULL DEFAULT 0,
    detail_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (session_id, anchor_uuid, incorrect_digest)
);

CREATE INDEX idx_corrections_session_ts ON corrections (session_id, ts_ms);

CREATE INDEX idx_corrections_incorrect_digest ON corrections (incorrect_digest);
