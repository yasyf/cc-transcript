# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""The correction/feedback mining mechanism.

Neutral fact-detectors over Claude Code transcripts, driven by a declarative
:class:`MiningSpec`: :func:`mine` interprets the spec and yields a
:class:`MiningSignal` per recognized transcript shape — a neutral fact carrying a
candidate trigger, confidence, and evidence, but no policy. Apps map signals to
their own candidate records with policy injected (their filter spec, their
disqualification rules, their review formats), capture each candidate's durable
:class:`~cc_transcript.context.ContextWindow` via
:func:`~cc_transcript.context.capture_window`, and persist them through
:class:`FeedbackStore`. LLM verdict passes over the stored corpus live in
:mod:`cc_transcript.judge`.

The :class:`MiningSpec` is the mining analogue of :class:`~cc_transcript.FilterSpec`
and :class:`~cc_transcript.sentiment.ScoreSpec`: a frozen-dataclass tree with a JSON
contract (:func:`mining_spec_to_json`) that the Python reference executor here and,
when :func:`mining_spec_is_portable` holds, the Rust backend both interpret.
"""

from __future__ import annotations

from cc_transcript.filterspec import (
    ANSWERED_PREFIX,
    ANSWERED_TRAILER,
    DENIAL_PREFIX,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)
from cc_transcript.mining.candidates import DedupKey, FeedbackCandidate, dedup_key
from cc_transcript.mining.confidence import (
    HIGH,
    LOW,
    MEDIUM,
    NOISE_FLOOR,
    NONE,
    VERY_HIGH,
    CandidateSignal,
    Confidence,
    firm,
    noise,
    strong,
    weak,
)
from cc_transcript.mining.engine import mine_signals, rehydrate_signal, rust_mine_backend
from cc_transcript.mining.filterspec import (
    CandidateClause,
    CandidateFilterSpec,
    CandidatePredicate,
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
from cc_transcript.mining.formats import (
    ReviewComment,
    StructuredFormat,
    extract_structured,
)
from cc_transcript.mining.signals import (
    MiningSignal,
    mine,
)
from cc_transcript.mining.sourcekind import (
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    QUESTION_ANSWER,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
    SourceKind,
)
from cc_transcript.mining.spec import (
    CALIBRATED_SPEC,
    HEDGE_GROUPS,
    USER_MESSAGE_SPEC,
    Base,
    BumpIfProximate,
    BumpIfSubstantive,
    CallableReviewFormat,
    ConfidenceSpec,
    ConfStage,
    DemoteIfHedged,
    DemoteIfShort,
    DetectorName,
    MiningSpec,
    NoiseIfStructural,
    Provenance,
    ProvenanceSpec,
    RegexReviewFormat,
    ReviewFormat,
    ReviewSpec,
    mining_spec_is_portable,
    mining_spec_to_json,
    signal_to_dict,
)
from cc_transcript.mining.store import FEEDBACK_DDL, FeedbackStore, Stats, event_row
