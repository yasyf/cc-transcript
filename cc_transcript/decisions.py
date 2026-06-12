"""The unified decision ledger shared by every hook and gate writer.

One SQLite table (``decisions_v1``) records every allow/block/warn/nudge/note
decision across the family. Python (:class:`DecisionLog`) and cc-review's Go
daemon write the same file directly, so the DDL below is vendored
byte-identical into the Go repo and byte-compared in CI. Attribution joins by
tool digest and time window — never by message text.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Literal, Self

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

    from cc_transcript.ids import EventUuid, SessionId, ToolDigest

DECISIONS_DDL = """\
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
"""

Action = Literal["allow", "block", "warn", "nudge", "note"]

DECISION_COLUMNS = (
    "ts_ms, session_id, source, kind, source_file, event, action, "
    "tool_name, tool_digest, event_uuid, message, detail_json"
)


@dataclass(frozen=True, slots=True)
class Decision:
    """One row of the decision ledger.

    Attributes:
        ts_ms: Integer-millisecond timestamp of the decision; part of the
            UNIQUE key, so re-running a writer is exactly idempotent.
        session_id: The Claude session UUID the decision fired in.
        source: The writing system, e.g. ``captain-hook`` or ``cc-review``.
        kind: The writer's decision taxonomy, e.g. the hook name.
        source_file: The file the deciding hook was registered from; ``""``
            when the writer has no file provenance.
        event: The Claude Code event, e.g. ``PreToolUse`` or ``Stop``.
        action: What the decision did: allow, block, warn, nudge, or note.
        tool_name: The tool under decision, for tool-shaped events.
        tool_digest: The cross-language content digest of the tool call; the
            preferred attribution key.
        event_uuid: The transcript entry uuid, when the writer knows it; the
            preferred disambiguator for repeated identical calls.
        message: The user-visible decision text, if any.
        detail: Structured extras, serialized to ``detail_json``.
    """

    ts_ms: int
    session_id: SessionId
    source: str
    kind: str
    source_file: str
    event: str
    action: Action
    tool_name: str | None = None
    tool_digest: ToolDigest | None = None
    event_uuid: EventUuid | None = None
    message: str | None = None
    detail: Mapping[str, Any] = field(default_factory=dict)


class DecisionLog:
    """The ``decisions_v1`` ledger at ``~/.cc-transcript/decisions.db``.

    Opened in WAL mode with a busy timeout because cc-review's Go daemon
    writes the same file concurrently. Durable by convention: rows are never
    auto-dropped. Requires a local disk — WAL does not work over NFS.

    Example:
        >>> log = DecisionLog.open()
        >>> log.append(decision)
        >>> log.attribute_tool(session_id, tool_digest=digest, near_ts_ms=ts)
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn

    @classmethod
    def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the ledger at ``path``.

        Args:
            path: The database file path; its parents are created if absent.
                Defaults to ``~/.cc-transcript/decisions.db``.

        Returns:
            The opened log.
        """
        path = path or Path.home() / ".cc-transcript" / "decisions.db"
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(path, autocommit=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA busy_timeout = 2000")
        conn.executescript(DECISIONS_DDL)
        return cls(conn)

    def append(self, decision: Decision) -> None:
        """Appends ``decision`` as a single ``INSERT OR IGNORE``.

        Idempotent on the UNIQUE key ``(session_id, ts_ms, source, kind,
        tool_digest)`` when ``tool_digest`` is present; SQLite treats NULL
        digests as distinct, so digestless rows rely on the writer not
        re-running the same integer-ms timestamp.
        """
        self.conn.execute(
            f"INSERT OR IGNORE INTO decisions_v1 ({DECISION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                decision.ts_ms,
                decision.session_id,
                decision.source,
                decision.kind,
                decision.source_file,
                decision.event,
                decision.action,
                decision.tool_name,
                decision.tool_digest,
                decision.event_uuid,
                decision.message,
                json.dumps(dict(decision.detail)),
            ),
        )

    def for_session(self, session_id: SessionId) -> tuple[Decision, ...]:
        """All decisions for ``session_id``, ordered by timestamp."""
        return tuple(
            decision_of(row)
            for row in self.conn.execute(
                "SELECT * FROM decisions_v1 WHERE session_id = ? ORDER BY ts_ms, id", (session_id,)
            )
        )

    def attribute_tool(
        self, session_id: SessionId, *, tool_digest: ToolDigest, near_ts_ms: int, window_ms: int = 300_000
    ) -> Decision | None:
        """The nearest decision preceding ``near_ts_ms`` with this digest.

        Joins by digest equality plus time window only — never by message
        text. Repeated identical calls share a digest, so in tight loops the
        nearest-preceding pick is probabilistic; prefer ``event_uuid`` to
        disambiguate when the row carries one.

        Returns:
            The matching decision, or None when no digest-equal row lies in
            ``[near_ts_ms - window_ms, near_ts_ms]``.
        """
        row = self.conn.execute(
            "SELECT * FROM decisions_v1 WHERE session_id = ? AND tool_digest = ? AND ts_ms BETWEEN ? AND ?"
            " ORDER BY ts_ms DESC, id DESC LIMIT 1",
            (session_id, tool_digest, near_ts_ms - window_ms, near_ts_ms),
        ).fetchone()
        return decision_of(row) if row else None

    def attribute_nearest(
        self,
        session_id: SessionId,
        *,
        event: str,
        near_ts_ms: int,
        kind: str | None = None,
        window_ms: int = 300_000,
    ) -> Decision | None:
        """The decision nearest ``near_ts_ms`` for a digestless event.

        The documented-probabilistic path for Stop, UserPromptSubmit, and
        Notification fires, which carry no tool digest: filters by ``event``
        (and ``kind`` when given), then picks the smallest absolute timestamp
        distance within the window, on either side of ``near_ts_ms``.

        Returns:
            The nearest matching decision, or None when none lies in
            ``[near_ts_ms - window_ms, near_ts_ms + window_ms]``.
        """
        kind_clause, kind_params = ("AND kind = ? ", (kind,)) if kind is not None else ("", ())
        row = self.conn.execute(
            f"SELECT * FROM decisions_v1 WHERE session_id = ? AND event = ? {kind_clause}AND ts_ms BETWEEN ? AND ?"
            " ORDER BY ABS(ts_ms - ?), id DESC LIMIT 1",
            (session_id, event, *kind_params, near_ts_ms - window_ms, near_ts_ms + window_ms, near_ts_ms),
        ).fetchone()
        return decision_of(row) if row else None


def decision_of(row: sqlite3.Row) -> Decision:
    return Decision(
        ts_ms=row["ts_ms"],
        session_id=row["session_id"],
        source=row["source"],
        kind=row["kind"],
        source_file=row["source_file"],
        event=row["event"],
        action=row["action"],
        tool_name=row["tool_name"],
        tool_digest=row["tool_digest"],
        event_uuid=row["event_uuid"],
        message=row["message"],
        detail=json.loads(row["detail_json"]),
    )
