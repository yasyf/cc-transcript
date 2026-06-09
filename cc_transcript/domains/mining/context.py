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


@dataclass(frozen=True, slots=True)
class ContextTurn:
    """One conversational turn surrounding a piece of feedback.

    Attributes:
        role: Whether the turn came from the user, the assistant, or a tool.
        text: The turn's text content.
        tool_calls: The names of the tools the turn invoked, in order.
    """

    role: Literal["user", "assistant", "tool"]
    text: str
    tool_calls: tuple[str, ...] = ()


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
    return {"role": turn.role, "text": turn.text, "tool_calls": list(turn.tool_calls)}


def turn_from_dict(data: Mapping[str, Any]) -> ContextTurn:
    return ContextTurn(role=data["role"], text=data["text"], tool_calls=tuple(data["tool_calls"]))


def turn_for(event: UserEvent | AssistantEvent) -> ContextTurn:
    match event:
        case UserEvent():
            return ContextTurn(role="user", text=event.text)
        case AssistantEvent():
            return ContextTurn(
                role="assistant",
                text=event.text[:ASSISTANT_TEXT_LIMIT],
                tool_calls=tuple(block.name for block in event.blocks if isinstance(block, ToolUseBlock)),
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
