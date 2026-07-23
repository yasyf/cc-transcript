from __future__ import annotations

import hashlib
import sqlite3
from pathlib import Path

SCHEMA_COMPONENT = "cc-review-decisions-v1"
SCHEMA_VERSION = 1
EXPECTED_DDL_FINGERPRINT = "6ae938038f3420cdd4a00189b678fb399d60bb7647d009acb0fa9cc4a653040f"
EXPECTED_OBJECT_FINGERPRINT = "a993521f1ae402d85545d9cd841b58c7e9ba755babba32c7d59cc3a97ee17af9"

_SCHEMA_DDL_ACTIONS = frozenset(
    {
        sqlite3.SQLITE_ALTER_TABLE,
        sqlite3.SQLITE_CREATE_INDEX,
        sqlite3.SQLITE_CREATE_TABLE,
        sqlite3.SQLITE_CREATE_TRIGGER,
        sqlite3.SQLITE_CREATE_VIEW,
        sqlite3.SQLITE_CREATE_VTABLE,
        sqlite3.SQLITE_DROP_INDEX,
        sqlite3.SQLITE_DROP_TABLE,
        sqlite3.SQLITE_DROP_TRIGGER,
        sqlite3.SQLITE_DROP_VIEW,
        sqlite3.SQLITE_DROP_VTABLE,
    }
)
_SCHEMA_DML_ACTIONS = frozenset({sqlite3.SQLITE_DELETE, sqlite3.SQLITE_INSERT, sqlite3.SQLITE_UPDATE})
_PROTECTED_SCHEMA_TABLES = frozenset({"cc_review_decisions_schema_v1", "sqlite_master", "sqlite_schema"})
_PROTECTED_PRAGMAS = frozenset({"user_version", "writable_schema"})

DECISIONS_DDL = """\
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
"""


def ddl_fingerprint() -> str:
    return hashlib.sha256(b"cc-review-decisions-ddl-v1\0" + DECISIONS_DDL.encode()).hexdigest()


def object_fingerprint(conn: sqlite3.Connection) -> str:
    digest = hashlib.sha256(b"cc-review-decisions-objects-v1\0")
    for object_type, name, table, statement in conn.execute(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name"
    ):
        for field in (object_type, name, table, statement or ""):
            digest.update(field.encode())
            digest.update(b"\0")
    return digest.hexdigest()


def verify_schema(conn: sqlite3.Connection) -> None:
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version != SCHEMA_VERSION:
        raise RuntimeError(f"decisions schema version {version}, want exactly {SCHEMA_VERSION}")
    row = conn.execute(
        "SELECT component, schema_version, ddl_fingerprint, object_fingerprint "
        "FROM cc_review_decisions_schema_v1 WHERE id=1"
    ).fetchone()
    if row is None:
        raise RuntimeError("decisions schema identity row is missing")
    component, marker_version, stored_ddl, stored_objects = row
    if component != SCHEMA_COMPONENT:
        raise RuntimeError(f"decisions schema component {component!r}, want exactly {SCHEMA_COMPONENT!r}")
    if marker_version != SCHEMA_VERSION:
        raise RuntimeError(f"decisions marker version {marker_version}, want exactly {SCHEMA_VERSION}")
    if stored_ddl != EXPECTED_DDL_FINGERPRINT:
        raise RuntimeError(f"decisions DDL fingerprint {stored_ddl!r}, want exactly {EXPECTED_DDL_FINGERPRINT!r}")
    if stored_objects != EXPECTED_OBJECT_FINGERPRINT:
        raise RuntimeError(
            f"decisions stored object fingerprint {stored_objects!r}, want exactly {EXPECTED_OBJECT_FINGERPRINT!r}"
        )
    if (actual := object_fingerprint(conn)) != EXPECTED_OBJECT_FINGERPRINT:
        raise RuntimeError(f"decisions object fingerprint {actual!r}, want exactly {EXPECTED_OBJECT_FINGERPRINT!r}")


def create_schema(conn: sqlite3.Connection) -> None:
    for statement in DECISIONS_DDL.split(";"):
        if statement := statement.strip():
            conn.execute(statement)
    conn.execute(f"PRAGMA user_version = {SCHEMA_VERSION}")
    if (actual := object_fingerprint(conn)) != EXPECTED_OBJECT_FINGERPRINT:
        raise RuntimeError(f"decisions object fingerprint {actual!r}, want exactly {EXPECTED_OBJECT_FINGERPRINT!r}")
    conn.execute(
        "INSERT INTO cc_review_decisions_schema_v1"
        "(id, component, schema_version, ddl_fingerprint, object_fingerprint) VALUES(1, ?, 1, ?, ?)",
        (SCHEMA_COMPONENT, EXPECTED_DDL_FINGERPRINT, EXPECTED_OBJECT_FINGERPRINT),
    )


def _authorize_exact_schema(
    action: int,
    argument1: str | None,
    argument2: str | None,
    database: str | None,
    _source: str | None,
) -> int:
    if action == sqlite3.SQLITE_ATTACH:
        return sqlite3.SQLITE_DENY
    if database == "main":
        if action in _SCHEMA_DDL_ACTIONS:
            return sqlite3.SQLITE_DENY
        if action in _SCHEMA_DML_ACTIONS and (argument1 or "").casefold() in _PROTECTED_SCHEMA_TABLES:
            return sqlite3.SQLITE_DENY
    if action == sqlite3.SQLITE_PRAGMA and argument2 is not None and (argument1 or "").casefold() in _PROTECTED_PRAGMAS:
        return sqlite3.SQLITE_DENY
    return sqlite3.SQLITE_OK


def open_decisions_sqlite(path: Path | None) -> sqlite3.Connection:
    if ddl_fingerprint() != EXPECTED_DDL_FINGERPRINT:
        raise RuntimeError(
            f"decisions compiled DDL fingerprint {ddl_fingerprint()!r}, want exactly {EXPECTED_DDL_FINGERPRINT!r}"
        )
    db_path = path or Path.home() / ".cc-transcript" / "decisions.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path, autocommit=True)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA busy_timeout = 2000")
    committed = False
    created = False
    try:
        conn.execute("BEGIN IMMEDIATE")
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        app_objects = conn.execute(
            "SELECT count(*) FROM sqlite_schema "
            "WHERE type IN ('table', 'index', 'trigger', 'view') "
            "AND lower(substr(name, 1, 7)) <> 'sqlite_'"
        ).fetchone()[0]
        created = version == 0 and app_objects == 0
        create_schema(conn) if created else verify_schema(conn)
        conn.execute("COMMIT")
        committed = True
    finally:
        if not committed:
            if conn.in_transaction:
                conn.execute("ROLLBACK")
            conn.close()
    if created:
        db_path.chmod(0o600)
    if (journal_mode := conn.execute("PRAGMA journal_mode = WAL").fetchone()[0]) != "wal":
        conn.close()
        raise RuntimeError(f"enable decisions WAL: mode {journal_mode!r}")
    conn.set_authorizer(_authorize_exact_schema)
    return conn
