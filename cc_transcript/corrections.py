"""The shared code-correction ledger every consumer reads and writes.

One SQLite table (``corrections``) records each incorrect edit a developer
pushed back on paired with the correction that overwrote it — a later edit, a
git commit, or a reviewer's natural-language note. cc-steer and captain-hook
write it from their harvest passes; cc-review writes human review corrections
through the ``cc-transcript corrections`` CLI; all join it by ``incorrect_digest``,
the same cross-language tool digest the decision ledger keys on.

One write codepath, in Rust: :class:`CorrectionLog` is a facade over the native
engine, which bundles its own SQLite. Two SQLite libraries in one process cannot
coordinate POSIX advisory locks, so within one process exactly one engine may
touch a given ledger file — this module never opens the ledger through
:mod:`sqlite3`, and neither should any caller. Cross-process mixing (cc-review's
Go reader, the Rust CLI, other Python processes) is safe.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Literal, Self

from cc_transcript import _native
from cc_transcript.literals import literal_str

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

    from cc_transcript.ids import EventUuid, SessionId, ToolDigest

CORRECTIONS_DDL = literal_str("corrections.DDL")

Origin = Literal["session", "git", "review"]


@dataclass(frozen=True, slots=True)
class Correction:
    """One incorrect edit and the correction that overwrote it.

    Attributes:
        ts_ms: Integer-millisecond timestamp of the incorrect edit.
        session_id: The Claude session UUID the edit fired in.
        source: The writing system, e.g. ``cc-steer``, ``captain-hook``, ``cc-review``.
        anchor_uuid: The transcript uuid of the feedback the harvest anchored on,
            or ``review:<reviewID>:<commentID>`` for a human review correction.
        incorrect_digest: The cross-language content digest of the incorrect
            edit's tool call — the join key shared with the ``decisions`` ledger.
            None for human review corrections, which join by ``anchor_uuid``.
        incorrect_file: The file the incorrect edit targeted.
        incorrect_old: The content the incorrect edit replaced (hunks joined).
        incorrect_new: The content the incorrect edit wrote (hunks joined).
        correction_origin: ``'session'``/``'git'`` for a mined code fix,
            ``'review'`` for a human note, or None when no correction was found.
        correction_file: The file the correction touched, when one exists.
        correction_old: The content the correction replaced, when one exists.
        correction_new: The content the correction wrote, when one exists.
        correction_commit: The full commit hash, when the correction is a git fix.
        correction_text: The reviewer's verbatim natural-language correction, for
            ``'review'`` rows; None for mined code corrections.
        overlap: The hunk-overlap score linking incorrect and correction; 0.0
            when there is no correction.
        detail: Structured extras (e.g. ``repo``), serialized to ``detail_json``.
    """

    ts_ms: int
    session_id: SessionId
    source: str
    anchor_uuid: EventUuid
    incorrect_digest: ToolDigest | None
    incorrect_file: str
    incorrect_old: str
    incorrect_new: str
    correction_origin: Origin | None = None
    correction_file: str | None = None
    correction_old: str | None = None
    correction_new: str | None = None
    correction_commit: str | None = None
    correction_text: str | None = None
    overlap: float = 0.0
    detail: Mapping[str, Any] = field(default_factory=dict)


class CorrectionLog:
    """The ``corrections`` ledger at ``~/.cc-transcript/corrections.db``.

    A facade over the native engine, opened in WAL mode with a busy timeout
    because writers across the family touch the same file concurrently.
    Durable by convention: rows are never auto-dropped. Requires a local
    disk — WAL does not work over NFS. The engine bundles its own SQLite, so
    within this process no other SQLite library may open the same file (see
    the module docstring).

    Example:
        >>> log = CorrectionLog.open()
        >>> log.append(correction)
        >>> log.by_digest(session_id, incorrect_digest=digest)
    """

    def __init__(self, engine: _native.RustCorrectionLog) -> None:
        self._engine = engine

    @classmethod
    def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the ledger at ``path``.

        Args:
            path: The database file path; its parents are created if absent.
                Defaults to the ledger's file under ``~/.cc-transcript``.

        Returns:
            The opened log.
        """
        path = path or Path.home() / ".cc-transcript" / "corrections.db"
        path.parent.mkdir(parents=True, exist_ok=True)
        return cls(_native.RustCorrectionLog(str(path)))

    def append(self, record: Correction) -> None:
        """Appends ``record`` as a single ``INSERT OR IGNORE``.

        Idempotent on the table's UNIQUE key, so re-running a writer writes one
        row; SQLite treats NULL key columns as distinct, so rows whose key
        carries a NULL rely on the writer not repeating the same values.
        """
        self._engine.append(
            record.ts_ms,
            record.session_id,
            record.source,
            record.anchor_uuid,
            record.incorrect_digest,
            record.incorrect_file,
            record.incorrect_old,
            record.incorrect_new,
            record.correction_origin,
            record.correction_file,
            record.correction_old,
            record.correction_new,
            record.correction_commit,
            record.correction_text,
            record.overlap,
            record.detail,
        )

    def for_session(self, session_id: SessionId) -> tuple[Correction, ...]:
        """All records for ``session_id``, ordered by timestamp."""
        return tuple(Correction(**row) for row in self._engine.for_session(session_id))

    def for_repo(self, repo: str) -> tuple[Correction, ...]:
        """All corrections whose ``detail.repo`` is ``repo``, ordered by timestamp.

        The repo key producers stamp into ``detail`` so a per-repo consumer (the
        captain-hook reviewer) can pull every correction for its repo at once.
        """
        return tuple(Correction(**row) for row in self._engine.for_repo(repo))

    def since(self, ts_ms: int, *, source: str | None = None) -> tuple[Correction, ...]:
        """Corrections with ``ts_ms`` strictly greater than ``ts_ms``, oldest first.

        A cursor read for incremental consumers; pass ``source`` to scope to one
        producer.
        """
        return tuple(Correction(**row) for row in self._engine.since(ts_ms, source))

    def for_anchor(self, session_id: SessionId, anchor_uuid: EventUuid) -> tuple[Correction, ...]:
        """The corrections harvested around one feedback ``anchor_uuid``."""
        return tuple(Correction(**row) for row in self._engine.for_anchor(session_id, anchor_uuid))

    def by_digest(self, session_id: SessionId, *, incorrect_digest: ToolDigest) -> tuple[Correction, ...]:
        """Corrections of the tool call with ``incorrect_digest`` in ``session_id``.

        The cross-consumer join: pass the ``tool_digest`` a hook recorded in
        the ``decisions`` ledger to learn whether that exact edit was later
        corrected.
        """
        return tuple(Correction(**row) for row in self._engine.by_digest(session_id, incorrect_digest))

    def sql(self, statement: str) -> list[dict[str, Any]]:
        """Runs one raw SQL ``statement`` — the escape hatch behind ``corrections sql``."""
        return self._engine.sql(statement)
