CREATE TABLE cc_review_decisions_schema_v1 (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    component TEXT NOT NULL CHECK (component = 'cc-review-decisions-v1'),
    schema_version INTEGER NOT NULL CHECK (schema_version = 1),
    ddl_fingerprint TEXT NOT NULL,
    object_fingerprint TEXT NOT NULL
);

CREATE TABLE decisions (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_file TEXT NOT NULL,
    event TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('allow', 'block', 'warn', 'nudge', 'note')),
    tool_name TEXT,
    tool_digest TEXT,
    event_uuid TEXT,
    message TEXT,
    detail_json TEXT NOT NULL,
    UNIQUE (session_id, ts_ms, source, kind, tool_digest)
);

CREATE INDEX idx_decisions_session_ts ON decisions (session_id, ts_ms);

CREATE INDEX idx_decisions_tool_digest ON decisions (tool_digest);

CREATE INDEX idx_decisions_source_file ON decisions (source_file);

CREATE TABLE dispatch_heartbeats (
    session_id TEXT NOT NULL,
    event TEXT NOT NULL,
    first_ts_ms INTEGER NOT NULL,
    last_ts_ms INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (session_id, event)
);

CREATE INDEX idx_heartbeats_session ON dispatch_heartbeats (session_id);
