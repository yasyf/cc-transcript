"""Session-activity probe — captain-hook's ``is_waiting`` oracle over one transcript.

:func:`session_activity_probe` is the public entry: it hands a ``.jsonl`` transcript to the
Rust extension, which reads and parses the file and runs the oracle in one pass, returning a
:class:`SessionActivityProbe`. The verdict spans undelivered task notifications (the
:class:`~cc_transcript.notifications.Notifications` queue replay), ephemeral waits (waiting
tools, backgrounded Agent/Task/Bash, subagentless Agent/Task) over the current turn, and
pending async launches (async Agent/Task, live Workflow) session-wide — a launch counts as
completed only once its notification was delivered or drained, never when merely enqueued —
and flags unanswered non-human-facing tool calls in the current turn as mid-tool.
Compact-summary user lines do not open turns, matching captain-hook, which consumes this
probe since the 10.2.0 shared-classifier fix.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from cc_transcript.tools import expand_tool_names

if TYPE_CHECKING:
    from pathlib import Path

DEFAULT_WAITING_TOOLS = frozenset({"Monitor", "ScheduleWakeup", "SendMessage", "TeamCreate"})
DEFAULT_HUMAN_FACING_TOOLS = expand_tool_names("AskUserQuestion|ExitPlanMode")

PendingKind = Literal[
    "waiting_tool",
    "background",
    "subagentless_task",
    "pending_async_task",
    "pending_async_workflow",
    "mid_tool",
]
"""How a tool call contributes to the session-activity verdict."""


@dataclass(frozen=True, slots=True)
class PendingItem:
    """One tool call contributing to the session-activity verdict.

    Attributes:
        tool_use_id: The contributing call's tool-use id.
        name: The tool's name exactly as invoked.
        kind: How the call contributes to the verdict.
    """

    tool_use_id: str | None
    name: str
    kind: PendingKind


@dataclass(frozen=True, slots=True)
class SessionActivityProbe:
    """The session-activity verdict over one transcript.

    Attributes:
        is_waiting: Whether the session waits on ephemeral or pending async work.
        mid_tool: Whether the current turn has an unanswered non-human-facing call.
        pending: The contributing calls, in document order, deduped by tool-use id.
        last_event_epoch: The latest envelope timestamp as epoch seconds, or None.
    """

    is_waiting: bool
    mid_tool: bool
    pending: tuple[PendingItem, ...]
    last_event_epoch: int | None


def session_activity_probe(
    path: Path,
    *,
    waiting_tools: frozenset[str] = DEFAULT_WAITING_TOOLS,
    human_facing_tools: frozenset[str] = DEFAULT_HUMAN_FACING_TOOLS,
) -> SessionActivityProbe:
    """Probes the transcript at ``path`` for session activity.

    The Rust extension reads and parses the file and runs the oracle in one pass,
    returning the session-activity verdict.

    Args:
        path: The ``.jsonl`` transcript to probe.
        waiting_tools: Tool names whose calls in the current turn mark the session waiting.
        human_facing_tools: Tool names whose unanswered calls are the user's move, never mid-tool.

    Returns:
        The session-activity verdict.
    """
    from cc_transcript import _native

    payload = _native.session_activity_probe(str(path), sorted(waiting_tools), sorted(human_facing_tools))
    return SessionActivityProbe(
        is_waiting=payload["is_waiting"],
        mid_tool=payload["mid_tool"],
        pending=tuple(PendingItem(**item) for item in payload["pending"]),
        last_event_epoch=payload["last_event_epoch"],
    )
