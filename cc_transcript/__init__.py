# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""Typed events for Claude Code transcripts.

Discovery, a superset JSONL parser (Python + Rust), and ingestion-state tracking.
"""

from __future__ import annotations

from cc_transcript.backend import Backend, ParsedTranscript
from cc_transcript.builders import (
    NOISE_SPEC,
    build_spec,
    drop_compacted,
    drop_empty,
    drop_entrypoints,
    drop_junk,
    drop_meta_flag,
    drop_phrases,
    drop_short,
    drop_sidechain,
    drop_synthetic,
    keep_only,
)
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, TranscriptDiscovery
from cc_transcript.filters import JUNK_USER_MESSAGE_RE, FilterConfig, apply_filters
from cc_transcript.filterspec import (
    ASSISTANTS,
    INTERRUPT_MARKER_RE,
    RESUME_PHRASE_SET,
    STOP_HOOK_RE,
    STRUCTURAL_NOISE_RE,
    TRIVIAL_ACK_SET,
    USERS,
    FilterSpec,
    annotate_spec,
    apply_spec,
    keep,
    labels_for,
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
