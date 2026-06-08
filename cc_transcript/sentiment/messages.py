from __future__ import annotations

from datetime import datetime
from typing import Literal, NamedTuple

from cc_transcript.models import SessionId


class ToolCall(NamedTuple):
    name: str
    file_path: str | None = None


class UserMessage(NamedTuple):
    content: str
    timestamp: datetime
    session_id: SessionId
    uuid: str
    tool_calls: tuple[ToolCall, ...]
    thinking_chars: int
    cc_version: str
    role: Literal["user"] = "user"


class AssistantMessage(NamedTuple):
    content: str
    timestamp: datetime
    session_id: SessionId
    uuid: str
    tool_calls: tuple[ToolCall, ...]
    thinking_chars: int
    claude_model: str
    role: Literal["assistant"] = "assistant"


BaseMessage = UserMessage | AssistantMessage
TranscriptMessage = UserMessage | AssistantMessage
