"""Typed events for Claude Code transcripts.

Discovery, a superset JSONL parser (Python + Rust), and ingestion-state tracking.
"""

from __future__ import annotations

from cc_transcript.backend import Backend, ParsedTranscript
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, TranscriptDiscovery
from cc_transcript.filters import SENTIMENT_FILTER, FilterConfig, apply_filters
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

__all__ = [
    "CLAUDE_PROJECTS_DIR",
    "SENTIMENT_FILTER",
    "AssistantEvent",
    "Backend",
    "CcVersion",
    "ContentBlock",
    "EntryMeta",
    "EntryUuid",
    "FileStateStore",
    "FilterConfig",
    "ModeEvent",
    "OtherEvent",
    "ParsedTranscript",
    "SessionId",
    "SystemEvent",
    "TextBlock",
    "ThinkingBlock",
    "ToolResultBlock",
    "ToolUseBlock",
    "ToolUseId",
    "TranscriptDiscovery",
    "TranscriptEvent",
    "TranscriptParser",
    "UserEvent",
    "apply_filters",
    "parse_events",
    "parse_events_from_bytes",
]
