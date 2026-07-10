"""The SQLite feedback store, layered on cc-transcript's file-state ledger."""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Self

from cc_transcript.mining.confidence import to_payload
from cc_transcript.store import FileStateStore

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from pathlib import Path
    from types import TracebackType

    from cc_transcript.mining.candidates import FeedbackCandidate
    from cc_transcript.mining.sourcekind import SourceKind

FEEDBACK_DDL = """
CREATE TABLE IF NOT EXISTS feedback_events (
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
CREATE INDEX IF NOT EXISTS idx_feedback_source ON feedback_events(source_kind);
CREATE INDEX IF NOT EXISTS idx_feedback_session ON feedback_events(session_id);
"""

INSERT_EVENT = """
INSERT OR IGNORE INTO feedback_events (
  dedup_key, source_kind, session_id, event_uuid,
  occurred_at, text, payload_json, context_json, cc_version, ingested_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""


def now() -> str:
    return datetime.now(UTC).isoformat()


def event_row(candidate: FeedbackCandidate, ingested_at: str) -> tuple[object, ...]:
    payload = dict(candidate.payload or {})
    payload["signal"] = to_payload(candidate.signal)
    return (
        candidate.dedup_key,
        candidate.source_kind,
        candidate.session_id,
        candidate.ref.event_uuid,
        candidate.occurred_at.isoformat(),
        candidate.text,
        json.dumps(payload),
        candidate.window.to_json(),
        candidate.cc_version,
        ingested_at,
    )


@dataclass(frozen=True, slots=True)
class Stats:
    """A snapshot of ingestion progress.

    Attributes:
        total: The total feedback events ingested.
        files: The number of scanned files recorded.
        by_source: Event counts keyed by source kind.
    """

    total: int
    files: int
    by_source: Mapping[str, int]


class FeedbackStore:
    """Persistent store for collected feedback over a :class:`FileStateStore`.

    Layers the ``feedback_events`` table onto cc-transcript's file-mtime ledger.
    Recording a scanned file and inserting its candidates commit in one
    transaction, so a scan is atomic: it either records the file and all its
    candidates or neither.

    Example:
        >>> async with await FeedbackStore.open(Path("feedback.db")) as store:
        ...     await store.record_file_scan(str(path), mtime, candidates)
    """

    def __init__(self, store: FileStateStore) -> None:
        self.store = store

    @classmethod
    async def open(cls, path: Path) -> Self:
        """Opens (creating if needed) the feedback database at ``path``."""
        return cls(await FileStateStore.open(path, extra_schema=FEEDBACK_DDL))

    async def close(self) -> None:
        """Closes the underlying store."""
        await self.store.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.close()

    async def file_mtimes(self) -> dict[str, float]:
        """Returns the recorded ``path`` to ``mtime`` map for incremental scans."""
        return await self.store.file_mtimes()

    async def record_file_scan(self, path: str, mtime: float, candidates: Sequence[FeedbackCandidate]) -> int:
        """Records a scanned file and its candidates in one transaction.

        Inserts every candidate with ``INSERT OR IGNORE`` keyed by its dedup key
        and upserts the file's mtime, so re-scanning an unchanged file is a no-op.

        Args:
            path: The scanned file's path.
            mtime: The file's modification time at scan.
            candidates: The candidates extracted from the file.

        Returns:
            The number of newly inserted feedback events.
        """
        ingested_at = now()
        async with self.store.transaction() as conn:
            before = conn.total_changes
            await conn.executemany(INSERT_EVENT, [event_row(candidate, ingested_at) for candidate in candidates])
            inserted = conn.total_changes - before
            await self.store.record_file(path, mtime)
            return inserted

    async def stats(self) -> Stats:
        """Returns ingestion counts by source kind and the scanned-file count."""
        conn = self.store.conn
        total_cur = await conn.execute("SELECT COUNT(*) AS n FROM feedback_events")
        files_cur = await conn.execute("SELECT COUNT(*) AS n FROM files")
        by_source_cur = await conn.execute(
            "SELECT source_kind, COUNT(*) AS n FROM feedback_events GROUP BY source_kind ORDER BY source_kind"
        )
        total_row, files_row = await total_cur.fetchone(), await files_cur.fetchone()
        assert total_row is not None and files_row is not None, "COUNT(*) always returns one row"
        return Stats(
            total=total_row["n"],
            files=files_row["n"],
            by_source={row["source_kind"]: row["n"] async for row in by_source_cur},
        )

    async def recent(self, *, source_kind: SourceKind | None = None, limit: int = 20) -> list[dict[str, object]]:
        """Returns the most recent feedback events, newest first.

        Args:
            source_kind: When set, restrict to this source kind.
            limit: The maximum number of rows to return.

        Returns:
            One dict per event with its ``source_kind``, ``occurred_at``, and ``text``.
        """
        query = "SELECT source_kind, occurred_at, text FROM feedback_events"
        params: tuple[object, ...] = ()
        if source_kind is not None:
            query += " WHERE source_kind = ?"
            params = (source_kind,)
        query += " ORDER BY occurred_at DESC, id DESC LIMIT ?"
        cur = await self.store.conn.execute(query, (*params, limit))
        return [dict(row) async for row in cur]

    async def events(self, *, source_kind: SourceKind | None = None) -> list[dict[str, object]]:
        """Returns every feedback event, newest first, with the columns needed to render it.

        Unlike :meth:`recent`, this returns the full row — payload and context — and
        applies no limit, so a caller can render the whole corpus in one pass.

        Args:
            source_kind: When set, restrict to this source kind.

        Returns:
            One dict per event with its ``id``, ``source_kind``, ``occurred_at``,
            ``text``, ``payload_json``, ``context_json``, ``event_uuid``, and
            ``session_id``.
        """
        query = (
            "SELECT id, source_kind, occurred_at, text, payload_json, context_json, event_uuid, session_id "
            "FROM feedback_events"
        )
        params: tuple[object, ...] = ()
        if source_kind is not None:
            query += " WHERE source_kind = ?"
            params = (source_kind,)
        query += " ORDER BY occurred_at DESC, id DESC"
        cur = await self.store.conn.execute(query, params)
        return [dict(row) async for row in cur]
