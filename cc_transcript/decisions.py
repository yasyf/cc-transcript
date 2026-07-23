"""The unified decision ledger shared by every hook and gate writer.

One SQLite table (``decisions``) records every allow/block/warn/nudge/note
decision across the family. Python (:class:`DecisionLog`) and cc-review's Go
daemon write the same file directly, so the DDL below is vendored
byte-identical into the Go repo and byte-compared in CI. Attribution joins by
tool digest and time window — never by message text.
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Literal, Self

from cc_transcript.decision_store import DECISIONS_DDL, open_decisions_sqlite
from cc_transcript.ledger import AsyncLedger, ConnectionActor

if TYPE_CHECKING:
    from collections.abc import Mapping
    from pathlib import Path
    from typing import Any

    from cc_transcript.ids import EventUuid, SessionId, ToolDigest

Action = Literal["allow", "block", "warn", "nudge", "note"]


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


class DecisionLog(AsyncLedger[Decision]):
    """The ``decisions`` ledger at ``~/.cc-transcript/decisions.db``.

    Opened in WAL mode with a busy timeout because cc-review's Go daemon
    writes the same file concurrently. Durable by convention: rows are never
    auto-dropped. Requires a local disk — WAL does not work over NFS.

    Example:
        >>> log = await DecisionLog.open()
        >>> async with log:
        ...     await log.append(decision)
        ...     await log.attribute_tool(session_id, tool_digest=digest, near_ts_ms=ts)
    """

    DDL = DECISIONS_DDL
    FILENAME = "decisions.db"
    TABLE = "decisions"
    COLUMNS = (
        "ts_ms",
        "session_id",
        "source",
        "kind",
        "source_file",
        "event",
        "action",
        "tool_name",
        "tool_digest",
        "event_uuid",
        "message",
        "detail_json",
    )

    @classmethod
    async def open(cls, path: Path | None = None) -> Self:
        """Opens the exact v1 decision ledger at ``path``."""
        actor = ConnectionActor()
        await actor.start(lambda: open_decisions_sqlite(path))
        return cls(actor)

    def row_to_record(self, row: sqlite3.Row) -> Decision:
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

    async def attribute_tool(
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
        conn = self._actor.conn
        row = await self._actor.run(
            lambda: conn.execute(
                "SELECT * FROM decisions WHERE session_id = ? AND tool_digest = ? AND ts_ms BETWEEN ? AND ?"
                " ORDER BY ts_ms DESC, id DESC LIMIT 1",
                (session_id, tool_digest, near_ts_ms - window_ms, near_ts_ms),
            ).fetchone()
        )
        return self.row_to_record(row) if row else None

    async def attribute_nearest(
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
        conn = self._actor.conn
        kind_clause, kind_params = ("AND kind = ? ", (kind,)) if kind is not None else ("", ())
        row = await self._actor.run(
            lambda: conn.execute(
                f"SELECT * FROM decisions WHERE session_id = ? AND event = ? {kind_clause}AND ts_ms BETWEEN ? AND ?"
                " ORDER BY ABS(ts_ms - ?), id DESC LIMIT 1",
                (session_id, event, *kind_params, near_ts_ms - window_ms, near_ts_ms + window_ms, near_ts_ms),
            ).fetchone()
        )
        return self.row_to_record(row) if row else None
