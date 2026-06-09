"""Pure navigation helpers over a transcript's ordered events."""

from __future__ import annotations

from typing import TYPE_CHECKING

from cc_transcript.models import AssistantEvent, ToolResultBlock, ToolUseBlock, UserEvent

from cc_transcript.domains.mining.markers import (
    DENIAL_PREFIX,
    EDIT_TOOLS,
    INTERRUPT_MARKER_RE,
    REENTRY_LOOKBACK,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)

if TYPE_CHECKING:
    from collections.abc import Iterator, Sequence
    from typing import Any

    from cc_transcript.models import ToolUseId, TranscriptEvent


def tool_uses(events: Sequence[TranscriptEvent]) -> dict[ToolUseId, ToolUseBlock]:
    return {
        block.id: block
        for event in events
        if isinstance(event, AssistantEvent)
        for block in event.blocks
        if isinstance(block, ToolUseBlock)
    }


def denial_results(event: UserEvent) -> Iterator[ToolResultBlock]:
    return (
        block
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
        if block.is_error
        if block.content.startswith(DENIAL_PREFIX)
    )


def embedded_user_text(content: str) -> str | None:
    if (start := content.find(USER_SAID_MARKER)) == -1:
        return None
    return content[start + len(USER_SAID_MARKER) :].split(USER_SAID_TRAILER, 1)[0].strip()


def last_edit_index(events: Sequence[TranscriptEvent], index: int) -> int | None:
    return next(
        (
            i
            for i in range(index - 1, max(index - REENTRY_LOOKBACK, 0) - 1, -1)
            if isinstance(event := events[i], AssistantEvent)
            if any(isinstance(b, ToolUseBlock) and b.name in EDIT_TOOLS for b in event.blocks)
        ),
        None,
    )


def next_user_message(events: Sequence[TranscriptEvent], index: int) -> tuple[int, UserEvent] | None:
    return next(
        (
            (i, event)
            for i in range(index, len(events))
            if isinstance(event := events[i], UserEvent)
            if event.text.strip()
        ),
        None,
    )


def denied_tool_payload(use: ToolUseBlock) -> dict[str, Any]:
    return {"tool": use.name, "file_path": use.input.get("file_path")}


def interrupt_marker(content: str) -> str | None:
    stripped = content.lstrip()
    if (match := INTERRUPT_MARKER_RE.match(stripped)) is None:
        return None
    end = stripped.find("]")
    return stripped[: end + 1] if end != -1 else match.group(0)


def is_bare_interrupt_marker(text: str) -> bool:
    return (marker := interrupt_marker(text)) is not None and not text.strip()[len(marker.strip()) :].strip()


def marker_in(event: UserEvent) -> str | None:
    return next(
        (
            marker
            for block in event.blocks
            if isinstance(block, ToolResultBlock)
            if (marker := interrupt_marker(block.content)) is not None
        ),
        None,
    )
