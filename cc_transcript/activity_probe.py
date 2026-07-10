"""Dual-backend session-activity probe — captain-hook's ``is_waiting`` oracle over one transcript.

:func:`rust_probe_backend` resolves the Rust extension when built, else None to fall back
to the Python reference. :func:`session_activity_probe` is the public dual-backend entry:
it reads a ``.jsonl`` transcript, runs the oracle over its parsed events, and returns a
:class:`SessionActivityProbe`; :func:`probe_events` is the Python reference the Rust probe
stays identical to. The verdict spans undelivered task notifications (the
:class:`~cc_transcript.notifications.Notifications` queue replay), ephemeral waits
(waiting tools, backgrounded Agent/Task/Bash, subagentless Agent/Task) over the current
turn, and pending async launches (async Agent/Task, live Workflow) session-wide — a
launch counts as completed only once its notification was delivered or drained, never
when merely enqueued — and flags unanswered non-human-facing tool calls in the current
turn as mid-tool. Compact-summary user lines do not open turns, matching captain-hook,
which consumes this probe since the 10.2.0 shared-classifier fix.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from cc_transcript.activity import native_user_classifier
from cc_transcript.models import (
    AssistantEvent,
    SystemEvent,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.notifications import Notifications
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.tools import expand_tool_names, matches_names

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path
    from types import ModuleType

    from cc_transcript.models import TranscriptEvent

DEFAULT_WAITING_TOOLS = frozenset({"Monitor", "ScheduleWakeup", "SendMessage", "TeamCreate"})
DEFAULT_HUMAN_FACING_TOOLS = expand_tool_names("AskUserQuestion|ExitPlanMode")

BACKGROUND_TOOLS = expand_tool_names("Agent|Task|Bash")
TASK_TOOLS = expand_tool_names("Agent|Task")

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


def ephemeral_wait(block: ToolUseBlock, waiting_tools: frozenset[str]) -> PendingKind | None:
    if matches_names(block.name, waiting_tools):
        return "waiting_tool"
    if matches_names(block.name, BACKGROUND_TOOLS) and block.input.get("run_in_background") is True:
        return "background"
    if matches_names(block.name, TASK_TOOLS) and not isinstance(block.input.get("subagent_type"), str):
        return "subagentless_task"
    return None


def pending_async(
    block: ToolUseBlock, result: ToolResultBlock | None, notifications: Notifications
) -> PendingKind | None:
    match block.name:
        case "Agent" | "Task" if result is not None and result.is_async and not notifications.completed(block.id):
            return "pending_async_task"
        case "Workflow" if not notifications.completed(block.id):
            return "pending_async_workflow"
    return None


def probe_events(
    events: Sequence[TranscriptEvent],
    *,
    waiting_tools: frozenset[str] = DEFAULT_WAITING_TOOLS,
    human_facing_tools: frozenset[str] = DEFAULT_HUMAN_FACING_TOOLS,
) -> SessionActivityProbe:
    """The Python reference oracle over parsed events, twin of the Rust probe.

    An undelivered task notification alone sets ``is_waiting`` with no pending
    item — a resumed session's orphan has no launch to point at.
    """
    results = {
        block.tool_use_id: block
        for event in events
        if isinstance(event, UserEvent | AssistantEvent)
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
    }
    notifications = Notifications.from_events(events)
    turn_start = next(
        (
            index
            for index in reversed(range(len(events)))
            if isinstance(event := events[index], UserEvent) and native_user_classifier(event)
        ),
        0,
    )

    is_waiting = notifications.has_pending
    mid_tool = False
    pending: list[PendingItem] = []
    seen: set[str] = set()
    for index, event in enumerate(events):
        if not isinstance(event, UserEvent | AssistantEvent):
            continue
        for block in event.blocks:
            if not isinstance(block, ToolUseBlock):
                continue
            result = results.get(block.id)
            if result is not None and result.is_error:
                continue
            in_current_turn = index >= turn_start
            waiting_kind = (ephemeral_wait(block, waiting_tools) if in_current_turn else None) or pending_async(
                block, result, notifications
            )
            unmatched = in_current_turn and result is None and not matches_names(block.name, human_facing_tools)
            is_waiting = is_waiting or waiting_kind is not None
            mid_tool = mid_tool or unmatched
            kind = waiting_kind or ("mid_tool" if unmatched else None)
            if kind is None or block.id in seen:
                continue
            seen.add(block.id)
            pending.append(PendingItem(tool_use_id=block.id, name=block.name, kind=kind))
    return SessionActivityProbe(
        is_waiting=is_waiting,
        mid_tool=mid_tool,
        pending=tuple(pending),
        last_event_epoch=max(
            (
                int(event.meta.timestamp.timestamp())
                for event in events
                if isinstance(event, UserEvent | AssistantEvent | SystemEvent)
            ),
            default=None,
        ),
    )


def rust_probe_backend() -> ModuleType | None:
    """The Rust probe when the extension is built with it; else None → the Python reference."""
    if os.environ.get("CC_TRANSCRIPT_DISABLE_RUST"):
        return None
    try:
        from cc_transcript import _parser_rs
    except ImportError:
        return None
    return _parser_rs if hasattr(_parser_rs, "session_activity_probe") else None


def session_activity_probe(
    path: Path,
    *,
    waiting_tools: frozenset[str] = DEFAULT_WAITING_TOOLS,
    human_facing_tools: frozenset[str] = DEFAULT_HUMAN_FACING_TOOLS,
) -> SessionActivityProbe:
    """Probes the transcript at ``path`` for session activity via the active backend.

    The Rust backend reads and parses the file inside the extension in one pass; the
    Python reference parses via :func:`~cc_transcript.parser.parse_events_from_bytes`
    and runs :func:`probe_events`. Both yield identical verdicts.

    Args:
        path: The ``.jsonl`` transcript to probe.
        waiting_tools: Tool names whose calls in the current turn mark the session waiting.
        human_facing_tools: Tool names whose unanswered calls are the user's move, never mid-tool.

    Returns:
        The session-activity verdict.
    """
    rust = rust_probe_backend()
    if rust is None:
        return probe_events(
            parse_events_from_bytes(path.read_bytes()),
            waiting_tools=waiting_tools,
            human_facing_tools=human_facing_tools,
        )
    payload = rust.session_activity_probe(str(path), sorted(waiting_tools), sorted(human_facing_tools))
    return SessionActivityProbe(
        is_waiting=payload["is_waiting"],
        mid_tool=payload["mid_tool"],
        pending=tuple(PendingItem(**item) for item in payload["pending"]),
        last_event_epoch=payload["last_event_epoch"],
    )
