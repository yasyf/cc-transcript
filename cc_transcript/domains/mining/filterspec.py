"""Declarative filtering of mined feedback candidates.

A :class:`CandidateFilterSpec` is an ordered tuple of :class:`CandidateClause`
rules — the candidate-level companion to the event-level
:class:`cc_transcript.filterspec.FilterSpec`. A candidate survives when every
clause matches, after per-clause negation. The core ships no concrete spec and
no thresholds; the consumer owns policy and composes its own spec from the
builders here.

Example:
    >>> from cc_transcript.domains.mining import NOISE_FLOOR, REVIEW_COMMENT
    >>> spec = build_candidate_filter(at_least(NOISE_FLOOR), only_kinds(REVIEW_COMMENT))
    >>> kept = list(apply_candidate_filter(candidates, spec))
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript.domains.mining.confidence import effective_confidence

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator

    from cc_transcript.domains.mining.candidates import FeedbackCandidate
    from cc_transcript.domains.mining.confidence import Confidence
    from cc_transcript.domains.mining.sourcekind import SourceKind


@dataclass(frozen=True, slots=True)
class ConfidenceAtLeast:
    """Matches candidates whose effective confidence is at least ``floor``.

    A candidate without a stored signal scores
    :data:`~cc_transcript.domains.mining.confidence.MEDIUM` via
    :func:`~cc_transcript.domains.mining.confidence.effective_confidence`, so
    legacy rows pass any floor at or below it.
    """

    floor: Confidence


@dataclass(frozen=True, slots=True)
class SourceKindIn:
    """Matches candidates whose ``source_kind`` is in ``kinds``."""

    kinds: frozenset[SourceKind]


@dataclass(frozen=True, slots=True)
class HasReason:
    """Matches candidates whose signal carries the reason code ``reason``."""

    reason: str


@dataclass(frozen=True, slots=True)
class IsDurable:
    """Matches candidates whose signal durability equals ``want``.

    A candidate without a stored signal counts as durable, mirroring
    :class:`~cc_transcript.domains.mining.confidence.CandidateSignal`'s default.
    """

    want: bool


CandidatePredicate = ConfidenceAtLeast | SourceKindIn | HasReason | IsDurable


@dataclass(frozen=True, slots=True)
class CandidateClause:
    """One filter rule: the candidate must satisfy ``predicate``.

    Attributes:
        predicate: The condition tested against a candidate.
        negate: Invert the predicate match.
    """

    predicate: CandidatePredicate
    negate: bool = False


@dataclass(frozen=True, slots=True)
class CandidateFilterSpec:
    """An ordered tuple of :class:`CandidateClause` rules, all of which must hold."""

    clauses: tuple[CandidateClause, ...]


def predicate_matches(predicate: CandidatePredicate, candidate: FeedbackCandidate) -> bool:
    match predicate:
        case ConfidenceAtLeast(floor):
            return effective_confidence(candidate.signal) >= floor
        case SourceKindIn(kinds):
            return candidate.source_kind in kinds
        case HasReason(reason):
            return candidate.signal is not None and reason in candidate.signal.reasons
        case IsDurable(want):
            return (candidate.signal is None or candidate.signal.durable) is want


def keep_candidate(candidate: FeedbackCandidate, spec: CandidateFilterSpec) -> bool:
    """Returns whether ``candidate`` satisfies every clause of ``spec``."""
    return all(predicate_matches(clause.predicate, candidate) is not clause.negate for clause in spec.clauses)


def apply_candidate_filter(
    candidates: Iterable[FeedbackCandidate], spec: CandidateFilterSpec
) -> Iterator[FeedbackCandidate]:
    """Yields the candidates that satisfy every clause of ``spec``."""
    return (candidate for candidate in candidates if keep_candidate(candidate, spec))


def at_least(floor: Confidence) -> CandidateClause:
    """Returns a clause keeping candidates at or above ``floor`` confidence."""
    return CandidateClause(ConfidenceAtLeast(floor))


def only_kinds(*kinds: SourceKind) -> CandidateClause:
    """Returns a clause keeping candidates whose ``source_kind`` is one of ``kinds``."""
    return CandidateClause(SourceKindIn(frozenset(kinds)))


def build_candidate_filter(*clauses: CandidateClause) -> CandidateFilterSpec:
    """Composes ``clauses`` into a :class:`CandidateFilterSpec`."""
    return CandidateFilterSpec(clauses=clauses)
