# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""Typed events for Claude Code transcripts.

Discovery, a superset JSONL parser (Python + Rust), and ingestion-state tracking.
"""

from __future__ import annotations

from cc_transcript.backend import Backend, ParsedTranscript
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, TranscriptDiscovery
from cc_transcript.filters import JUNK_USER_MESSAGE_RE, SENTIMENT_FILTER, FilterConfig, apply_filters
from cc_transcript.filterspec import (
    FRUSTRATION_GROUPS,
    INTERRUPT_MARKER_GROUPS,
    INTERRUPT_MARKER_RE,
    MILD_IMPATIENCE_GROUPS,
    PUSHBACK_SPEC,
    RESUME_PHRASE_SET,
    SENTIMENT_JUNK_GROUPS,
    SENTIMENT_SPEC,
    SENTIMENT_STRUCTURAL_GROUPS,
    SHORT_MESSAGE_MAX_WORDS,
    STOP_HOOK_GROUPS,
    STOP_HOOK_RE,
    STRUCTURAL_NOISE_GROUPS,
    STRUCTURAL_NOISE_RE,
    TRIVIAL_ACK_SET,
    Action,
    Clause,
    EntrypointIn,
    FilterSpec,
    KindIs,
    MetaFlag,
    ModelIs,
    TextEmpty,
    TextInSet,
    TextMatchesAny,
    WordCountAtMost,
    annotate_spec,
    apply_spec,
    is_portable,
    keep,
    labels_for,
    spec_to_json,
)
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    EntryUuid,
    ModeEvent,
    OtherEvent,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    TranscriptEvent,
    UserEvent,
)
from cc_transcript.parser import TranscriptParser, parse_events, parse_events_from_bytes
from cc_transcript.store import FileStateStore
