"""Neutral fact-detectors over a transcript's ordered events.

Each detector recognizes one transcript shape and yields a :class:`MiningSignal`
describing it. A signal is a neutral fact: it carries a candidate ``trigger_index``
but never disqualifies on its absence, never applies a ``FilterSpec``, and never
builds an app candidate. The app maps signals to its own records with policy
injected, capturing each candidate's window via
:func:`~cc_transcript.context.capture_window` over a lifted
:class:`~cc_transcript.activity.SessionActivity`.

The mining policy is a :class:`~cc_transcript.mining.spec.MiningSpec`: which
detectors run, the confidence-scoring stages each folds, the provenance set, and the
review-format policy. :func:`mine` interprets the spec and dispatches the enabled
detectors; the same spec serializes for the Rust backend. Every signal carries a
calibrated :class:`CandidateSignal` spanning the full confidence band, with named
reason codes (``trigger_proximate``, ``short_followup``, ``substantive``, ``hedged``,
``embedded_text``, ``bare_marker``, ``structural_only``) so apps filter on confidence
and reasons instead of re-deriving them.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, NamedTuple

from cc_transcript.facts import is_denial
from cc_transcript.filterspec import (
    ANSWERED_PREFIX,
    ANSWERED_TRAILER,
    INTERRUPT_MARKER_RE,
    compile_groups,
    embedded_user_text,
    event_kind,
    interrupt_marker,
    is_bare_interrupt_marker,
    tool_names,
    tool_uses,
)
from cc_transcript.mining.confidence import CandidateSignal, noise, weak
from cc_transcript.mining.formats import extract_structured
from cc_transcript.mining.sourcekind import (
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    QUESTION_ANSWER,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
)
from cc_transcript.mining.spec import (
    ASK_USER_QUESTION_DETECTOR,
    DENIAL_DETECTOR,
    EXIT_PLAN_REJECTION_DETECTOR,
    INTERRUPT_DETECTOR,
    PLAN_REENTRY_DETECTOR,
    REVIEW_COMMENT_DETECTOR,
    TRANSCRIPT_MESSAGE_DETECTOR,
    MiningSpec,
    NoiseIfStructural,
    Provenance,
    calibrated,
    classify_provenance,
    regex_review_comments,
    score_user_message,
)
from cc_transcript.models import AssistantEvent, ModeEvent, ToolResultBlock, ToolUseBlock, UserEvent
from cc_transcript.tools import matches_names

if TYPE_CHECKING:
    import re
    from collections.abc import Iterator, Mapping, Sequence
    from datetime import datetime
    from typing import Any

    from cc_transcript.mining.formats import ReviewComment
    from cc_transcript.mining.sourcekind import SourceKind
    from cc_transcript.mining.spec import ReviewSpec
    from cc_transcript.models import CcVersion, EventUuid, SessionId, ToolUseId, TranscriptEvent

# An answered AskUserQuestion round renders each pair as '"Q"="A"' (or
# '"Q"=(no option selected)'), optionally followed by ' selected preview:\n<raw>'
# and/or ' notes: <text>', with pairs joined by ', ' inside the ANSWERED banner.
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


class ScoredText(NamedTuple):
    text: str
    signal: CandidateSignal


def denial_results(event: UserEvent) -> Iterator[ToolResultBlock]:
    return (block for block in event.blocks if isinstance(block, ToolResultBlock) if is_denial(block))


def last_edit_index(events: Sequence[TranscriptEvent], index: int, spec: MiningSpec) -> int | None:
    return next(
        (
            i
            for i in range(index - 1, max(index - spec.reentry_lookback, 0) - 1, -1)
            if isinstance(event := events[i], AssistantEvent)
            if any(isinstance(b, ToolUseBlock) and b.name in spec.edit_tools for b in event.blocks)
        ),
        None,
    )


def next_user_message(events: Sequence[TranscriptEvent], index: int) -> tuple[int, UserEvent] | None:
    return next(
        (
            (i, event)
            for i in range(index, len(events))
            if isinstance(event := events[i], UserEvent)
            if event.text.strip()
        ),
        None,
    )


def denied_tool_payload(use: ToolUseBlock) -> dict[str, Any]:
    return {"tool": use.name, "file_path": use.input.get("file_path")}


def marker_in(event: UserEvent) -> str | None:
    return next(
        (
            marker
            for block in event.blocks
            if isinstance(block, ToolResultBlock)
            if (marker := interrupt_marker(block.content)) is not None
        ),
        None,
    )


def nearest_assistant_index(events: Sequence[TranscriptEvent], index: int) -> int | None:
    return next((i for i in range(index - 1, -1, -1) if event_kind(events[i]) == "assistant"), None)


def structural_re(spec: MiningSpec) -> re.Pattern[str]:
    return next(
        (
            compile_groups(stage.groups, stage.ignore_case)
            for stage in spec.user_message.stages
            if isinstance(stage, NoiseIfStructural)
        ),
        INTERRUPT_MARKER_RE,
    )


def correction_text(events: Sequence[TranscriptEvent], index: int, structural: re.Pattern[str]) -> str | None:
    while (found := next_user_message(events, index + 1)) is not None:
        i, event = found
        if not is_bare_interrupt_marker(event.text) and not structural.search(event.text):
            return event.text
        index = i
    return None


def first_followup(events: Sequence[TranscriptEvent], index: int) -> str | None:
    while (found := next_user_message(events, index + 1)) is not None:
        index, event = found
        if not is_bare_interrupt_marker(event.text):
            return event.text
    return None


def marker_correction(events: Sequence[TranscriptEvent], index: int, structural: re.Pattern[str]) -> ScoredText | None:
    if (correction := correction_text(events, index, structural)) is not None:
        return ScoredText(correction, weak("bare_marker"))
    if (followup := first_followup(events, index)) is not None:
        return ScoredText(followup, noise("structural_only"))
    return None


def denial_correction(
    events: Sequence[TranscriptEvent], index: int, embedded: str | None, spec: MiningSpec, structural: re.Pattern[str]
) -> ScoredText | None:
    if embedded:
        return ScoredText(embedded, calibrated(spec.calibrated, embedded, seed="embedded_text"))
    return marker_correction(events, index, structural)


def iter_user_message_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
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
            signal=score_user_message(spec.user_message, event.text, index, trigger),
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        if event.text.strip()
        if not is_bare_interrupt_marker(event.text)
    )


def iter_plan_rejection_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
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
            signal=calibrated(spec.calibrated, text, seed="embedded_text"),
        )
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for result in denial_results(event)
        if (use := uses.get(result.tool_use_id)) is not None
        if matches_names(use.name, spec.plan_tools)
        if (text := embedded_user_text(result.content)) is not None
    )


def iter_plan_reentry_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    seen: set[EventUuid] = set()
    for index, event in enumerate(events):
        if not (isinstance(event, ModeEvent) and event.value == "plan"):
            continue
        if (user := next_user_message(events, index)) is None:
            continue
        user_index, user_event = user
        if user_event.meta.uuid in seen or is_bare_interrupt_marker(user_event.text):
            continue
        if (edit := last_edit_index(events, user_index, spec)) is None:
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
            signal=calibrated(spec.calibrated, user_event.text, seed="reentry_after_edit"),
        )


def iter_tool_denial_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    uses = tool_uses(events)
    structural = structural_re(spec)
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
        if (paired := uses.get(block.tool_use_id)) is None or not matches_names(paired.name, spec.denial_excluded_tools)
        if (scored := denial_correction(events, index, embedded_user_text(block.content), spec, structural)) is not None
    )


def iter_interrupt_marker_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    structural = structural_re(spec)
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
        if (scored := marker_correction(events, index, structural)) is not None
    )


class ScanText(NamedTuple):
    text: str
    provenance: Provenance
    trigger_index: int | None


def review_scan_texts(
    events: Sequence[TranscriptEvent],
    event: UserEvent,
    index: int,
    spec: MiningSpec,
    *,
    names: Mapping[ToolUseId, str],
) -> Iterator[ScanText]:
    surfaces = spec.review.surfaces
    if "typed" in surfaces and event.text.strip():
        yield ScanText(event.text, "typed", nearest_assistant_index(events, index))
    yield from (
        ScanText(block.content, provenance, None)
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
        if (
            provenance := classify_provenance(
                spec.provenance, names.get(block.tool_use_id), is_sidechain=event.meta.is_sidechain
            )
        )
        != "typed"
        if provenance in surfaces
    )


def review_comment_signal(
    event: UserEvent, index: int, scan: ScanText, fmt_name: str, comment: ReviewComment, spec: MiningSpec
) -> MiningSignal:
    return MiningSignal(
        kind=REVIEW_COMMENT,
        detector="review_comment",
        session_id=event.meta.session_id,
        event_index=index,
        event_uuid=event.meta.uuid,
        occurred_at=event.meta.timestamp,
        text=comment.comment,
        cc_version=event.meta.cc_version,
        trigger_index=scan.trigger_index,
        evidence={
            "format": fmt_name,
            "file": comment.file,
            "line_start": comment.line_start,
            "line_end": comment.line_end,
            "provenance": scan.provenance,
        },
        signal=calibrated(spec.calibrated, comment.comment, seed="format_match"),
    )


def review_comments(review: ReviewSpec, text: str) -> Iterator[tuple[str, ReviewComment]]:
    yield from ((fmt.name, comment) for fmt in review.regex_formats for comment in regex_review_comments(fmt, text))
    yield from (
        (fmt.name, comment)
        for fmt in review.callable_formats
        if fmt.pattern.search(text)
        for comment in fmt.extract(text)
    )
    yield from ((fmt.name, comment) for fmt, comment in extract_structured(text, review.structured_formats))


def iter_review_comment_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    names = tool_names(events)
    return (
        review_comment_signal(event, index, scan, fmt_name, comment, spec)
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for scan in review_scan_texts(events, event, index, spec, names=names)
        for fmt_name, comment in review_comments(spec.review, scan.text)
    )


class AnsweredPair(NamedTuple):
    question: Mapping[str, Any]
    answer: str | None
    preview: str | None
    notes: str | None


def answered_question_results(event: UserEvent) -> Iterator[ToolResultBlock]:
    return (
        block
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
        if not block.is_error
        if block.content.startswith(ANSWERED_PREFIX)
    )


def split_answer_segment(segment: str) -> tuple[str, str | None, str | None]:
    if (at := segment.find(ANSWER_PREVIEW_SEP)) != -1:
        rest = segment[at + len(ANSWER_PREVIEW_SEP) :]
        if (notes_at := rest.find(ANSWER_NOTES_SEP)) != -1:
            return segment[:at], rest[:notes_at], rest[notes_at + len(ANSWER_NOTES_SEP) :]
        return segment[:at], rest, None
    if (at := segment.find(ANSWER_NOTES_SEP)) != -1:
        return segment[:at], None, segment[at + len(ANSWER_NOTES_SEP) :]
    return segment, None, None


def find_anchor(body: str, anchor: str, pos: int) -> int:
    while (at := body.find(anchor, pos)) != -1:
        if at == 0 or body[:at].endswith(", "):
            return at
        pos = at + 1
    return -1


def answered_pairs(body: str, questions: Sequence[Mapping[str, Any]]) -> Iterator[AnsweredPair]:
    found: list[tuple[Mapping[str, Any], int, int]] = []
    pos = 0
    for question in questions:
        if not isinstance(text := question.get("question"), str):
            continue
        anchor = f'"{text}"='
        if (at := find_anchor(body, anchor, pos)) == -1:
            continue
        found.append((question, at, at + len(anchor)))
        pos = at + len(anchor)
    for i, (question, _, value_at) in enumerate(found):
        last = i + 1 == len(found)
        segment = body[value_at : len(body) if last else found[i + 1][1]]
        if not last:
            segment = segment.removesuffix(", ")
        head, preview, notes = split_answer_segment(segment)
        if head.startswith('"'):
            yield AnsweredPair(question, head[1:].removesuffix('"'), preview, notes)
        elif head.startswith(NO_OPTION_SELECTED):
            yield AnsweredPair(question, None, preview, notes)


def join_labels(answer: str, labels: Sequence[str]) -> list[str] | None:
    picked: list[str] = []
    start = 0
    acc: str | None = None
    for part in answer.split(", "):
        acc = part if acc is None else f"{acc}, {part}"
        if (at := next((i for i in range(start, len(labels)) if labels[i] == acc), None)) is not None:
            picked.append(labels[at])
            start = at + 1
            acc = None
    return picked if picked and acc is None else None


def ordinal_label(answer: str, labels: Sequence[str]) -> tuple[str, bool] | None:
    rest = answer.lstrip("0123456789")
    digits = answer[: len(answer) - len(rest)]
    if not digits or not 1 <= (n := int(digits)) <= len(labels) or (rest and rest[0] not in ",. )"):
        return None
    return labels[n - 1], not rest


def resolve_pick(answer: str | None, labels: Sequence[str]) -> tuple[list[str], bool]:
    if answer is None:
        return [], False
    if (joined := join_labels(answer, labels)) is not None:
        return joined, True
    if (ordinal := ordinal_label(answer, labels)) is not None:
        label, bare = ordinal
        return [label], bare
    return [], False


def question_answer_signal(
    events: Sequence[TranscriptEvent], event: UserEvent, index: int, pair: AnsweredPair, spec: MiningSpec
) -> MiningSignal | None:
    options = pair.question.get("options")
    labels = [
        label
        for option in (options if isinstance(options, list) else ())
        if isinstance(option, dict) and isinstance(label := option.get("label"), str)
    ]
    picked, option_pick = resolve_pick(pair.answer, labels)
    text = pair.notes if pair.notes is not None else pair.answer
    if text is None:
        return None
    evidence: dict[str, Any] = {
        "question": question if isinstance(question := pair.question.get("question"), str) else None,
        "header": header if isinstance(header := pair.question.get("header"), str) else None,
        "multi_select": isinstance(multi := pair.question.get("multiSelect"), bool) and multi,
        "option_pick": option_pick,
        "picked_labels": picked,
        "recommended_pick": any("(Recommended)" in label for label in picked),
    }
    if pair.preview is not None:
        evidence["preview"] = pair.preview
    if pair.notes is not None:
        evidence["notes"] = pair.notes
    return MiningSignal(
        kind=QUESTION_ANSWER,
        detector="ask_user_question",
        session_id=event.meta.session_id,
        event_index=index,
        event_uuid=event.meta.uuid,
        occurred_at=event.meta.timestamp,
        text=text,
        cc_version=event.meta.cc_version,
        trigger_index=nearest_assistant_index(events, index),
        signal=weak("option_pick")
        if option_pick and not pair.notes
        else calibrated(spec.calibrated, text, seed="freeform_answer"),
        evidence=evidence,
    )


def iter_ask_user_question_signals(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    uses = tool_uses(events)
    return (
        signal
        for index, event in enumerate(events)
        if isinstance(event, UserEvent)
        for block in answered_question_results(event)
        if (use := uses.get(block.tool_use_id)) is not None
        if use.name == "AskUserQuestion"
        if block.content.endswith(ANSWERED_TRAILER)
        for pair in answered_pairs(block.content[len(ANSWERED_PREFIX) : -len(ANSWERED_TRAILER)], use.input["questions"])
        if (signal := question_answer_signal(events, event, index, pair, spec)) is not None
    )


def mine(events: Sequence[TranscriptEvent], spec: MiningSpec) -> Iterator[MiningSignal]:
    """Yields every :class:`MiningSignal` the enabled detectors recognize in ``events``.

    Args:
        events: The ordered transcript events to mine.
        spec: The mining policy: which detectors run, with which scoring, provenance,
            and review-format policy.

    Yields:
        Neutral mined facts, one per recognized transcript shape, in detector order.
    """
    if TRANSCRIPT_MESSAGE_DETECTOR in spec.detectors:
        yield from iter_user_message_signals(events, spec)
    if EXIT_PLAN_REJECTION_DETECTOR in spec.detectors:
        yield from iter_plan_rejection_signals(events, spec)
    if PLAN_REENTRY_DETECTOR in spec.detectors:
        yield from iter_plan_reentry_signals(events, spec)
    if DENIAL_DETECTOR in spec.detectors:
        yield from iter_tool_denial_signals(events, spec)
    if INTERRUPT_DETECTOR in spec.detectors:
        yield from iter_interrupt_marker_signals(events, spec)
    if REVIEW_COMMENT_DETECTOR in spec.detectors:
        yield from iter_review_comment_signals(events, spec)
    if ASK_USER_QUESTION_DETECTOR in spec.detectors:
        yield from iter_ask_user_question_signals(events, spec)
