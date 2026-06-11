"""Neutral fact-detectors over a transcript's ordered events.

Each iterator recognizes one transcript shape and yields a :class:`MiningSignal`
describing it. A signal is a neutral fact: it carries a candidate ``trigger_index``
but never disqualifies on its absence, never applies a ``FilterSpec``, and never
builds an app candidate. The app maps signals to its own records with policy injected.

Every signal carries a calibrated :class:`CandidateSignal` spanning the full
confidence band: arithmetic bumps and demotions over the anchors, with named
reason codes (``trigger_proximate``, ``short_followup``, ``substantive``,
``hedged``, ``embedded_text``, ``bare_marker``, ``structural_only``) so apps can
filter on :func:`~cc_transcript.domains.mining.confidence.effective_confidence`
and reasons instead of re-deriving them.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, NamedTuple

from cc_transcript import STRUCTURAL_NOISE_RE
from cc_transcript.models import AssistantEvent, ModeEvent, UserEvent

from cc_transcript.domains.mining.confidence import CandidateSignal, Confidence, firm, noise, weak
from cc_transcript.domains.mining.formats import extract_all
from cc_transcript.domains.mining.nav import (
    denial_results,
    denied_tool_payload,
    embedded_user_text,
    is_bare_interrupt_marker,
    last_edit_index,
    marker_in,
    next_user_message,
    tool_uses,
)
from cc_transcript.domains.mining.sourcekind import (
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
)

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator, Mapping, Sequence
    from datetime import datetime
    from typing import Any

    from cc_transcript.models import CcVersion, EntryUuid, SessionId, TranscriptEvent

    from cc_transcript.domains.mining.formats import ReviewFormat
    from cc_transcript.domains.mining.sourcekind import SourceKind

CONFIDENCE_STEP = 0.25
SHORT_FOLLOWUP_MAX_WORDS = 2
TIGHT_PROXIMITY = 2
HEDGE_RE = re.compile(
    r"\b(?:maybe|perhaps|possibly|might|not sure|i think|i guess|if you (?:want|prefer)|up to you)\b",
    re.IGNORECASE,
)


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
        lower_bound: A context anchor, such as the plan-reentry edit index.
        evidence: Detector-specific metadata preserved verbatim.
        signal: The de-noising confidence signal; apps re-derive as needed.
    """

    kind: SourceKind
    detector: str
    session_id: SessionId
    event_index: int
    event_uuid: EntryUuid
    occurred_at: datetime
    text: str
    cc_version: CcVersion | None
    trigger_index: int | None
    lower_bound: int | None = None
    evidence: Mapping[str, Any] = field(default_factory=dict)
    signal: CandidateSignal | None = None


class ScoredText(NamedTuple):
    text: str
    signal: CandidateSignal


def nearest_assistant_index(events: Sequence[TranscriptEvent], index: int) -> int | None:
    return next((i for i in range(index - 1, -1, -1) if isinstance(events[i], AssistantEvent)), None)


def correction_text(events: Sequence[TranscriptEvent], index: int) -> str | None:
    while (found := next_user_message(events, index + 1)) is not None:
        i, event = found
        if not is_bare_interrupt_marker(event.text) and not STRUCTURAL_NOISE_RE.search(event.text):
            return event.text
        index = i
    return None


def first_followup(events: Sequence[TranscriptEvent], index: int) -> str | None:
    while (found := next_user_message(events, index + 1)) is not None:
        index, event = found
        if not is_bare_interrupt_marker(event.text):
            return event.text
    return None


def adjust(signal: CandidateSignal, delta: float, reason: str) -> CandidateSignal:
    return CandidateSignal(
        Confidence(min(1.0, max(0.0, signal.confidence + delta))), (*signal.reasons, reason), signal.durable
    )


def is_substantive(text: str) -> bool:
    return len(text.split()) > SHORT_FOLLOWUP_MAX_WORDS and not STRUCTURAL_NOISE_RE.search(text)


def is_proximate(index: int, trigger: int | None) -> bool:
    return trigger is not None and index - trigger <= TIGHT_PROXIMITY


def calibrated(text: str, *reasons: str) -> CandidateSignal:
    base = firm(*reasons)
    promoted = adjust(base, CONFIDENCE_STEP, "substantive") if is_substantive(text) else base
    return adjust(promoted, -CONFIDENCE_STEP, "hedged") if HEDGE_RE.search(text) else promoted


def score_user_message(text: str, index: int, trigger: int | None) -> CandidateSignal:
    if STRUCTURAL_NOISE_RE.search(text):
        return noise("structural_only")
    base = firm("user_message")
    short = len(text.split()) <= SHORT_FOLLOWUP_MAX_WORDS
    demoted = adjust(base, -CONFIDENCE_STEP, "short_followup") if short else base
    return adjust(demoted, CONFIDENCE_STEP, "trigger_proximate") if is_proximate(index, trigger) else demoted


def marker_correction(events: Sequence[TranscriptEvent], index: int) -> ScoredText | None:
    if (correction := correction_text(events, index)) is not None:
        return ScoredText(correction, weak("bare_marker"))
    if (followup := first_followup(events, index)) is not None:
        return ScoredText(followup, noise("structural_only"))
    return None


def denial_correction(events: Sequence[TranscriptEvent], index: int, embedded: str | None) -> ScoredText | None:
    if embedded:
        return ScoredText(embedded, calibrated(embedded, "embedded_text"))
    return marker_correction(events, index)


def iter_user_message_signals(events: Sequence[TranscriptEvent]) -> Iterator[MiningSignal]:
    return (
        MiningSignal(
            kind=TRANSCRIPT_MESSAGE,
            detector="transcript_message",
            session_id=event.meta.session_id,
            event_index=index,
            event_uuid=event.meta.uuid,
            occurred_at=event.meta.timestamp,
            text=event.text,
            cc_version=event.meta.cc_version,
            trigger_index=(trigger := nearest_assistant_index(events, index)),
            signal=score_user_message(event.text, index, trigger),
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        if event.text.strip()
        if not is_bare_interrupt_marker(event.text)
    )


def iter_plan_rejection_signals(events: Sequence[TranscriptEvent]) -> Iterator[MiningSignal]:
    uses = tool_uses(events)
    return (
        MiningSignal(
            kind=PLAN_REVIEW,
            detector="exit_plan_rejection",
            session_id=event.meta.session_id,
            event_index=index,
            event_uuid=event.meta.uuid,
            occurred_at=event.meta.timestamp,
            text=text,
            cc_version=event.meta.cc_version,
            trigger_index=nearest_assistant_index(events, index),
            signal=calibrated(text, "embedded_text"),
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for result in denial_results(event)
        if (use := uses.get(result.tool_use_id)) is not None
        if use.name == "ExitPlanMode"
        if (text := embedded_user_text(result.content)) is not None
    )


def iter_plan_reentry_signals(events: Sequence[TranscriptEvent]) -> Iterator[MiningSignal]:
    seen: set[EntryUuid] = set()
    for index, event in enumerate(events):
        if not (isinstance(event, ModeEvent) and event.value == "plan"):
            continue
        if (user := next_user_message(events, index)) is None:
            continue
        user_index, user_event = user
        if user_event.meta.uuid in seen or is_bare_interrupt_marker(user_event.text):
            continue
        if (edit := last_edit_index(events, user_index)) is None:
            continue
        seen.add(user_event.meta.uuid)
        yield MiningSignal(
            kind=PLAN_REVIEW,
            detector="plan_reentry",
            session_id=user_event.meta.session_id,
            event_index=user_index,
            event_uuid=user_event.meta.uuid,
            occurred_at=user_event.meta.timestamp,
            text=user_event.text,
            cc_version=user_event.meta.cc_version,
            trigger_index=nearest_assistant_index(events, user_index),
            lower_bound=edit,
            signal=calibrated(user_event.text, "reentry_after_edit"),
        )


def iter_tool_denial_signals(events: Sequence[TranscriptEvent]) -> Iterator[MiningSignal]:
    uses = tool_uses(events)
    return (
        MiningSignal(
            kind=INTERRUPT_REJECTION,
            detector="denial",
            session_id=event.meta.session_id,
            event_index=index,
            event_uuid=event.meta.uuid,
            occurred_at=event.meta.timestamp,
            text=scored.text,
            cc_version=event.meta.cc_version,
            trigger_index=nearest_assistant_index(events, index),
            evidence=denied_tool_payload(paired) if paired else {},
            signal=scored.signal,
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for block in denial_results(event)
        if (paired := uses.get(block.tool_use_id)) is None or paired.name not in {"ExitPlanMode", "AskUserQuestion"}
        if (scored := denial_correction(events, index, embedded_user_text(block.content))) is not None
    )


def iter_interrupt_marker_signals(events: Sequence[TranscriptEvent]) -> Iterator[MiningSignal]:
    return (
        MiningSignal(
            kind=INTERRUPT_REJECTION,
            detector="interrupt",
            session_id=event.meta.session_id,
            event_index=index,
            event_uuid=event.meta.uuid,
            occurred_at=event.meta.timestamp,
            text=scored.text,
            cc_version=event.meta.cc_version,
            trigger_index=nearest_assistant_index(events, index),
            signal=scored.signal,
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        if marker_in(event) is not None
        if (scored := marker_correction(events, index)) is not None
    )


def iter_review_comment_signals(
    events: Sequence[TranscriptEvent], formats: Sequence[ReviewFormat]
) -> Iterator[MiningSignal]:
    return (
        MiningSignal(
            kind=REVIEW_COMMENT,
            detector="review_comment",
            session_id=event.meta.session_id,
            event_index=index,
            event_uuid=event.meta.uuid,
            occurred_at=event.meta.timestamp,
            text=comment.comment,
            cc_version=event.meta.cc_version,
            trigger_index=nearest_assistant_index(events, index),
            evidence={
                "format": fmt.name,
                "file": comment.file,
                "line_start": comment.line_start,
                "line_end": comment.line_end,
            },
            signal=calibrated(comment.comment, "format_match"),
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for fmt, comment in extract_all(event.text, formats)
    )


DEFAULT_DETECTORS: tuple[Callable[[Sequence[TranscriptEvent]], Iterator[MiningSignal]], ...] = (
    iter_user_message_signals,
    iter_plan_rejection_signals,
    iter_plan_reentry_signals,
    iter_tool_denial_signals,
    iter_interrupt_marker_signals,
)
