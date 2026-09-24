"""The one renderer: every cut the platform makes happens here, under a Budget."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript.tools import ToolCallBase

if TYPE_CHECKING:
    from cc_transcript.activity import SessionActivity, Turn
    from cc_transcript.tools import FallbackCall, ToolCall


@dataclass(frozen=True, slots=True)
class Budget:
    """Render-time character budgets — the only place the platform cuts content.

    Every cut appends an ellipsis marker carrying the omitted-character count,
    so a reader always knows how much is missing.

    Attributes:
        turn_chars: Budget for each prose chunk of a rendered turn.
        tool_chars: Budget for each content piece of a rendered tool call.
    """

    turn_chars: int = 700
    tool_chars: int = 1500


def clip(text: str, limit: int) -> str:
    return text if len(text) <= limit else f"{text[:limit]}…(+{len(text) - limit}ch)"


def render_tool_call(call: ToolCall | FallbackCall, *, budget: Budget) -> str:
    """Render a typed tool call, clipping each content piece to the tool budget.

    The single tool-input renderer: Edit renders the path plus ``- old`` /
    ``+ new`` lines, MultiEdit renders every span under an ``edit i/n``
    marker, Write renders the path plus content, Bash renders the command,
    and everything else renders the tool name with its full compact-JSON input.

    Example:
        >>> render_tool_call(parse_tool_call("Bash", {"command": "ls"}), budget=Budget())
        'ls'
    """
    if isinstance(call, ToolCallBase):
        return _native.render_tool_call_view(call, budget.turn_chars, budget.tool_chars)
    return _native.render_tool_call(
        call.name, json.dumps(call.raw, default=str), budget.turn_chars, budget.tool_chars
    )


def hunk_lines(old: str, new: str, *, budget: Budget) -> tuple[str, ...]:
    return (*prefixed("- ", clip(old, budget.tool_chars)), *prefixed("+ ", clip(new, budget.tool_chars)))


def prefixed(prefix: str, text: str) -> tuple[str, ...]:
    return tuple(f"{prefix}{line}" for line in text.splitlines()) or (prefix.rstrip(),)


def render_turn(turn: Turn, *, budget: Budget, tool_results: bool = False) -> str:
    """Render one turn: the prompt, the messages queued mid-turn, assistant prose, and every tool call, in order.

    A message queued while the agent worked renders under who sent it: ``user:`` for
    the user's own, ``notification:`` for a background task's, ``peer:`` for another
    agent's, ``channel:`` for an MCP channel's.

    Prose chunks clip to ``budget.turn_chars``; each tool call renders via
    :func:`render_tool_call` under ``budget.tool_chars``. An AskUserQuestion answer
    renders as ``user answered:`` with the chosen option's description, the
    selected preview and any notes.

    Args:
        turn: The turn to render.
        budget: Character budgets for prose and tool calls.
        tool_results: Render each other tool result after its call: a ``result:``
            head, or ``failed:`` when the call errored, naming the tool, then the
            content clipped to ``budget.turn_chars`` on ``> `` lines. Calls batched in
            one message and their results carry a ``call i/n``/``result i/n`` ordinal.
    """
    return _native.render_turn_from_events(
        turn.prompt,
        list(turn.events),
        budget.turn_chars,
        budget.tool_chars,
        tool_results,
    )


def render_session(activity: SessionActivity, *, budget: Budget, tool_results: bool = False) -> str:
    """Render every turn of a session under ``budget``, separated by blank lines.

    Args:
        activity: The session to render.
        budget: Character budgets for prose and tool calls.
        tool_results: Render each tool result after its call; see :func:`render_turn`.
    """
    return "\n\n".join(
        rendered
        for turn in activity.turns
        if (rendered := render_turn(turn, budget=budget, tool_results=tool_results))
    )
