"""The feedback candidate model and the dedup key that makes ingestion idempotent."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from typing import TYPE_CHECKING, NewType

if TYPE_CHECKING:
    from collections.abc import Mapping
    from datetime import datetime
    from typing import Any

    from cc_transcript.context import ContextWindow
    from cc_transcript.ids import EventRef, SessionId
    from cc_transcript.mining.confidence import CandidateSignal
    from cc_transcript.mining.sourcekind import SourceKind

DedupKey = NewType("DedupKey", str)
"""A content-derived SHA-256 key that makes feedback ingestion idempotent."""


@dataclass(frozen=True, slots=True)
class FeedbackCandidate:
    """A single piece of developer pushback extracted from a transcript.

    Attributes:
        dedup_key: The content-derived key that makes ingestion idempotent.
        source_kind: Which detector produced the candidate.
        occurred_at: When the feedback was given.
        text: The verbatim pushback text.
        window: The durable context window around the feedback, captured via
            :func:`~cc_transcript.context.capture_window`.
        ref: The resolvable reference to the originating event; None for
            migrated legacy rows.
        session_id: The transcript session the feedback came from.
        cc_version: The Claude Code version recorded for the origin.
        signal: The de-noising confidence signal, when computed.
        payload: Detector-specific metadata preserved verbatim.
    """

    dedup_key: DedupKey
    source_kind: SourceKind
    occurred_at: datetime
    text: str
    window: ContextWindow
    ref: EventRef | None = None
    session_id: SessionId | None = None
    cc_version: str | None = None
    signal: CandidateSignal | None = None
    payload: Mapping[str, Any] | None = None


def dedup_key(*parts: str) -> DedupKey:
    """Returns the stable dedup key for ``parts``.

    Detectors key on session, kind, and the feedback content (plus its code
    location for review comments) rather than the transcript entry's uuid or the
    absolute file path, so the same pushback recorded under two transcript entries
    collapses to one row, and the database stays portable and idempotent across moves.

    Args:
        parts: The content fragments that uniquely identify a candidate.

    Returns:
        The SHA-256 hex digest of the parts joined by a null byte.
    """
    return DedupKey(hashlib.sha256("\x00".join(parts).encode()).hexdigest())
