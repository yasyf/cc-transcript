"""Declarative, both-backend mining policy — the mining analogue of
:class:`~cc_transcript.FilterSpec` and :class:`~cc_transcript.sentiment.ScoreSpec`.

A :class:`MiningSpec` packages the full mining policy as data: which detectors run,
the confidence-scoring stages each detector folds over a candidate, the provenance
classification set, and the review-format policy. Its JSON contract
(:func:`mining_spec_to_json`) is executed solely by the Rust backend.

Confidence scoring is an ordered tuple of :class:`ConfStage` folded over a
:class:`ScoreCtx`. :class:`NoiseIfStructural` short-circuits to noise like
:class:`~cc_transcript.sentiment.scorespec.FrustrationShortCircuit`. The stage order
in :data:`CALIBRATED_SPEC` and :data:`USER_MESSAGE_SPEC` reproduces the historical
``firm → +substantive → −hedged`` and ``structural → firm − short + proximate``
sequences verbatim, so mined confidences and reason tuples stay byte-identical.

Review formats come in two shapes: a :class:`RegexReviewFormat` (one named regex
plus a declarative group map) executes directly in Rust, while a
:class:`CallableReviewFormat` (arbitrary pattern plus a Python extractor) is invoked
through a pyo3 callback side-channel — single-threaded, under the held GIL.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from functools import reduce
from typing import TYPE_CHECKING, Literal, NewType

import orjson

from cc_transcript.filterspec import (
    HEDGE_GROUPS,
    STRUCTURAL_NOISE_GROUPS,
    compile_groups,
)
from cc_transcript.mining.confidence import MEDIUM, NONE, CandidateSignal, Confidence, firm
from cc_transcript.mining.formats import FINDING_KEYS, ReviewComment, StructuredFormat
from cc_transcript.tools import expand_tool_names, matches_names

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import Any

    from cc_transcript.mining.signals import MiningSignal

DetectorName = NewType("DetectorName", str)
"""One of the seven core detector identifiers a :class:`MiningSpec` may enable."""

TRANSCRIPT_MESSAGE_DETECTOR = DetectorName("transcript_message")
EXIT_PLAN_REJECTION_DETECTOR = DetectorName("exit_plan_rejection")
PLAN_REENTRY_DETECTOR = DetectorName("plan_reentry")
DENIAL_DETECTOR = DetectorName("denial")
INTERRUPT_DETECTOR = DetectorName("interrupt")
REVIEW_COMMENT_DETECTOR = DetectorName("review_comment")
ASK_USER_QUESTION_DETECTOR = DetectorName("ask_user_question")

ALL_DETECTORS: frozenset[DetectorName] = frozenset(
    {
        TRANSCRIPT_MESSAGE_DETECTOR,
        EXIT_PLAN_REJECTION_DETECTOR,
        PLAN_REENTRY_DETECTOR,
        DENIAL_DETECTOR,
        INTERRUPT_DETECTOR,
        REVIEW_COMMENT_DETECTOR,
        ASK_USER_QUESTION_DETECTOR,
    }
)

EDIT_TOOLS: frozenset[str] = expand_tool_names("Edit|Write|MultiEdit|NotebookEdit")
SUBAGENT_TOOLS: frozenset[str] = expand_tool_names("Agent|Task")
PLAN_TOOLS: frozenset[str] = expand_tool_names("ExitPlanMode")
DENIAL_EXCLUDED_TOOLS: frozenset[str] = expand_tool_names("ExitPlanMode|AskUserQuestion")
REENTRY_LOOKBACK = 40
CONFIDENCE_STEP = 0.25
SHORT_FOLLOWUP_MAX_WORDS = 2
TIGHT_PROXIMITY = 2

Provenance = Literal["typed", "surfaced", "claude"]
DEFAULT_SURFACES: frozenset[Provenance] = frozenset({"typed", "surfaced"})


@dataclass(frozen=True, slots=True)
class ScoreCtx:
    text: str
    index: int
    trigger: int | None


@dataclass(frozen=True, slots=True)
class Base:
    """Seeds the fold with a fixed confidence band and reason code."""

    band: Confidence
    reason: str


@dataclass(frozen=True, slots=True)
class BumpIfSubstantive:
    """Raises confidence by ``delta`` when the text exceeds ``min_words`` and is not structural noise."""

    groups: tuple[tuple[str, str], ...]
    delta: float = CONFIDENCE_STEP
    min_words: int = SHORT_FOLLOWUP_MAX_WORDS
    ignore_case: bool = True
    reason: str = "substantive"


@dataclass(frozen=True, slots=True)
class DemoteIfHedged:
    """Lowers confidence by ``delta`` when the text matches a hedging ``groups`` regex."""

    groups: tuple[tuple[str, str], ...]
    delta: float = -CONFIDENCE_STEP
    ignore_case: bool = True
    reason: str = "hedged"


@dataclass(frozen=True, slots=True)
class DemoteIfShort:
    """Lowers confidence by ``delta`` when the text has at most ``max_words`` words."""

    max_words: int = SHORT_FOLLOWUP_MAX_WORDS
    delta: float = -CONFIDENCE_STEP
    reason: str = "short_followup"


@dataclass(frozen=True, slots=True)
class BumpIfProximate:
    """Raises confidence by ``delta`` when the candidate's index is within ``within`` of its trigger."""

    within: int = TIGHT_PROXIMITY
    delta: float = CONFIDENCE_STEP
    reason: str = "trigger_proximate"


@dataclass(frozen=True, slots=True)
class NoiseIfStructural:
    """Short-circuits the fold to ``band`` with a single ``reason`` when the text is structural noise."""

    groups: tuple[tuple[str, str], ...]
    band: Confidence = NONE
    ignore_case: bool = True
    reason: str = "structural_only"


ConfStage = Base | BumpIfSubstantive | DemoteIfHedged | DemoteIfShort | BumpIfProximate | NoiseIfStructural


@dataclass(frozen=True, slots=True)
class ConfidenceSpec:
    """An ordered tuple of :class:`ConfStage` folded over a :class:`ScoreCtx`."""

    stages: tuple[ConfStage, ...]


CALIBRATED_SPEC = ConfidenceSpec(
    (
        BumpIfSubstantive(groups=STRUCTURAL_NOISE_GROUPS),
        DemoteIfHedged(groups=HEDGE_GROUPS),
    )
)
"""Calibration stages applied after a per-call :class:`Base` seed: ``+substantive`` then ``−hedged``."""

USER_MESSAGE_SPEC = ConfidenceSpec(
    (
        NoiseIfStructural(groups=STRUCTURAL_NOISE_GROUPS),
        Base(MEDIUM, "user_message"),
        DemoteIfShort(),
        BumpIfProximate(),
    )
)
"""User-message scoring stages: structural → noise, else firm − short + proximate."""


@dataclass(frozen=True, slots=True)
class ProvenanceSpec:
    """Names the subagent tools whose tool results classify as ``claude`` provenance."""

    subagent_tools: frozenset[str] = SUBAGENT_TOOLS


@dataclass(frozen=True, slots=True)
class RegexReviewFormat:
    """A review format Rust can run: one named regex plus a declarative group map.

    The comment is built from ``comment_groups`` in order: each matched group is
    stripped first, groups that are unmatched or empty after stripping are skipped,
    and the remaining parts are joined with ``join``. Line groups are stripped then
    parsed as integers; an unmatched or unparseable value yields ``None``. Both
    backends implement exactly these semantics.

    Attributes:
        name: The format's identifier.
        groups: A single ``(name, pattern)`` pair; the named regex tried against texts.
        file_group: The capture-group index for the cited file, or None.
        line_start_group: The capture-group index for the first cited line, or None.
        line_end_group: The capture-group index for the last cited line, or None.
        comment_groups: Ordered capture-group indices joined into the comment text.
        join: The separator between joined comment groups.
        multiline: Whether the pattern compiles with ``re.MULTILINE``.
        ignore_case: Whether the pattern compiles case-insensitively.
    """

    name: str
    groups: tuple[tuple[str, str], ...]
    file_group: int | None
    line_start_group: int | None
    line_end_group: int | None
    comment_groups: tuple[int, ...]
    join: str = " "
    multiline: bool = True
    ignore_case: bool = False


@dataclass(frozen=True, slots=True)
class CallableReviewFormat:
    """An escape hatch: an arbitrary pattern plus a Python extractor.

    The Rust mining executor invokes ``pattern`` and ``extract`` through a pyo3
    callback side-channel — single-threaded, under the held GIL — passing the format
    by position. Both are ordinary Python objects the Rust JSON contract never carries.

    Attributes:
        name: The format's identifier.
        pattern: A pattern that matches when the format is present in a text.
        extract: Parses a matching text into its review comments.
    """

    name: str
    pattern: re.Pattern[str]
    extract: Callable[[str], tuple[ReviewComment, ...]]


ReviewFormat = RegexReviewFormat | CallableReviewFormat


@dataclass(frozen=True, slots=True)
class ReviewSpec:
    """The review-comment detector's policy: which formats and surfaces to scan."""

    regex_formats: tuple[RegexReviewFormat, ...] = ()
    callable_formats: tuple[CallableReviewFormat, ...] = ()
    structured_formats: tuple[StructuredFormat, ...] = ()
    surfaces: frozenset[Provenance] = DEFAULT_SURFACES


@dataclass(frozen=True, slots=True)
class MiningSpec:
    """The full declarative mining policy interpreted by both backends.

    Attributes:
        detectors: The detector ids to run; absent detectors are skipped.
        user_message: Scoring stages for the transcript-message detector.
        calibrated: Calibration stages seeded per call by the denial, plan, and
            review detectors.
        provenance: The provenance classification policy.
        review: The review-comment detector's format and surface policy.
        reentry_lookback: How many events back the plan-reentry detector scans for an edit.
        edit_tools: The tool names whose use anchors a plan-reentry edit.
        plan_tools: The plan-submission tool names whose denials mine as plan rejections.
        denial_excluded_tools: The tool names whose denials the denial detector skips.
    """

    detectors: frozenset[DetectorName] = ALL_DETECTORS
    user_message: ConfidenceSpec = USER_MESSAGE_SPEC
    calibrated: ConfidenceSpec = CALIBRATED_SPEC
    provenance: ProvenanceSpec = field(default_factory=ProvenanceSpec)
    review: ReviewSpec = field(default_factory=ReviewSpec)
    reentry_lookback: int = REENTRY_LOOKBACK
    edit_tools: frozenset[str] = EDIT_TOOLS
    plan_tools: frozenset[str] = PLAN_TOOLS
    denial_excluded_tools: frozenset[str] = DENIAL_EXCLUDED_TOOLS


def run_confidence(spec: ConfidenceSpec, ctx: ScoreCtx, base: CandidateSignal) -> CandidateSignal:
    """Folds ``spec``'s stages over ``ctx``, seeded by ``base``.

    A :class:`NoiseIfStructural` hit short-circuits to its band with only its reason,
    like the score spec's :class:`~cc_transcript.sentiment.scorespec.FrustrationShortCircuit`.
    """
    for stage in spec.stages:
        if isinstance(stage, NoiseIfStructural) and compile_groups(stage.groups, stage.ignore_case).search(ctx.text):
            return CandidateSignal(stage.band, (stage.reason,), base.durable)
    return reduce(lambda signal, stage: apply_conf_stage(stage, ctx, signal), spec.stages, base)


def apply_conf_stage(stage: ConfStage, ctx: ScoreCtx, signal: CandidateSignal) -> CandidateSignal:
    match stage:
        case Base(band=band, reason=reason):
            return CandidateSignal(band, (*signal.reasons, reason), signal.durable)
        case BumpIfSubstantive(groups=groups, delta=delta, min_words=mw, ignore_case=ic, reason=reason) if (
            len(ctx.text.split()) > mw and not compile_groups(groups, ic).search(ctx.text)
        ):
            return bump(signal, delta, reason)
        case DemoteIfHedged(groups=groups, delta=delta, ignore_case=ic, reason=reason) if compile_groups(
            groups, ic
        ).search(ctx.text):
            return bump(signal, delta, reason)
        case DemoteIfShort(max_words=mw, delta=delta, reason=reason) if len(ctx.text.split()) <= mw:
            return bump(signal, delta, reason)
        case BumpIfProximate(within=within, delta=delta, reason=reason) if (
            ctx.trigger is not None and ctx.index - ctx.trigger <= within
        ):
            return bump(signal, delta, reason)
        case _:
            return signal


def bump(signal: CandidateSignal, delta: float, reason: str) -> CandidateSignal:
    return CandidateSignal(
        Confidence(min(1.0, max(0.0, signal.confidence + delta))), (*signal.reasons, reason), signal.durable
    )


def calibrated(spec: ConfidenceSpec, text: str, *, seed: str) -> CandidateSignal:
    """Scores ``text`` by folding ``spec`` over a :class:`Base` seeded with ``seed``."""
    return run_confidence(spec, ScoreCtx(text, 0, None), firm(seed))


def score_user_message(spec: ConfidenceSpec, text: str, index: int, trigger: int | None) -> CandidateSignal:
    """Scores a transcript user message by folding ``spec`` over its context."""
    return run_confidence(spec, ScoreCtx(text, index, trigger), CandidateSignal(NONE, (), True))


def classify_provenance(spec: ProvenanceSpec, tool_name: str | None, *, is_sidechain: bool) -> Provenance:
    match (tool_name, is_sidechain):
        case (None, _):
            return "typed"
        case (name, False) if not matches_names(name, spec.subagent_tools):
            return "surfaced"
        case _:
            return "claude"


def regex_review_comments(fmt: RegexReviewFormat, text: str) -> tuple[ReviewComment, ...]:
    """Extracts comments from ``text`` per ``fmt``'s declarative group map."""
    pattern = compile_groups(fmt.groups, fmt.ignore_case, multiline=fmt.multiline)
    return tuple(
        ReviewComment(
            file=group_value(match, fmt.file_group),
            line_start=int_group(match, fmt.line_start_group),
            line_end=int_group(match, fmt.line_end_group),
            comment=fmt.join.join(
                part for index in fmt.comment_groups if (part := (match.group(index) or "").strip())
            ),
        )
        for match in pattern.finditer(text)
    )


def group_value(match: re.Match[str], index: int | None) -> str | None:
    return None if index is None else match.group(index)


def int_group(match: re.Match[str], index: int | None) -> int | None:
    if index is None or (value := match.group(index)) is None:
        return None
    try:
        return int(value.strip())
    except ValueError:
        return None


def conf_spec_to_dict(spec: ConfidenceSpec) -> dict[str, Any]:
    return {"stages": [conf_stage_to_dict(stage) for stage in spec.stages]}


def conf_stage_to_dict(stage: ConfStage) -> dict[str, Any]:
    match stage:
        case Base(band=band, reason=reason):
            return {"kind": "Base", "band": band, "reason": reason}
        case BumpIfSubstantive(groups=groups, delta=delta, min_words=mw, ignore_case=ic, reason=reason):
            return {
                "kind": "BumpIfSubstantive",
                "groups": [list(group) for group in groups],
                "delta": delta,
                "min_words": mw,
                "ignore_case": ic,
                "reason": reason,
            }
        case DemoteIfHedged(groups=groups, delta=delta, ignore_case=ic, reason=reason):
            return {
                "kind": "DemoteIfHedged",
                "groups": [list(group) for group in groups],
                "delta": delta,
                "ignore_case": ic,
                "reason": reason,
            }
        case DemoteIfShort(max_words=mw, delta=delta, reason=reason):
            return {"kind": "DemoteIfShort", "max_words": mw, "delta": delta, "reason": reason}
        case BumpIfProximate(within=within, delta=delta, reason=reason):
            return {"kind": "BumpIfProximate", "within": within, "delta": delta, "reason": reason}
        case NoiseIfStructural(groups=groups, band=band, ignore_case=ic, reason=reason):
            return {
                "kind": "NoiseIfStructural",
                "groups": [list(group) for group in groups],
                "band": band,
                "ignore_case": ic,
                "reason": reason,
            }


def provenance_to_dict(spec: ProvenanceSpec) -> dict[str, Any]:
    return {"subagent_tools": sorted(spec.subagent_tools)}


def structured_format_to_dict(fmt: StructuredFormat) -> dict[str, Any]:
    return {
        "kind": "StructuredFormat",
        "name": fmt.name,
        "file_keys": list(fmt.file_keys),
        "line_keys": list(fmt.line_keys),
        "comment_keys": list(fmt.comment_keys),
        "fix_keys": list(fmt.fix_keys),
        "finding_keys": list(FINDING_KEYS + fmt.finding_keys),
    }


def regex_format_to_dict(fmt: RegexReviewFormat) -> dict[str, Any]:
    return {
        "kind": "RegexReviewFormat",
        "name": fmt.name,
        "groups": [list(group) for group in fmt.groups],
        "file_group": fmt.file_group,
        "line_start_group": fmt.line_start_group,
        "line_end_group": fmt.line_end_group,
        "comment_groups": list(fmt.comment_groups),
        "join": fmt.join,
        "multiline": fmt.multiline,
        "ignore_case": fmt.ignore_case,
    }


def review_spec_to_dict(spec: ReviewSpec) -> dict[str, Any]:
    """Serializes the Rust-executed review formats. Callable formats travel
    out-of-band via the pyo3 side-channel, so they carry no JSON representation."""
    return {
        "surfaces": sorted(spec.surfaces),
        "regex_formats": [regex_format_to_dict(fmt) for fmt in spec.regex_formats],
        "structured_formats": [structured_format_to_dict(fmt) for fmt in spec.structured_formats],
    }


def mining_spec_to_json(spec: MiningSpec) -> str:
    """Serializes ``spec`` to the JSON contract consumed by the Rust mining executor."""
    return orjson.dumps(
        {
            "detectors": sorted(spec.detectors),
            "reentry_lookback": spec.reentry_lookback,
            "edit_tools": sorted(spec.edit_tools),
            "plan_tools": sorted(spec.plan_tools),
            "denial_excluded_tools": sorted(spec.denial_excluded_tools),
            "provenance": provenance_to_dict(spec.provenance),
            "user_message": conf_spec_to_dict(spec.user_message),
            "calibrated": conf_spec_to_dict(spec.calibrated),
            "review": review_spec_to_dict(spec.review),
        }
    ).decode()


def signal_to_dict(signal: MiningSignal) -> dict[str, Any]:
    """Returns the canonical comparison dict for ``signal``.

    The Rust mining executor returns this exact shape; the parity test compares
    ``[signal_to_dict(s) for s in py_mine(...)]`` against the Rust output by value.
    """
    return {
        "kind": str(signal.kind),
        "detector": signal.detector,
        "session_id": str(signal.session_id),
        "event_index": signal.event_index,
        "event_uuid": str(signal.event_uuid),
        "occurred_at": signal.occurred_at.isoformat(),
        "text": signal.text,
        "cc_version": None if signal.cc_version is None else str(signal.cc_version),
        "trigger_index": signal.trigger_index,
        "signal": {
            "confidence": signal.signal.confidence,
            "reasons": list(signal.signal.reasons),
            "durable": signal.signal.durable,
        },
        "lower_bound": signal.lower_bound,
        "evidence": dict(signal.evidence),
    }
