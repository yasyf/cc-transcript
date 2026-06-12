# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""The correction/feedback mining mechanism.

Neutral fact-detectors over Claude Code transcripts: each iterator recognizes a
transcript shape and yields a :class:`MiningSignal` — a neutral fact carrying a
candidate trigger, confidence, and evidence, but no policy. Apps map signals to
their own candidate records with policy injected (their filter spec, their
disqualification rules, their review formats), capture each candidate's durable
:class:`~cc_transcript.context.ContextWindow` via
:func:`~cc_transcript.context.capture_window`, and persist them through
:class:`FeedbackStore`. LLM verdict passes over the stored corpus live in
:mod:`cc_transcript.judge`.
"""

from __future__ import annotations

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
from cc_transcript.mining.formats import ReviewComment, ReviewFormat, extract_all
from cc_transcript.mining.signals import (
    DEFAULT_DETECTORS,
    DENIAL_PREFIX,
    EDIT_TOOLS,
    REENTRY_LOOKBACK,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
    MiningSignal,
    correction_text,
    denial_results,
    denied_tool_payload,
    embedded_user_text,
    interrupt_marker,
    is_bare_interrupt_marker,
    iter_interrupt_marker_signals,
    iter_plan_reentry_signals,
    iter_plan_rejection_signals,
    iter_review_comment_signals,
    iter_tool_denial_signals,
    iter_user_message_signals,
    last_edit_index,
    marker_in,
    nearest_assistant_index,
    next_user_message,
)
from cc_transcript.mining.sourcekind import (
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
    SourceKind,
)
from cc_transcript.mining.store import FEEDBACK_DDL, FeedbackStore, Stats, event_row
