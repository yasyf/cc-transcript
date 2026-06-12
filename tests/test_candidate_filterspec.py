from __future__ import annotations

from datetime import UTC, datetime

import pytest

from cc_transcript.context import ContextWindow
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.mining import (
    MEDIUM,
    NOISE_FLOOR,
    PLAN_REVIEW,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
    CandidateSignal,
    Confidence,
    FeedbackCandidate,
    SourceKind,
    dedup_key,
    firm,
    noise,
    strong,
    weak,
)
from cc_transcript.mining.filterspec import (
    CandidateClause,
    CandidateFilterSpec,
    ConfidenceAtLeast,
    HasReason,
    IsDurable,
    SourceKindIn,
    apply_candidate_filter,
    at_least,
    build_candidate_filter,
    keep_candidate,
    only_kinds,
)

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
WINDOW = ContextWindow(
    anchor=EventRef(SessionId("sess"), EventUuid("u1")),
    before=(),
    trigger=None,
    after=(),
    fidelity="summary",
    preview_chars=200,
)


def candidate(
    text: str = "feedback",
    *,
    kind: SourceKind = TRANSCRIPT_MESSAGE,
    signal: CandidateSignal = firm(),
) -> FeedbackCandidate:
    return FeedbackCandidate(
        dedup_key=dedup_key("sess", kind, text),
        source_kind=kind,
        occurred_at=BASE,
        text=text,
        window=WINDOW,
        ref=WINDOW.anchor,
        signal=signal,
    )


@pytest.mark.parametrize(
    ("signal", "floor", "kept"),
    [
        pytest.param(strong("trigger_proximate"), MEDIUM, True, id="strong_clears_medium_floor"),
        pytest.param(weak("bare_marker"), MEDIUM, False, id="weak_fails_medium_floor"),
        pytest.param(weak("bare_marker"), NOISE_FLOOR, True, id="weak_sits_exactly_on_noise_floor"),
        pytest.param(noise("structural_only"), NOISE_FLOOR, False, id="noise_falls_below_noise_floor"),
    ],
)
def test_confidence_at_least(signal: CandidateSignal, floor: Confidence, kept: bool) -> None:
    spec = build_candidate_filter(CandidateClause(ConfidenceAtLeast(floor)))
    assert keep_candidate(candidate(signal=signal), spec) is kept


def test_source_kind_in() -> None:
    spec = build_candidate_filter(CandidateClause(SourceKindIn(frozenset({REVIEW_COMMENT, PLAN_REVIEW}))))
    assert keep_candidate(candidate(kind=REVIEW_COMMENT), spec)
    assert keep_candidate(candidate(kind=PLAN_REVIEW), spec)
    assert not keep_candidate(candidate(kind=TRANSCRIPT_MESSAGE), spec)


@pytest.mark.parametrize(
    ("signal", "reason", "kept"),
    [
        pytest.param(strong("embedded_text", "substantive"), "substantive", True, id="reason_present"),
        pytest.param(strong("embedded_text"), "hedged", False, id="reason_absent"),
    ],
)
def test_has_reason(signal: CandidateSignal, reason: str, kept: bool) -> None:
    spec = build_candidate_filter(CandidateClause(HasReason(reason)))
    assert keep_candidate(candidate(signal=signal), spec) is kept


@pytest.mark.parametrize(
    ("signal", "want", "kept"),
    [
        pytest.param(weak("bare_marker", durable=False), False, True, id="ephemeral_matches_want_false"),
        pytest.param(weak("bare_marker", durable=False), True, False, id="ephemeral_fails_want_true"),
        pytest.param(strong("substantive"), True, True, id="durable_matches_want_true"),
    ],
)
def test_is_durable(signal: CandidateSignal, want: bool, kept: bool) -> None:
    spec = build_candidate_filter(CandidateClause(IsDurable(want)))
    assert keep_candidate(candidate(signal=signal), spec) is kept


def test_negate_inverts_a_clause() -> None:
    spec = build_candidate_filter(CandidateClause(HasReason("hedged"), negate=True))
    assert keep_candidate(candidate(signal=strong("substantive")), spec)
    assert not keep_candidate(candidate(signal=firm("substantive", "hedged")), spec)


def test_empty_spec_keeps_everything() -> None:
    assert keep_candidate(candidate(signal=noise("structural_only")), build_candidate_filter())


def test_clauses_combine_conjunctively() -> None:
    spec = build_candidate_filter(at_least(MEDIUM), only_kinds(REVIEW_COMMENT))
    assert keep_candidate(candidate(kind=REVIEW_COMMENT, signal=strong("format_match")), spec)
    assert not keep_candidate(candidate(kind=REVIEW_COMMENT, signal=weak("bare_marker")), spec)
    assert not keep_candidate(candidate(kind=TRANSCRIPT_MESSAGE, signal=strong("user_message")), spec)


def test_builders_construct_expected_clauses() -> None:
    assert at_least(MEDIUM) == CandidateClause(ConfidenceAtLeast(MEDIUM))
    assert only_kinds(REVIEW_COMMENT, PLAN_REVIEW) == CandidateClause(
        SourceKindIn(frozenset({REVIEW_COMMENT, PLAN_REVIEW}))
    )
    clause = at_least(NOISE_FLOOR)
    assert build_candidate_filter(clause) == CandidateFilterSpec(clauses=(clause,))


def test_apply_candidate_filter_filters_stream() -> None:
    keeper = candidate("split this function", kind=REVIEW_COMMENT, signal=strong("format_match", "substantive"))
    wrong_kind = candidate("plain message", kind=TRANSCRIPT_MESSAGE, signal=strong("user_message"))
    too_noisy = candidate("<system-reminder>x</system-reminder>", kind=REVIEW_COMMENT, signal=noise("structural_only"))
    spec = build_candidate_filter(at_least(NOISE_FLOOR), only_kinds(REVIEW_COMMENT))
    assert list(apply_candidate_filter([keeper, wrong_kind, too_noisy], spec)) == [keeper]
