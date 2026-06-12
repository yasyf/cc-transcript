"""The shared code-correction ledger every consumer reads and writes.

One SQLite table (``corrections_v1``) records each incorrect edit a developer
pushed back on paired with the correction that later overwrote it — from the
same session or from git history. cc-pushback writes it from its enrich pass;
captain-hook and cc-review join it to their own records by ``incorrect_digest``,
the same cross-language tool digest the decision ledger keys on. The DDL below
is vendored byte-identical into the Go repo and byte-compared in CI.

Import-light by contract, like :mod:`cc_transcript.ids` and
:mod:`cc_transcript.decisions`: the standard library plus identity primitives
only, so a hook reading corrections pays nothing for the parser. The harvest
that produces rows lives in :func:`cc_transcript.evidence.record_harvest`,
which is allowed the heavier activity imports.
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

CORRECTIONS_DDL = """\
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
"""

Origin = Literal["session", "git"]

CORRECTION_COLUMNS = (
    "ts_ms, session_id, source, anchor_uuid, incorrect_digest, incorrect_file, incorrect_old, "
    "incorrect_new, correction_origin, correction_file, correction_old, correction_new, "
    "correction_commit, overlap, extractor_version, detail_json"
)


@dataclass(frozen=True, slots=True)
class Correction:
    """One incorrect edit and the correction that later overwrote it.

    Attributes:
        ts_ms: Integer-millisecond timestamp of the incorrect edit.
        session_id: The Claude session UUID the edit fired in.
        source: The writing system, e.g. ``cc-pushback``.
        anchor_uuid: The transcript uuid of the feedback the harvest anchored on.
        incorrect_digest: The cross-language content digest of the incorrect
            edit's tool call — the join key shared with ``decisions_v1``.
        incorrect_file: The file the incorrect edit targeted.
        incorrect_old: The content the incorrect edit replaced (hunks joined).
        incorrect_new: The content the incorrect edit wrote (hunks joined).
        extractor_version: The deterministic-extraction version that derived the
            pair; part of the UNIQUE key.
        correction_origin: ``'session'`` or ``'git'``, or None when no
            correction was found.
        correction_file: The file the correction touched, when one exists.
        correction_old: The content the correction replaced, when one exists.
        correction_new: The content the correction wrote, when one exists.
        correction_commit: The full commit hash, when the correction is a git fix.
        overlap: The hunk-overlap score linking incorrect and correction; 0.0
            when there is no correction.
        detail: Structured extras, serialized to ``detail_json``.
    """

    ts_ms: int
    session_id: SessionId
    source: str
    anchor_uuid: EventUuid
    incorrect_digest: ToolDigest
    incorrect_file: str
    incorrect_old: str
    incorrect_new: str
    extractor_version: int
    correction_origin: Origin | None = None
    correction_file: str | None = None
    correction_old: str | None = None
    correction_new: str | None = None
    correction_commit: str | None = None
    overlap: float = 0.0
    detail: Mapping[str, Any] = field(default_factory=dict)


class CorrectionLog:
    """The ``corrections_v1`` ledger at ``~/.cc-transcript/corrections.db``.

    Opened in WAL mode with a busy timeout because writers across the family
    (and cc-review's Go port) touch the same file concurrently. Durable by
    convention: rows are never auto-dropped. Requires a local disk — WAL does
    not work over NFS.

    Example:
        >>> log = CorrectionLog.open()
        >>> log.append(correction)
        >>> log.by_digest(session_id, incorrect_digest=digest)
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn

    @classmethod
    def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the ledger at ``path``.

        Args:
            path: The database file path; its parents are created if absent.
                Defaults to ``~/.cc-transcript/corrections.db``.

        Returns:
            The opened log.
        """
        path = path or Path.home() / ".cc-transcript" / "corrections.db"
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(path, autocommit=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA busy_timeout = 2000")
        conn.executescript(CORRECTIONS_DDL)
        return cls(conn)

    def append(self, correction: Correction) -> None:
        """Appends ``correction`` as a single ``INSERT OR IGNORE``.

        Idempotent on the UNIQUE key ``(session_id, anchor_uuid,
        incorrect_digest, extractor_version)`` — re-harvesting the same edit,
        across reruns or across the pairs of one event, writes one row.
        """
        self.conn.execute(
            f"INSERT OR IGNORE INTO corrections_v1 ({CORRECTION_COLUMNS}) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                correction.ts_ms,
                correction.session_id,
                correction.source,
                correction.anchor_uuid,
                correction.incorrect_digest,
                correction.incorrect_file,
                correction.incorrect_old,
                correction.incorrect_new,
                correction.correction_origin,
                correction.correction_file,
                correction.correction_old,
                correction.correction_new,
                correction.correction_commit,
                correction.overlap,
                correction.extractor_version,
                json.dumps(dict(correction.detail)),
            ),
        )

    def for_session(self, session_id: SessionId) -> tuple[Correction, ...]:
        """All corrections for ``session_id``, ordered by timestamp."""
        return tuple(
            correction_of(row)
            for row in self.conn.execute(
                "SELECT * FROM corrections_v1 WHERE session_id = ? ORDER BY ts_ms, id", (session_id,)
            )
        )

    def for_anchor(self, session_id: SessionId, anchor_uuid: EventUuid) -> tuple[Correction, ...]:
        """The corrections harvested around one feedback ``anchor_uuid``."""
        return tuple(
            correction_of(row)
            for row in self.conn.execute(
                "SELECT * FROM corrections_v1 WHERE session_id = ? AND anchor_uuid = ? ORDER BY ts_ms, id",
                (session_id, anchor_uuid),
            )
        )

    def by_digest(self, session_id: SessionId, *, incorrect_digest: ToolDigest) -> tuple[Correction, ...]:
        """Corrections of the tool call with ``incorrect_digest`` in ``session_id``.

        The cross-consumer join: pass the ``tool_digest`` a hook recorded in
        ``decisions_v1`` to learn whether that exact edit was later corrected.
        """
        return tuple(
            correction_of(row)
            for row in self.conn.execute(
                "SELECT * FROM corrections_v1 WHERE session_id = ? AND incorrect_digest = ? ORDER BY ts_ms, id",
                (session_id, incorrect_digest),
            )
        )


def correction_of(row: sqlite3.Row) -> Correction:
    return Correction(
        ts_ms=row["ts_ms"],
        session_id=row["session_id"],
        source=row["source"],
        anchor_uuid=row["anchor_uuid"],
        incorrect_digest=row["incorrect_digest"],
        incorrect_file=row["incorrect_file"],
        incorrect_old=row["incorrect_old"],
        incorrect_new=row["incorrect_new"],
        extractor_version=row["extractor_version"],
        correction_origin=row["correction_origin"],
        correction_file=row["correction_file"],
        correction_old=row["correction_old"],
        correction_new=row["correction_new"],
        correction_commit=row["correction_commit"],
        overlap=row["overlap"],
        detail=json.loads(row["detail_json"]),
    )
