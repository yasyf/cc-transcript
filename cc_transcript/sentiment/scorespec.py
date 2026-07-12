"""Declarative, composable sentiment-score stages — the score-side analogue of
:class:`~cc_transcript.FilterSpec`.

A :class:`ScoreSpec` is an ordered tuple of :class:`ScoreStage`. The phase a stage
acts in is intrinsic to its type: :class:`FrustrationShortCircuit` pre-empts
inference; the rest post-process the model score. Consumers compose a spec from the
builders (:func:`flag_frustration`, :func:`clamp_positive`,
:func:`demote_mild_irritation`, :func:`clamp_resume`); the library ships no preset.

The spec serializes to JSON (:func:`score_spec_to_json`) for the Rust executor, the
sole runtime for every deterministic stage; only model inference stays Python-side.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import orjson

from cc_transcript.filterspec import (
    FRUSTRATION_GROUPS,
    MILD_IMPATIENCE_GROUPS,
    RESUME_PHRASE_SET,
    SHORT_MESSAGE_MAX_WORDS,
    TRAILING_PUNCT,
)

if TYPE_CHECKING:
    from typing import Any


@dataclass(frozen=True, slots=True)
class FrustrationShortCircuit:
    """Pre-empts inference with ``score`` when a user message matches ``groups``."""

    groups: tuple[tuple[str, str], ...]
    score: int = 1
    ignore_case: bool = True


@dataclass(frozen=True, slots=True)
class PositiveClamp:
    """Lowers ``from_score`` to ``to_score`` when a short user message lacks positive lexicon."""

    from_score: int = 5
    to_score: int = 3
    max_words: int = SHORT_MESSAGE_MAX_WORDS


@dataclass(frozen=True, slots=True)
class MildIrritationDemote:
    """Lowers ``from_score`` to ``to_score`` for mild-impatience messages that are not hostile.

    Hostile means a ``hostile_groups`` regex hit OR a negative lexicon hit at the
    fixed polarity floor (``Lexicon.FLOOR``).
    """

    trigger_groups: tuple[tuple[str, str], ...]
    hostile_groups: tuple[tuple[str, str], ...]
    from_score: int = 1
    to_score: int = 2
    ignore_case: bool = True


@dataclass(frozen=True, slots=True)
class ResumeClamp:
    """Clamps to ``to_score`` when a user message is a bare resume phrase."""

    phrases: frozenset[str]
    to_score: int = 3
    strip_trailing: str = TRAILING_PUNCT


ScoreStage = FrustrationShortCircuit | PositiveClamp | MildIrritationDemote | ResumeClamp


@dataclass(frozen=True, slots=True)
class ScoreSpec:
    """An ordered list of :class:`ScoreStage` applied around model inference."""

    stages: tuple[ScoreStage, ...]


def flag_frustration(*, score: int = 1) -> FrustrationShortCircuit:
    """Composes the short-circuit stage that pins a frustrated message to ``score`` before inference."""
    return FrustrationShortCircuit(groups=FRUSTRATION_GROUPS, score=score)


def clamp_positive(*, max_words: int = SHORT_MESSAGE_MAX_WORDS) -> PositiveClamp:
    """Composes the post-process stage that lowers a top score on a short message lacking positive lexicon."""
    return PositiveClamp(max_words=max_words)


def demote_mild_irritation() -> MildIrritationDemote:
    """Composes the post-process stage that softens a non-hostile mild-impatience message off the floor score."""
    return MildIrritationDemote(trigger_groups=MILD_IMPATIENCE_GROUPS, hostile_groups=FRUSTRATION_GROUPS)


def clamp_resume() -> ResumeClamp:
    """Composes the post-process stage that neutralizes a bare resume phrase to a middling score."""
    return ResumeClamp(phrases=RESUME_PHRASE_SET)


def build_score_spec(*stages: ScoreStage) -> ScoreSpec:
    """Assembles ``stages`` into a :class:`ScoreSpec` for the engine to apply around inference."""
    return ScoreSpec(stages=tuple(stages))


def stage_to_dict(stage: ScoreStage) -> dict[str, Any]:
    match stage:
        case FrustrationShortCircuit(groups=groups, score=score, ignore_case=ignore_case):
            return {
                "kind": "FrustrationShortCircuit",
                "groups": [list(group) for group in groups],
                "score": score,
                "ignore_case": ignore_case,
            }
        case PositiveClamp(from_score=fr, to_score=to, max_words=mw):
            return {"kind": "PositiveClamp", "from_score": fr, "to_score": to, "max_words": mw}
        case MildIrritationDemote(trigger_groups=tg, hostile_groups=hg, from_score=fr, to_score=to, ignore_case=ic):
            return {
                "kind": "MildIrritationDemote",
                "trigger_groups": [list(group) for group in tg],
                "hostile_groups": [list(group) for group in hg],
                "from_score": fr,
                "to_score": to,
                "ignore_case": ic,
            }
        case ResumeClamp(phrases=phrases, to_score=to, strip_trailing=strip):
            return {"kind": "ResumeClamp", "phrases": sorted(phrases), "to_score": to, "strip_trailing": strip}


def score_spec_to_json(spec: ScoreSpec) -> str:
    """Serializes ``spec`` to the JSON contract consumed by the Rust score executor."""
    return orjson.dumps({"stages": [stage_to_dict(stage) for stage in spec.stages]}).decode()
