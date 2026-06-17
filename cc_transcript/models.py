from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal, NewType

from cc_transcript.ids import EventUuid, SessionId, ToolDigest, ToolUseId, tool_digest
from cc_transcript.tools import ToolCall, parse_tool_call

if TYPE_CHECKING:
    from collections.abc import Mapping
    from datetime import datetime
    from typing import Any

CcVersion = NewType("CcVersion", str)


@dataclass(frozen=True, slots=True)
class TextBlock:
    """A text content block from a user or assistant message.

    Attributes:
        text: The block's literal text.
    """

    text: str


@dataclass(frozen=True, slots=True)
class ThinkingBlock:
    """An extended-thinking content block emitted by the assistant.

    Attributes:
        thinking: The model's thinking text.
    """

    thinking: str


@dataclass(frozen=True, slots=True)
class ToolUseBlock:
    """An assistant request to invoke a tool.

    Attributes:
        id: The tool-use identifier referenced by the matching result.
        name: The tool's name.
        input: The tool's input arguments, preserved verbatim.
    """

    id: ToolUseId
    name: str
    input: Mapping[str, Any]

    @property
    def call(self) -> ToolCall:
        """The block parsed into the typed tool-call hierarchy.

        Strict: a known tool whose input is malformed raises
        :class:`~cc_transcript.tools.ToolInputError`.
        """
        return parse_tool_call(self.name, self.input)

    @property
    def digest(self) -> ToolDigest:
        """The cross-language content digest of this call."""
        return tool_digest(self.name, self.input)


@dataclass(frozen=True, slots=True)
class ToolResultBlock:
    """The result of a tool invocation, delivered in a user turn.

    Attributes:
        tool_use_id: The id of the originating tool-use block.
        content: The result text, flattened from string or block content.
        is_error: Whether the tool reported a failure.
        is_async: Whether the originating tool ran asynchronously, read from the
            entry-level ``toolUseResult.isAsync`` marker.
    """

    tool_use_id: ToolUseId
    content: str
    is_error: bool
    is_async: bool = False


@dataclass(frozen=True, slots=True)
class FallbackBlock:
    """A marker that the assistant turn fell back from one model to another.

    Claude Code records this when a turn switches models mid-stream; it carries
    no message content, only the two model names.

    Attributes:
        from_model: The model the turn started on.
        to_model: The model the turn fell back to.
    """

    from_model: str
    to_model: str


@dataclass(frozen=True, slots=True)
class OtherBlock:
    """Any assistant content block whose ``type`` is not yet modeled.

    The escape hatch that keeps an unrecognized block from crashing the parser
    as Claude Code's transcript format evolves, mirroring :class:`OtherEvent`.

    Attributes:
        type: The block's ``type`` field.
        raw: The block's full decoded payload.
    """

    type: str
    raw: Mapping[str, Any]


ContentBlock = TextBlock | ThinkingBlock | ToolUseBlock | ToolResultBlock | FallbackBlock | OtherBlock


@dataclass(frozen=True, slots=True)
class EntryMeta:
    """Envelope metadata shared by the conversational transcript events.

    Attributes:
        uuid: The entry's unique identifier.
        parent_uuid: The parent entry's id, or None for roots.
        session_id: The session this entry belongs to.
        timestamp: The entry's timezone-aware timestamp.
        cwd: The working directory recorded for the entry.
        git_branch: The git branch recorded for the entry.
        cc_version: The Claude Code version that wrote the entry.
        is_sidechain: Whether the entry belongs to a subagent sidechain.
        is_meta: Whether the entry is a meta entry injected by the client.
        entrypoint: The entrypoint that produced the entry, e.g. ``cli``.
        is_compact_summary: Whether the entry is a compaction summary.
        is_visible_in_transcript_only: Whether the entry is transcript-only.
    """

    uuid: EventUuid
    parent_uuid: EventUuid | None
    session_id: SessionId
    timestamp: datetime
    cwd: str | None
    git_branch: str | None
    cc_version: CcVersion | None
    is_sidechain: bool
    is_meta: bool
    entrypoint: str | None
    is_compact_summary: bool
    is_visible_in_transcript_only: bool


@dataclass(frozen=True, slots=True)
class UserEvent:
    """A user turn.

    Attributes:
        meta: The entry envelope metadata.
        text: The joined text of the turn.
        blocks: The parsed content blocks, including tool results.
        interrupted: Whether the turn is a user interruption.
    """

    meta: EntryMeta
    text: str
    blocks: tuple[ContentBlock, ...]
    interrupted: bool


@dataclass(frozen=True, slots=True)
class AssistantEvent:
    """An assistant turn.

    Attributes:
        meta: The entry envelope metadata.
        model: The model that produced the turn, e.g. ``<synthetic>``.
        text: The joined text of the turn.
        blocks: The parsed content blocks, including thinking and tool uses.
        stop_reason: The model's stop reason, when present.
    """

    meta: EntryMeta
    model: str
    text: str
    blocks: tuple[ContentBlock, ...]
    stop_reason: str | None


@dataclass(frozen=True, slots=True)
class SystemEvent:
    """A system entry, such as a hook summary or notice.

    Attributes:
        meta: The entry envelope metadata.
        subtype: The system entry's subtype.
        content: The entry's text content, when present.
    """

    meta: EntryMeta
    subtype: str
    content: str | None


@dataclass(frozen=True, slots=True)
class ModeEvent:
    """A mode or permission-mode change marker.

    These entries carry only a session id on disk — no uuid, timestamp, or
    other envelope fields — so they hold a :attr:`session_id` directly rather
    than an :class:`EntryMeta`.

    Attributes:
        session_id: The session whose mode changed.
        channel: Which mode channel changed.
        value: The new mode value.
    """

    session_id: SessionId
    channel: Literal["mode", "permission-mode"]
    value: str


@dataclass(frozen=True, slots=True)
class OtherEvent:
    """Any recognized entry without a guaranteed conversational envelope.

    Covers attachment, ai-title, last-prompt, summary, queue-operation,
    file-history-snapshot, and similar entry types whose shape carries no
    :class:`EntryMeta`.

    Attributes:
        type: The entry's ``type`` field.
        raw: The entry's full decoded payload.
    """

    type: str
    raw: Mapping[str, Any]


TranscriptEvent = UserEvent | AssistantEvent | SystemEvent | ModeEvent | OtherEvent
"""The union of every typed event a parsed transcript can yield."""
