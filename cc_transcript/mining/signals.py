"""The mined-fact data type.

A :class:`MiningSignal` is a neutral fact mined from a transcript: it names the shape
a detector recognized, carries a candidate trigger, a confidence signal, and
detector-specific evidence, but no policy. The Rust executor
(:func:`~cc_transcript.mining.engine.mine_signals`) produces these; apps map each
signal to their own candidate record with policy injected, capturing its window via
:func:`~cc_transcript.context.capture_window` over a lifted
:class:`~cc_transcript.activity.SessionActivity`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Mapping
    from datetime import datetime
    from typing import Any

    from cc_transcript.mining.confidence import CandidateSignal
    from cc_transcript.mining.sourcekind import SourceKind
    from cc_transcript.models import CcVersion, EventUuid, SessionId

# Separators inside the ANSWERED banner's rendered '"Q"="A"' pairs.
ANSWER_PREVIEW_SEP = " selected preview:\n"
ANSWER_NOTES_SEP = " notes: "
NO_OPTION_SELECTED = "(no option selected)"


@dataclass(frozen=True, slots=True)
class MiningSignal:
    """A neutral fact mined from a transcript, ready for an app to map to a candidate.

    Attributes:
        kind: The descriptive source category.
        detector: The sub-discriminator naming the shape that produced the signal.
        session_id: The session the originating event belongs to.
        event_index: The index of the originating event in the stream.
        event_uuid: The originating event's uuid.
        occurred_at: When the originating event was recorded.
        text: The mined feedback text.
        cc_version: The Claude Code version recorded for the origin.
        trigger_index: The nearest preceding assistant index, or None — a hint the
            app may use; absence never disqualifies the fact here.
        signal: The de-noising confidence signal; apps re-derive as needed.
        lower_bound: A context anchor, such as the plan-reentry edit index.
        evidence: Detector-specific metadata preserved verbatim.
    """

    kind: SourceKind
    detector: str
    session_id: SessionId
    event_index: int
    event_uuid: EventUuid
    occurred_at: datetime
    text: str
    cc_version: CcVersion | None
    trigger_index: int | None
    signal: CandidateSignal
    lower_bound: int | None = None
    evidence: Mapping[str, Any] = field(default_factory=dict)
