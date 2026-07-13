"""Per-(session, event) dispatch heartbeats — proof a hook event reached dispatch at all.

The decision ledger records only hooks that *fired*, so it cannot distinguish "this event
was never dispatched" from "it dispatched and matched nothing". A heartbeat is written
unconditionally at dispatch entry, before matching, keyed by ``(session_id, event)`` and
counting hits — so a missing beat for an event a session should emit is an unambiguous
wiring gap. It shares the ``decisions.db`` file with
:class:`~cc_transcript.decisions.DecisionLog` in a separate ``dispatch_heartbeats`` table,
so the vendored, Go-byte-compared ``decisions`` schema is untouched. The upsert keeps at
most one row per ``(session, event)``, so the table stays small however hot the path.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Self

if TYPE_CHECKING:
    from cc_transcript.ids import SessionId

HEARTBEATS_DDL = """\
CREATE TABLE IF NOT EXISTS dispatch_heartbeats (
    session_id TEXT NOT NULL,
    event TEXT NOT NULL,
    first_ts_ms INTEGER NOT NULL,
    last_ts_ms INTEGER NOT NULL,
    count INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (session_id, event)
);

CREATE INDEX IF NOT EXISTS idx_heartbeats_session ON dispatch_heartbeats (session_id);
"""

FILENAME = "decisions.db"


@dataclass(frozen=True, slots=True)
class Heartbeat:
    """One ``(session, event)`` dispatch heartbeat.

    Attributes:
        session_id: The Claude session the event dispatched in.
        event: The Claude Code event name, e.g. ``PreToolUse`` or ``Stop``.
        first_ts_ms: Millisecond timestamp of the first dispatch this session.
        last_ts_ms: Millisecond timestamp of the most recent dispatch.
        count: How many times the event has dispatched this session.
    """

    session_id: SessionId
    event: str
    first_ts_ms: int
    last_ts_ms: int
    count: int


class HeartbeatLog:
    """The ``dispatch_heartbeats`` table in ``~/.cc-transcript/decisions.db``.

    Opened in WAL mode with a busy timeout, matching the decision ledger it shares a file
    with. ``beat`` upserts one ``(session, event)`` row per dispatch, so a session holds at
    most one row per event. Requires a local disk — WAL does not work over NFS.

    Example:
        >>> log = HeartbeatLog.open()
        >>> log.beat(session_id, "PreToolUse", ts_ms)
        >>> log.for_session(session_id)
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn

    @classmethod
    def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the heartbeat table at ``path`` (defaults to ``decisions.db``)."""
        path = path or Path.home() / ".cc-transcript" / FILENAME
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(path, autocommit=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA busy_timeout = 2000")
        conn.executescript(HEARTBEATS_DDL)
        return cls(conn)

    def beat(self, session_id: SessionId, event: str, ts_ms: int) -> None:
        """Records one dispatch of ``event`` in ``session_id`` at ``ts_ms`` (upsert; ``count += 1``)."""
        self.conn.execute(
            "INSERT INTO dispatch_heartbeats (session_id, event, first_ts_ms, last_ts_ms, count) "
            "VALUES (?, ?, ?, ?, 1) "
            "ON CONFLICT (session_id, event) DO UPDATE SET last_ts_ms = excluded.last_ts_ms, count = count + 1",
            (session_id, event, ts_ms, ts_ms),
        )

    def for_session(self, session_id: SessionId) -> tuple[Heartbeat, ...]:
        """Every ``(session, event)`` beat for ``session_id``, ordered by first dispatch."""
        return tuple(
            Heartbeat(
                session_id=row["session_id"],
                event=row["event"],
                first_ts_ms=row["first_ts_ms"],
                last_ts_ms=row["last_ts_ms"],
                count=row["count"],
            )
            for row in self.conn.execute(
                "SELECT * FROM dispatch_heartbeats WHERE session_id = ? ORDER BY first_ts_ms, event", (session_id,)
            )
        )

    def events_seen(self, session_id: SessionId) -> frozenset[str]:
        """The set of event names that have dispatched at least once in ``session_id``."""
        return frozenset(
            row["event"]
            for row in self.conn.execute(
                "SELECT event FROM dispatch_heartbeats WHERE session_id = ?", (session_id,)
            )
        )
