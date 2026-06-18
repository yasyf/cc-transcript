CREATE TABLE IF NOT EXISTS decisions (
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

CREATE INDEX IF NOT EXISTS idx_decisions_session_ts ON decisions (session_id, ts_ms);

CREATE INDEX IF NOT EXISTS idx_decisions_tool_digest ON decisions (tool_digest);

CREATE INDEX IF NOT EXISTS idx_decisions_source_file ON decisions (source_file);
