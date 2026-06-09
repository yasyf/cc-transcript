"""Declarative, composable sentiment-score stages — the score-side analogue of
:class:`~cc_transcript.FilterSpec`.

A :class:`ScoreSpec` is an ordered tuple of :class:`ScoreStage`. The phase a stage
acts in is intrinsic to its type: :class:`FrustrationShortCircuit` pre-empts
inference; the rest post-process the model score. Consumers compose a spec from the
builders (:func:`flag_frustration`, :func:`clamp_positive`,
:func:`demote_mild_irritation`, :func:`clamp_resume`); the library ships no preset.

The pure-Python interpreter here is the fallback; the spec also serializes to JSON
(:func:`score_spec_to_json`) for the Rust executor when :func:`score_spec_is_portable`.
"""

from __future__ import annotations

from dataclasses import dataclass
from functools import reduce
from typing import TYPE_CHECKING

import orjson

from cc_transcript.domains.sentiment.buckets import SentimentScore
from cc_transcript.domains.sentiment.lexicon import Lexicon
from cc_transcript.filterspec import (
    FRUSTRATION_GROUPS,
    MILD_IMPATIENCE_GROUPS,
    PORTABLE_GROUP_NAMES,
    RESUME_PHRASE_SET,
    SHORT_MESSAGE_MAX_WORDS,
    TRAILING_PUNCT,
    compile_groups,
    normalize_bare,
)

if TYPE_CHECKING:
    from collections.abc import Sequence
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
    positive_floor: int = 3


@dataclass(frozen=True, slots=True)
class MildIrritationDemote:
    """Lowers ``from_score`` to ``to_score`` for mild-impatience messages that are not hostile.

    Hostile means a ``hostile_groups`` regex hit OR lexicon polarity ``<= -hostile_floor``.
    """

    trigger_groups: tuple[tuple[str, str], ...]
    hostile_groups: tuple[tuple[str, str], ...]
    from_score: int = 1
    to_score: int = 2
    hostile_floor: int = 3
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


def clamp_positive(*, floor: int = 3, max_words: int = SHORT_MESSAGE_MAX_WORDS) -> PositiveClamp:
    """Composes the post-process stage that lowers a top score on a short message lacking positive lexicon."""
    return PositiveClamp(positive_floor=floor, max_words=max_words)


def demote_mild_irritation(*, floor: int = 3) -> MildIrritationDemote:
    """Composes the post-process stage that softens a non-hostile mild-impatience message off the floor score."""
    return MildIrritationDemote(
        trigger_groups=MILD_IMPATIENCE_GROUPS, hostile_groups=FRUSTRATION_GROUPS, hostile_floor=floor
    )


def clamp_resume() -> ResumeClamp:
    """Composes the post-process stage that neutralizes a bare resume phrase to a middling score."""
    return ResumeClamp(phrases=RESUME_PHRASE_SET)


def build_score_spec(*stages: ScoreStage) -> ScoreSpec:
    """Assembles ``stages`` into a :class:`ScoreSpec` for the engine to apply around inference."""
    return ScoreSpec(stages=tuple(stages))


def has_lexicon_stage(spec: ScoreSpec) -> bool:
    return any(isinstance(stage, PositiveClamp | MildIrritationDemote) for stage in spec.stages)


def stage_to_dict(stage: ScoreStage) -> dict[str, Any]:
    match stage:
        case FrustrationShortCircuit(groups=groups, score=score, ignore_case=ignore_case):
            return {
                "kind": "FrustrationShortCircuit",
                "groups": [list(group) for group in groups],
                "score": score,
                "ignore_case": ignore_case,
            }
        case PositiveClamp(from_score=fr, to_score=to, max_words=mw, positive_floor=pf):
            return {"kind": "PositiveClamp", "from_score": fr, "to_score": to, "max_words": mw, "positive_floor": pf}
        case MildIrritationDemote(
            trigger_groups=tg, hostile_groups=hg, from_score=fr, to_score=to, hostile_floor=hf, ignore_case=ic
        ):
            return {
                "kind": "MildIrritationDemote",
                "trigger_groups": [list(group) for group in tg],
                "hostile_groups": [list(group) for group in hg],
                "from_score": fr,
                "to_score": to,
                "hostile_floor": hf,
                "ignore_case": ic,
            }
        case ResumeClamp(phrases=phrases, to_score=to, strip_trailing=strip):
            return {"kind": "ResumeClamp", "phrases": sorted(phrases), "to_score": to, "strip_trailing": strip}


def score_spec_to_json(spec: ScoreSpec) -> str:
    """Serializes ``spec`` to the JSON contract consumed by the Rust score executor."""
    return orjson.dumps({"stages": [stage_to_dict(stage) for stage in spec.stages]}).decode()


def stage_portable(stage: ScoreStage) -> bool:
    match stage:
        case FrustrationShortCircuit(groups=groups):
            return all(name in PORTABLE_GROUP_NAMES for name, _ in groups)
        case MildIrritationDemote(trigger_groups=tg, hostile_groups=hg):
            return all(name in PORTABLE_GROUP_NAMES for name, _ in (*tg, *hg))
        case _:
            return True


def score_spec_is_portable(spec: ScoreSpec) -> bool:
    """Whether every regex-bearing stage uses Rust-portable group names."""
    return all(stage_portable(stage) for stage in spec.stages)


def py_short_circuit(spec: ScoreSpec, buckets: Sequence[Sequence[str]]) -> list[SentimentScore | None]:
    """First short-circuit score per bucket (in stage order), else None."""
    return [first_short_circuit(spec, texts) for texts in buckets]


def first_short_circuit(spec: ScoreSpec, texts: Sequence[str]) -> SentimentScore | None:
    for stage in spec.stages:
        if isinstance(stage, FrustrationShortCircuit) and any(
            compile_groups(stage.groups, stage.ignore_case).search(text) for text in texts
        ):
            return SentimentScore(stage.score)
    return None


def py_post_process(
    spec: ScoreSpec, buckets: Sequence[Sequence[str]], raw: Sequence[SentimentScore]
) -> list[SentimentScore]:
    """Folds each bucket's score through the post-process stages in order."""
    return [
        reduce(lambda score, stage: apply_post_process(stage, texts, score), spec.stages, score)
        for texts, score in zip(buckets, raw, strict=True)
    ]


def apply_post_process(stage: ScoreStage, texts: Sequence[str], score: SentimentScore) -> SentimentScore:
    match stage:
        case PositiveClamp(from_score=fr, to_score=to, max_words=mw, positive_floor=pf) if int(score) == fr and any(
            len(text.split()) <= mw and not Lexicon.has_hit(text, floor=pf, want_negative=False) for text in texts
        ):
            return SentimentScore(to)
        case MildIrritationDemote(
            trigger_groups=tg, hostile_groups=hg, from_score=fr, to_score=to, hostile_floor=hf, ignore_case=ic
        ) if int(score) == fr and any(
            compile_groups(tg, ic).search(text) and not is_hostile(text, hg, ic, hf) for text in texts
        ):
            return SentimentScore(to)
        case ResumeClamp(phrases=phrases, to_score=to, strip_trailing=strip) if any(
            normalize_bare(text, strip) in phrases for text in texts
        ):
            return SentimentScore(to)
        case _:
            return score


def is_hostile(text: str, hostile_groups: tuple[tuple[str, str], ...], ignore_case: bool, hostile_floor: int) -> bool:
    return compile_groups(hostile_groups, ignore_case).search(text) is not None or Lexicon.has_hit(
        text, floor=hostile_floor, want_negative=True
    )
