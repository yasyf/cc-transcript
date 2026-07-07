"""The harness notification-delivery queue, replayed from a transcript.

Claude Code enqueues background-task notifications (and queued user commands)
into a FIFO, then drains it as each item reaches the agent, recording every
``enqueue``/``dequeue``/``remove``/``popAll`` as a ``queue-operation`` audit
entry. :class:`Notifications` replays that audit trail over a session's events
to model which notifications are still queued, which were delivered, and which
ever passed through — so a hook can ask whether a given background tool call has
already been reported to the agent.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript.models import OtherEvent, UserEvent

if TYPE_CHECKING:
    from collections.abc import Sequence

    from cc_transcript.models import TranscriptEvent

NOTIFICATION_MARKER = "<task-notification>"


def tool_use_marker(tool_use_id: str) -> str:
    return f"<tool-use-id>{tool_use_id}</tool-use-id>"


def delivered_text(event: TranscriptEvent) -> str | None:
    match event:
        case UserEvent(text=text) if NOTIFICATION_MARKER in text:
            return text
        case OtherEvent(type="attachment", raw=raw) if (attachment := raw.get("attachment", {})).get(
            "type"
        ) == "queued_command":
            return str(attachment.get("prompt", ""))
        case _:
            return None


def replay_queue(events: Sequence[TranscriptEvent]) -> tuple[tuple[str, ...], tuple[str, ...]]:
    queued: list[str] = []
    enqueued: list[str] = []
    for event in events:
        if not (isinstance(event, OtherEvent) and event.type == "queue-operation"):
            continue
        match event.raw.get("operation"):
            case "enqueue":
                enqueued.append(content := str(event.raw.get("content", "")))
                queued.append(content)
            case "dequeue" | "remove" if queued:
                queued.pop(0)
            case "popAll":
                content = str(event.raw.get("content", ""))
                queued = [item for item in queued if item not in content]
    return tuple(queued), tuple(enqueued)


@dataclass(frozen=True, slots=True)
class Notifications:
    """The modeled state of a session's harness notification-delivery queue.

    Reconstructed by replaying the transcript's ``queue-operation`` audit
    records in order: each ``enqueue`` appends its content to a FIFO, each
    ``dequeue``/``remove`` drops the head, and each ``popAll`` subtracts every
    queued item whose text is contained in the operation's content — a queued
    user command drains without taking an unrelated notification with it.

    Attributes:
        queued: The enqueue contents still in the modeled FIFO, undelivered.
        delivered: The notification texts that actually reached the agent —
            user turns carrying a ``<task-notification>`` plus every queued
            command replayed to the model.
        enqueued: Every enqueue content ever observed, in order.

    Example:
        >>> session.notifications.completed("toolu_01XVXcp6yKvn2xbmPxdf1a3z")
        True
    """

    queued: tuple[str, ...]
    delivered: tuple[str, ...]
    enqueued: tuple[str, ...]

    @classmethod
    def from_events(cls, events: Sequence[TranscriptEvent]) -> Notifications:
        """Replays the notification queue over ``events``, in order."""
        queued, enqueued = replay_queue(events)
        return cls(
            queued=queued,
            delivered=tuple(text for event in events if (text := delivered_text(event)) is not None),
            enqueued=enqueued,
        )

    def completed(self, tool_use_id: str) -> bool:
        """Whether the tool call's notification has reached the agent.

        True when the notification was delivered, or when it was enqueued at
        some point yet no longer sits undelivered in the queue.
        """
        marker = tool_use_marker(tool_use_id)
        return any(marker in text for text in self.delivered) or (
            any(marker in text for text in self.enqueued) and not any(marker in text for text in self.queued)
        )

    def pending(self, tool_use_id: str) -> bool:
        """Whether the tool call's notification is still queued for delivery."""
        return any(tool_use_marker(tool_use_id) in text for text in self.queued)

    @property
    def has_pending(self) -> bool:
        """Whether any queued item is an undelivered task notification."""
        return any(NOTIFICATION_MARKER in text for text in self.queued)
