"""The one renderer: every cut the platform makes happens here, under a Budget."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript.models import AssistantEvent, TextBlock, ToolUseBlock
from cc_transcript.tools import BashCall, EditCall, MultiEditCall, WriteCall

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping
    from typing import Any

    from cc_transcript.activity import SessionActivity, ToolUse, Turn
    from cc_transcript.models import ContentBlock
    from cc_transcript.tools import FallbackCall, ToolCall

PRIMARY_KEYS = ("file_path", "path", "command", "pattern", "url", "prompt", "query", "description")


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
    and everything else renders a compact ``name(primary-arg)`` line.

    Example:
        >>> render_tool_call(parse_tool_call("Bash", {"command": "ls"}), budget=Budget())
        'ls'
    """
    match call:
        case BashCall(command=command):
            return clip(command, budget.tool_chars)
        case EditCall(file_path=file_path, old=old, new=new):
            return "\n".join((f"Edit {file_path}", *hunk_lines(old, new, budget=budget)))
        case MultiEditCall(file_path=file_path, edits=edits):
            return "\n".join(
                (
                    f"MultiEdit {file_path}",
                    *(
                        line
                        for i, span in enumerate(edits, 1)
                        for line in (f"edit {i}/{len(edits)}", *hunk_lines(span.old, span.new, budget=budget))
                    ),
                )
            )
        case WriteCall(file_path=file_path, content=content):
            return f"Write {file_path}\n{clip(content, budget.tool_chars)}"
        case _:
            return f"{call.name}({clip(primary_arg(call.raw), budget.tool_chars)})"


def hunk_lines(old: str, new: str, *, budget: Budget) -> tuple[str, ...]:
    return (*prefixed("- ", clip(old, budget.tool_chars)), *prefixed("+ ", clip(new, budget.tool_chars)))


def prefixed(prefix: str, text: str) -> tuple[str, ...]:
    return tuple(f"{prefix}{line}" for line in text.splitlines()) or (prefix.rstrip(),)


def render_turn(turn: Turn, *, budget: Budget) -> str:
    """Render one turn: the prompt, assistant prose, and every tool call, in order.

    Prose chunks clip to ``budget.turn_chars``; each tool call renders via
    :func:`render_tool_call` under ``budget.tool_chars``.
    """
    calls = iter(turn.tool_uses)
    return "\n".join(
        (
            *((f"user: {clip(turn.prompt, budget.turn_chars)}",) if turn.prompt else ()),
            *(
                part
                for event in turn.events
                if isinstance(event, AssistantEvent)
                for block in event.blocks
                for part in turn_block_parts(block, calls, budget=budget)
            ),
        )
    )


def turn_block_parts(block: ContentBlock, calls: Iterator[ToolUse], *, budget: Budget) -> tuple[str, ...]:
    match block:
        case TextBlock(text=text) if text.strip():
            return (f"assistant: {clip(text, budget.turn_chars)}",)
        case ToolUseBlock():
            return (render_tool_call(next(calls).call, budget=budget),)
        case _:
            return ()


def render_session(activity: SessionActivity, *, budget: Budget) -> str:
    """Render every turn of a session under ``budget``, separated by blank lines."""
    return "\n\n".join(rendered for turn in activity.turns if (rendered := render_turn(turn, budget=budget)))


def primary_arg(input: Mapping[str, Any]) -> str:
    return str(next((input[key] for key in PRIMARY_KEYS if key in input), next(iter(input.values()), "")))
