# Re-exports establish the domain's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""The correction/feedback mining mechanism.

Neutral fact-detectors over Claude Code transcripts: each iterator recognizes a
transcript shape and yields a :class:`MiningSignal` — a neutral fact carrying a
candidate trigger, confidence, and evidence, but no policy. Apps map signals to
their own candidate records with policy injected (their filter spec, their
disqualification rules, their review formats), and persist them through
:class:`FeedbackStore`.
"""

from __future__ import annotations

from cc_transcript.domains.mining.candidates import DedupKey, FeedbackCandidate, dedup_key
from cc_transcript.domains.mining.confidence import (
    HIGH,
    LOW,
    MEDIUM,
    NOISE_FLOOR,
    NONE,
    VERY_HIGH,
    CandidateSignal,
    Confidence,
    effective_confidence,
    firm,
    noise,
    strong,
    weak,
)
from cc_transcript.domains.mining.context import (
    ContextSnapshot,
    ContextTurn,
    build_snapshot,
    trigger_for,
    turn_for,
)
from cc_transcript.domains.mining.formats import ReviewComment, ReviewFormat, extract_all
from cc_transcript.domains.mining.markers import (
    DENIAL_PREFIX,
    EDIT_TOOLS,
    INTERRUPT_MARKER_RE,
    REENTRY_LOOKBACK,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)
from cc_transcript.domains.mining.nav import (
    denial_results,
    denied_tool_payload,
    embedded_user_text,
    interrupt_marker,
    is_bare_interrupt_marker,
    last_edit_index,
    marker_in,
    next_user_message,
    tool_uses,
)
from cc_transcript.domains.mining.signals import (
    DEFAULT_DETECTORS,
    MiningSignal,
    correction_text,
    iter_interrupt_marker_signals,
    iter_plan_reentry_signals,
    iter_plan_rejection_signals,
    iter_review_comment_signals,
    iter_tool_denial_signals,
    iter_user_message_signals,
    nearest_assistant_index,
)
from cc_transcript.domains.mining.sourcekind import (
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    REVIEW_COMMENT,
    TRANSCRIPT_MESSAGE,
    SourceKind,
)
from cc_transcript.domains.mining.store import FEEDBACK_DDL, FeedbackStore, Stats, event_row
