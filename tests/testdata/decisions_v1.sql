CREATE TABLE IF NOT EXISTS decisions_v1 (
    id INTEGER PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_file TEXT NOT NULL DEFAULT '',
    event TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('allow', 'block', 'warn', 'nudge', 'note')),
    tool_name TEXT,
    tool_digest TEXT,
    event_uuid TEXT,
    message TEXT,
    detail_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (session_id, ts_ms, source, kind, tool_digest)
);

CREATE INDEX IF NOT EXISTS idx_decisions_v1_session_ts ON decisions_v1 (session_id, ts_ms);

CREATE INDEX IF NOT EXISTS idx_decisions_v1_tool_digest ON decisions_v1 (tool_digest);

CREATE INDEX IF NOT EXISTS idx_decisions_v1_source_file ON decisions_v1 (source_file);
