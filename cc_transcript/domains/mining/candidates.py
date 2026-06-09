"""The feedback candidate model and the dedup key that makes ingestion idempotent."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from typing import TYPE_CHECKING, NewType

if TYPE_CHECKING:
    from collections.abc import Mapping
    from datetime import datetime
    from pathlib import Path
    from typing import Any

    from cc_transcript.models import SessionId

    from cc_transcript.domains.mining.confidence import CandidateSignal
    from cc_transcript.domains.mining.context import ContextSnapshot
    from cc_transcript.domains.mining.sourcekind import SourceKind

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
        context: The conversational window around the feedback.
        session_id: The transcript session the feedback came from.
        origin_path: The file the candidate was extracted from.
        origin_uuid: The originating transcript entry's uuid.
        cc_version: The Claude Code version recorded for the origin.
        payload: Detector-specific metadata preserved verbatim.
        signal: The de-noising confidence signal, when computed.
    """

    dedup_key: DedupKey
    source_kind: SourceKind
    occurred_at: datetime
    text: str
    context: ContextSnapshot
    session_id: SessionId | None = None
    origin_path: Path | None = None
    origin_uuid: str | None = None
    cc_version: str | None = None
    payload: Mapping[str, Any] | None = None
    signal: CandidateSignal | None = None


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
