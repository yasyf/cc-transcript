"""What actually hit disk, as recorded by cc-review's turn ledger.

cc-review brackets every Claude turn with working-tree snapshots and
attributes diff line ranges to the turns that wrote them. The
``cc-review export activity`` CLI surfaces that ledger as versioned
``cc-review.activity/1`` JSON; this module is its Python reader. Timestamps
are integer milliseconds, per the cross-process convention.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript.context import SchemaError
from cc_transcript.ids import SessionId

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

SCHEMA = "cc-review.activity/1"


@dataclass(frozen=True, slots=True)
class TreeTurn:
    """One Claude prompt-to-stop window bracketed by working-tree snapshots.

    Attributes:
        turn_id: cc-review's identifier for the turn.
        repo_root: The repository the tree snapshots were taken in.
        started_at_ms: Integer-millisecond timestamp the turn opened.
        ended_at_ms: Integer-millisecond timestamp the turn closed; 0 while
            the turn is still open.
        tree_start: The working-tree hash at turn open.
        tree_end: The working-tree hash at turn close; empty while open.
        status: cc-review's turn state: ``open``, ``closed``, or
            ``interrupted``.
    """

    turn_id: int
    repo_root: str
    started_at_ms: int
    ended_at_ms: int
    tree_start: str
    tree_end: str
    status: str


@dataclass(frozen=True, slots=True)
class AttributionRange:
    """An inclusive span of new-side diff lines attributed to one turn.

    Attributes:
        start: The first attributed line, 1-based.
        end: The last attributed line, inclusive.
        turn_id: The turn that wrote the span; None when unattributed.
    """

    start: int
    end: int
    turn_id: int | None


@dataclass(frozen=True, slots=True)
class FileAttribution:
    """One file's attribution ranges within one review version.

    Attributions are keyed by ``(version, file_path)`` — the version
    dimension is preserved because flat per-session ranges were ambiguous
    across review versions.

    Attributes:
        review_id: The review the version belongs to.
        version: The review version number the ranges were computed against.
        file_path: The file the ranges cover, relative to the repo root.
        ranges: The attributed spans, in file order.
    """

    review_id: str
    version: int
    file_path: str
    ranges: tuple[AttributionRange, ...]


@dataclass(frozen=True, slots=True)
class DiskTruth:
    """A session's disk-level activity exported from cc-review.

    Attributes:
        session_id: The Claude session UUID the export covers.
        turns: The session's tree-bracketed turns, in ledger order.
        attributions: Version-dimensioned per-file attribution ranges.

    Example:
        >>> truth = export_activity(session_id)
        >>> changed = [turn for turn in truth.turns if turn.tree_end != turn.tree_start] if truth else []
    """

    session_id: SessionId
    turns: tuple[TreeTurn, ...]
    attributions: tuple[FileAttribution, ...]


def load_export(data: bytes) -> DiskTruth:
    """Parse a ``cc-review export activity`` payload.

    Strict by contract: every field must be present, and timestamps are
    integer milliseconds.

    Raises:
        SchemaError: When ``data`` does not carry the literal
            ``cc-review.activity/1`` schema.
    """
    payload = json.loads(data)
    if not isinstance(payload, dict) or payload.get("schema") != SCHEMA:
        raise SchemaError(f"expected schema {SCHEMA!r}, got: {data[:120]!r}")
    return DiskTruth(
        session_id=SessionId(payload["session_id"]),
        turns=tuple(tree_turn_from(item) for item in payload["turns"]),
        attributions=tuple(attribution_from(item) for item in payload["attributions"]),
    )


def export_activity(session_id: SessionId, *, binary: str = "cc-review") -> DiskTruth | None:
    """Export ``session_id``'s disk truth by shelling out to cc-review.

    Runs ``<binary> export activity --session <uuid>`` and parses its stdout
    via :func:`load_export`.

    Returns:
        The parsed export, or None when the binary is absent or exits
        nonzero — consumers degrade to transcript-only activity.
    """
    try:
        proc = subprocess.run([binary, "export", "activity", "--session", session_id], capture_output=True)
    except OSError:
        return None
    return load_export(proc.stdout) if proc.returncode == 0 else None


def tree_turn_from(payload: Mapping[str, Any]) -> TreeTurn:
    return TreeTurn(
        turn_id=payload["turn_id"],
        repo_root=payload["repo_root"],
        started_at_ms=payload["started_at_ms"],
        ended_at_ms=payload["ended_at_ms"],
        tree_start=payload["tree_start"],
        tree_end=payload["tree_end"],
        status=payload["status"],
    )


def attribution_from(payload: Mapping[str, Any]) -> FileAttribution:
    return FileAttribution(
        review_id=payload["review_id"],
        version=payload["version"],
        file_path=payload["file_path"],
        ranges=tuple(range_from(item) for item in payload["ranges"]),
    )


def range_from(payload: Mapping[str, Any]) -> AttributionRange:
    return AttributionRange(start=payload["start"], end=payload["end"], turn_id=payload["turn_id"])
