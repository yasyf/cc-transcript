"""The conversational-window primitive captured around each piece of feedback."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from cc_transcript.models import AssistantEvent, ToolUseBlock, UserEvent

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from typing import Any

    from cc_transcript.models import TranscriptEvent

ASSISTANT_TEXT_LIMIT = 2000
TOOL_INPUT_LIMIT = 1500


@dataclass(frozen=True, slots=True)
class ContextTurn:
    """One conversational turn surrounding a piece of feedback.

    Attributes:
        role: Whether the turn came from the user, the assistant, or a tool.
        text: The turn's text content.
        tool_calls: The names of the tools the turn invoked, in order.
        tool_inputs: One input summary per tool call, in the same order.
    """

    role: Literal["user", "assistant", "tool"]
    text: str
    tool_calls: tuple[str, ...] = ()
    tool_inputs: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ContextSnapshot:
    """The conversational window around a piece of feedback.

    Attributes:
        before: The turns leading up to the trigger.
        trigger: The assistant action the feedback responds to, when known.
        after: The turns following the trigger.
    """

    before: tuple[ContextTurn, ...]
    trigger: ContextTurn | None
    after: tuple[ContextTurn, ...]

    def to_json(self) -> str:
        """Serializes the snapshot to the JSON stored in ``context_json``."""
        return json.dumps(
            {
                "before": [turn_to_dict(turn) for turn in self.before],
                "trigger": turn_to_dict(self.trigger) if self.trigger else None,
                "after": [turn_to_dict(turn) for turn in self.after],
            }
        )

    @classmethod
    def from_json(cls, raw: str) -> ContextSnapshot:
        """Deserializes a snapshot from a ``context_json`` string."""
        data = json.loads(raw)
        return cls(
            before=tuple(turn_from_dict(turn) for turn in data["before"]),
            trigger=turn_from_dict(data["trigger"]) if data["trigger"] else None,
            after=tuple(turn_from_dict(turn) for turn in data["after"]),
        )


def turn_to_dict(turn: ContextTurn) -> dict[str, Any]:
    return {
        "role": turn.role,
        "text": turn.text,
        "tool_calls": list(turn.tool_calls),
        "tool_inputs": list(turn.tool_inputs),
    }


def turn_from_dict(data: Mapping[str, Any]) -> ContextTurn:
    return ContextTurn(
        role=data["role"],
        text=data["text"],
        tool_calls=tuple(data["tool_calls"]),
        tool_inputs=tuple(data.get("tool_inputs", ())),
    )


def summarize_tool_input(name: str, input: Mapping[str, Any]) -> str:
    """Summarizes one tool call's input for context snapshots.

    Extracts the field that captures what the tool actually did — the Bash
    command, the Edit diff, the plan body — falling back to the raw JSON for
    unrecognized tools, truncated to :data:`TOOL_INPUT_LIMIT`.

    Args:
        name: The tool's name as recorded in the transcript.
        input: The tool call's input mapping, preserved verbatim by the parser.

    Returns:
        The bounded one-string summary of the call.
    """
    match name:
        case "Bash":
            summary = str(input.get("command", ""))
        case "Edit":
            summary = f"{input.get('file_path', '')}\n- {input.get('old_string', '')}\n+ {input.get('new_string', '')}"
        case "MultiEdit":
            first: Mapping[str, Any] = next(iter(input.get("edits") or ()), {})
            summary = f"{input.get('file_path', '')}\n- {first.get('old_string', '')}\n+ {first.get('new_string', '')}"
        case "Write":
            summary = f"{input.get('file_path', '')}\n{input.get('content', '')}"
        case "ExitPlanMode":
            summary = str(input.get("plan", ""))
        case "Task" | "Agent":
            summary = str(input.get("prompt", ""))
        case _:
            summary = json.dumps(dict(input))
    return summary[:TOOL_INPUT_LIMIT]


def turn_for(event: UserEvent | AssistantEvent) -> ContextTurn:
    match event:
        case UserEvent():
            return ContextTurn(role="user", text=event.text)
        case AssistantEvent():
            uses = tuple(block for block in event.blocks if isinstance(block, ToolUseBlock))
            return ContextTurn(
                role="assistant",
                text=event.text[:ASSISTANT_TEXT_LIMIT],
                tool_calls=tuple(use.name for use in uses),
                tool_inputs=tuple(summarize_tool_input(use.name, use.input) for use in uses),
            )


def trigger_for(events: Sequence[TranscriptEvent], index: int, lower: int) -> ContextTurn | None:
    return next(
        (
            turn_for(event)
            for i in range(index - 1, lower - 1, -1)
            if isinstance(event := events[i], AssistantEvent)
        ),
        None,
    )


def build_snapshot(
    events: Sequence[TranscriptEvent],
    index: int,
    *,
    before: int = 6,
    after: int = 2,
    lower_bound: int | None = None,
) -> ContextSnapshot:
    """Builds the conversational window around the event at ``index``.

    A turn is a :class:`UserEvent` or :class:`AssistantEvent`; system, mode, and
    other events are skipped. The trigger is the nearest preceding assistant
    turn — the action the feedback responds to.

    Args:
        events: The full ordered event stream for one transcript.
        index: The index of the event the feedback was attached to.
        before: The maximum number of turns to capture before the trigger.
        after: The maximum number of turns to capture after the index.
        lower_bound: When set, an event index the ``before`` window and trigger
            search may not reach back past — used to anchor plan-review context
            to the triggering edit cycle.

    Returns:
        The assembled :class:`ContextSnapshot`.
    """
    lower = lower_bound if lower_bound is not None else 0
    return ContextSnapshot(
        before=tuple(
            turn_for(event)
            for i in range(index - 1, lower - 1, -1)
            if isinstance(event := events[i], UserEvent | AssistantEvent)
        )[:before][::-1],
        trigger=trigger_for(events, index, lower),
        after=tuple(
            turn_for(event)
            for i in range(index + 1, len(events))
            if isinstance(event := events[i], UserEvent | AssistantEvent)
        )[:after],
    )
