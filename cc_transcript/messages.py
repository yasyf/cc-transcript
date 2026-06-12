from __future__ import annotations

from datetime import datetime
from typing import Literal, NamedTuple

from cc_transcript.models import SessionId


class MessageToolCall(NamedTuple):
    """A single tool invocation within a message: the tool ``name`` and optional target file path."""

    name: str
    file_path: str | None = None


class UserMessage(NamedTuple):
    """A user turn distilled for analysis: its text, tool calls, and authoring metadata."""

    content: str
    timestamp: datetime
    session_id: SessionId
    uuid: str
    tool_calls: tuple[MessageToolCall, ...]
    thinking_chars: int
    cc_version: str
    role: Literal["user"] = "user"


class AssistantMessage(NamedTuple):
    """An assistant turn distilled for analysis: its text, tool calls, and responding model."""

    content: str
    timestamp: datetime
    session_id: SessionId
    uuid: str
    tool_calls: tuple[MessageToolCall, ...]
    thinking_chars: int
    claude_model: str
    role: Literal["assistant"] = "assistant"


BaseMessage = UserMessage | AssistantMessage
TranscriptMessage = UserMessage | AssistantMessage
