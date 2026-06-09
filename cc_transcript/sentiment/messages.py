# Re-exports preserve the pre-0.6 import path; the distilled message types now live in core.
# pyright: reportUnusedImport=false
"""Deprecated shim: distilled message types moved to :mod:`cc_transcript.messages`."""

from __future__ import annotations

from cc_transcript.messages import (
    AssistantMessage,
    BaseMessage,
    ToolCall,
    TranscriptMessage,
    UserMessage,
)
