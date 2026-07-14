"""The one renderer: every cut the platform makes happens here, under a Budget."""

from __future__ import annotations

import sys
from collections import Counter
from dataclasses import asdict, dataclass
from itertools import islice
from pathlib import Path
from typing import TYPE_CHECKING

import orjson

from cc_transcript.filterspec import event_kind, event_meta, event_text
from cc_transcript.models import (
    AssistantEvent,
    AsyncHookResponse,
    AttachmentEvent,
    FallbackBlock,
    HookAdditionalContext,
    HookBlockingError,
    HookCancelled,
    HookNonBlockingError,
    HookSuccess,
    ModeEvent,
    OtherAttachment,
    OtherBlock,
    OtherEvent,
    QueuedCommand,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.tools import BashCall, EditCall, MultiEditCall, WriteCall, parse_tool_call

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator, Mapping
    from datetime import datetime
    from typing import Any

    from cc_transcript.activity import SessionActivity, ToolUse, Turn
    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.facts import ToolFact
    from cc_transcript.models import AttachmentDetail, ContentBlock, SessionId, ToolUseId, TranscriptEvent
    from cc_transcript.tools import ToolCall

PRIMARY_KEYS = ("file_path", "path", "command", "pattern", "url", "prompt", "query", "description")
SIZE_UNITS = ("B", "KB", "MB", "GB", "TB")
TAGS = {"user": "user", "assistant": "asst", "system": "sys", "mode": "mode", "other": "other", "attachment": "att"}
BLANK_TIME = " " * 8
WHERE_ALL = frozenset({"text", "thinking", "tools"})
UNCLIPPED = sys.maxsize


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


def truncate(text: str, width: int) -> str:
    collapsed = " ".join(text.split())
    return collapsed if width == 0 or len(collapsed) <= width else collapsed[: width - 1] + "…"


def clip(text: str, limit: int) -> str:
    return text if len(text) <= limit else f"{text[:limit]}…(+{len(text) - limit}ch)"


def render_tool_call(call: ToolCall, *, budget: Budget) -> str:
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


def human_size(n: int) -> str:
    return next(
        f"{n / 1024**i:.1f}{unit}" if i else f"{n}{unit}"
        for i, unit in enumerate(SIZE_UNITS)
        if n < 1024 ** (i + 1) or unit == SIZE_UNITS[-1]
    )


def primary_arg(input: Mapping[str, Any]) -> str:
    return str(next((input[key] for key in PRIMARY_KEYS if key in input), next(iter(input.values()), "")))


def display_path(path: Path) -> str:
    return f"~/{path.relative_to(home)}" if path.is_relative_to(home := Path.home()) else str(path)


def transcript_header(path: Path) -> str:
    return f"== {display_path(path)}"


def compact_line(
    index: int,
    event: TranscriptEvent,
    *,
    names: Mapping[ToolUseId, str],
    width: int,
    thinking: bool,
    uuids: bool,
) -> str:
    meta = event_meta(event)
    tag = TAGS[event_kind(event)] + ("*" if meta is not None and meta.is_sidechain else "")
    time = meta.timestamp.strftime("%H:%M:%S") if meta is not None else BLANK_TIME
    line = f"{index:>5} {tag:<5} {time} {event_payload(event, names=names, width=width, thinking=thinking)}".rstrip()
    return f"{line} {meta.uuid}" if uuids and meta is not None else line


def event_payload(event: TranscriptEvent, *, names: Mapping[ToolUseId, str], width: int, thinking: bool) -> str:
    match event:
        case UserEvent():
            return user_payload(event, names=names, width=width)
        case AssistantEvent():
            return assistant_payload(event, width=width, thinking=thinking)
        case SystemEvent(subtype=subtype, content=content):
            return f"{subtype}: {truncate(content, width)}" if content else subtype
        case ModeEvent(channel=channel, value=value):
            return f"{channel}={value}"
        case OtherEvent(type=type_):
            return type_
        case AttachmentEvent(attachment_type=attachment_type, detail=detail):
            return f"{attachment_type} {truncate(attachment_text(detail), width)}".strip()


def attachment_text(detail: AttachmentDetail) -> str:
    match detail:
        case HookSuccess(content=content, stdout=stdout, command=command):
            return content or stdout or command or ""
        case HookNonBlockingError(stderr=stderr, stdout=stdout, command=command):
            return stderr or stdout or command or ""
        case HookBlockingError(blocking_error=blocking_error):
            return orjson.dumps(blocking_error).decode() if blocking_error else ""
        case HookCancelled(command=command):
            return command or ""
        case HookAdditionalContext(content=content):
            return " ".join(content)
        case AsyncHookResponse(stdout=stdout, stderr=stderr):
            return stdout or stderr or ""
        case QueuedCommand(prompt=prompt):
            return prompt or ""
        case OtherAttachment(raw=raw):
            return orjson.dumps(raw).decode()


def user_payload(event: UserEvent, *, names: Mapping[ToolUseId, str], width: int) -> str:
    head = f"[int] {truncate(event.text, width)}".strip() if event.interrupted else truncate(event.text, width)
    results = (
        result_payload(block, names=names, width=width) for block in event.blocks if isinstance(block, ToolResultBlock)
    )
    return " ".join(part for part in (head, *results) if part)


def result_payload(block: ToolResultBlock, *, names: Mapping[ToolUseId, str], width: int) -> str:
    name = names.get(block.tool_use_id, "?")
    err = "[err]" if block.is_error else ""
    return f"<-{name}{err} ({len(block.content)}ch) {truncate(block.content, width)}".rstrip()


def assistant_payload(event: AssistantEvent, *, width: int, thinking: bool) -> str:
    rendered = (block_payload(block, width=width, thinking=thinking) for block in event.blocks)
    return " ".join(part for part in (f"[{event.model}]", *rendered) if part)


def block_payload(block: ContentBlock, *, width: int, thinking: bool) -> str:
    match block:
        case TextBlock(text=text):
            return f'"{truncate(text, width)}"' if text.strip() else ""
        case ThinkingBlock(thinking=thought):
            return f"th({len(thought)}ch) {truncate(thought, width)}" if thinking else f"th({len(thought)}ch)"
        case ToolUseBlock(name=name, input=input):
            return " ".join(
                render_tool_call(parse_tool_call(name, input, on_error="other"), budget=line_budget(width)).split()
            )
        case ToolResultBlock():
            return ""
        case FallbackBlock(from_model=src, to_model=dst):
            return f"fallback {src}->{dst}"
        case OtherBlock(type=type_):
            return type_


def line_budget(width: int) -> Budget:
    return Budget(turn_chars=width or UNCLIPPED, tool_chars=width or UNCLIPPED)


def event_dict(index: int, event: TranscriptEvent) -> dict[str, Any]:
    return {"i": index, "kind": event_kind(event)} | view_asdict(event)


def view_asdict(view: object) -> dict[str, Any]:
    return {name: view_field(getattr(view, name)) for name in type(view).__match_args__}


def view_field(value: object) -> Any:
    match value:
        case tuple():
            return tuple(view_field(item) for item in value)
        case dict():
            return value
        case _ if type(value).__module__ == "cc_transcript.models":
            return view_asdict(value)
        case _:
            return value


def haystack(event: TranscriptEvent, *, where: frozenset[str]) -> str:
    match event:
        case UserEvent(blocks=blocks) | AssistantEvent(blocks=blocks):
            parts = (
                *((event_text(event),) if "text" in where else ()),
                *(
                    (block.thinking for block in blocks if isinstance(block, ThinkingBlock))
                    if "thinking" in where
                    else ()
                ),
                *(
                    (tool_haystack(block) for block in blocks if isinstance(block, ToolUseBlock | ToolResultBlock))
                    if "tools" in where
                    else ()
                ),
            )
            return "\n".join(part for part in parts if part)
        case SystemEvent(subtype=subtype, content=content) if "text" in where:
            return f"{subtype}: {content}" if content else subtype
        case ModeEvent(channel=channel, value=value) if "text" in where:
            return f"{channel}={value}"
        case OtherEvent(type=type_) if "text" in where:
            return type_
        case AttachmentEvent(attachment_type=attachment_type, detail=detail) if "text" in where:
            return f"{attachment_type} {attachment_text(detail)}".strip()
        case _:
            return ""


def tool_haystack(block: ToolUseBlock | ToolResultBlock) -> str:
    match block:
        case ToolUseBlock(name=name, input=input):
            return f"{name} {orjson.dumps(input).decode()}"
        case ToolResultBlock(content=content):
            return content


@dataclass(frozen=True, slots=True)
class Stats:
    files: int
    events: int
    kinds: Mapping[str, int]
    models: Mapping[str, int]
    tools: Mapping[str, int]
    text_chars: int
    thinking_chars: int
    tool_io_chars: int
    sessions: int
    first_timestamp: datetime | None
    last_timestamp: datetime | None
    interrupts: int
    tool_errors: int
    sidechain: int


def collect_stats(transcripts: Iterable[ParsedTranscript]) -> Stats:
    kinds: Counter[str] = Counter()
    models: Counter[str] = Counter()
    tools: Counter[str] = Counter()
    sessions: set[SessionId] = set()
    files = events = text_chars = thinking_chars = tool_io_chars = interrupts = tool_errors = sidechain = 0
    first: datetime | None = None
    last: datetime | None = None
    for parsed in transcripts:
        files += 1
        for event in parsed.events:
            events += 1
            kinds[event_kind(event)] += 1
            text_chars += len(event_text(event))
            if (meta := event_meta(event)) is not None:
                sessions.add(meta.session_id)
                sidechain += meta.is_sidechain
                first = meta.timestamp if first is None or meta.timestamp < first else first
                last = meta.timestamp if last is None or meta.timestamp > last else last
            match event:
                case UserEvent(blocks=blocks, interrupted=interrupted):
                    interrupts += interrupted
                    for block in blocks:
                        if isinstance(block, ToolResultBlock):
                            tool_io_chars += len(block.content)
                            tool_errors += block.is_error
                case AssistantEvent(model=model, blocks=blocks):
                    models[model] += 1
                    for block in blocks:
                        match block:
                            case ThinkingBlock(thinking=thought):
                                thinking_chars += len(thought)
                            case ToolUseBlock(name=name, input=input):
                                tools[name] += 1
                                tool_io_chars += len(orjson.dumps(input))
                            case _:
                                pass
                case ModeEvent(session_id=session_id):
                    sessions.add(session_id)
                case _:
                    pass
    return Stats(
        files=files,
        events=events,
        kinds=dict(kinds.most_common()),
        models=dict(models.most_common()),
        tools=dict(tools.most_common()),
        text_chars=text_chars,
        thinking_chars=thinking_chars,
        tool_io_chars=tool_io_chars,
        sessions=len(sessions),
        first_timestamp=first,
        last_timestamp=last,
        interrupts=interrupts,
        tool_errors=tool_errors,
        sidechain=sidechain,
    )


def render_stats(stats: Stats) -> str:
    return "\n".join(
        f"{label:<12} {value}"
        for label, value in (
            ("files", stats.files),
            ("events", stats.events),
            ("kinds", render_histogram(stats.kinds)),
            ("models", render_histogram(stats.models)),
            ("tools", render_histogram(stats.tools)),
            ("text", human_size(stats.text_chars)),
            ("thinking", human_size(stats.thinking_chars)),
            ("tool io", human_size(stats.tool_io_chars)),
            ("sessions", stats.sessions),
            ("span", render_span(stats.first_timestamp, stats.last_timestamp)),
            ("interrupts", stats.interrupts),
            ("tool errors", stats.tool_errors),
            ("sidechain", stats.sidechain),
        )
    )


def render_span(first: datetime | None, last: datetime | None) -> str:
    return f"{first:%Y-%m-%d %H:%M:%S} → {last:%Y-%m-%d %H:%M:%S}" if first is not None and last is not None else "-"


def render_histogram(counts: Mapping[str, int]) -> str:
    return " · ".join(f"{name} {count}" for name, count in counts.items()) or "-"


def stats_dict(stats: Stats) -> dict[str, Any]:
    return asdict(stats)


def render_counts(counts: Mapping[str, int]) -> list[str]:
    width = len(str(max(counts.values(), default=0)))
    return [
        f"  {count:>{width}}  {name}" for name, count in sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    ]


def render_mcp(summary: Mapping[str, Mapping[str, int | dict[str, int]]]) -> list[str]:
    width = max((len(server) for server in summary), default=0)
    lines: list[str] = []
    for server, data in summary.items():
        match data:
            case {"read": int(read), "write": int(write), "total": int(total), "tools": dict(tools)}:
                lines.append(
                    f"{server:<{width}}  read {read} · write {write} · total {total}  "
                    f"{render_histogram(dict(islice(tools.items(), 5)))}"
                )
    return lines


def fact_dict(fact: ToolFact) -> dict[str, Any]:
    return {
        "ts": fact.ts,
        "session_id": fact.session_id,
        "path": str(fact.path),
        "tool_use_id": fact.tool_use_id,
        "tool": fact.tool,
        "command_prefixes": list(fact.command_prefixes),
        "command": fact.command,
        "mcp_server": fact.mcp_server,
        "mcp_tool": fact.mcp_tool,
        "mcp_access": fact.mcp_access,
        "file_path": fact.file_path,
        "is_error": fact.is_error,
        "denied": fact.denied,
        "denial_kind": fact.denial_kind,
        "user_said": fact.user_said,
        "duration_ms": fact.duration_ms,
    }


def fact_line(fact: ToolFact) -> str:
    ts = f"{fact.ts:%Y-%m-%d %H:%M:%S}" if fact.ts is not None else "-"
    name = f"{fact.mcp_server}/{fact.mcp_tool}" if fact.mcp_server is not None else fact.tool
    prefixes = f" {','.join(fact.command_prefixes)}" if fact.command_prefixes else ""
    marker = " [denied]" if fact.denied else " [err]" if fact.is_error else ""
    return f"{ts} {fact.session_id[:8]} {name}{prefixes}{marker}"


def denial_dict(fact: ToolFact) -> dict[str, Any]:
    return {
        "ts": fact.ts,
        "session": fact.session_id,
        "path": str(fact.path),
        "tool": fact.tool,
        "command": fact.command,
        "file_path": fact.file_path,
        "denial_kind": fact.denial_kind,
        "user_said": fact.user_said,
    }


def denial_line(fact: ToolFact) -> str:
    head = f"{fact.tool} {target}" if (target := fact.command or fact.file_path) else fact.tool
    return f"{head} → {fact.user_said}" if fact.user_said else head
