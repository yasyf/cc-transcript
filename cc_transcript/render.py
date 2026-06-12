from __future__ import annotations

from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import TYPE_CHECKING

import orjson

from cc_transcript.filterspec import event_kind, event_meta, event_text, tool_uses
from cc_transcript.models import (
    AssistantEvent,
    ModeEvent,
    OtherEvent,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)

if TYPE_CHECKING:
    from collections.abc import Iterable, Mapping, Sequence
    from datetime import datetime
    from typing import Any

    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.models import ContentBlock, SessionId, ToolUseId, TranscriptEvent

PRIMARY_KEYS = ("file_path", "path", "command", "pattern", "url", "prompt", "query", "description")
SIZE_UNITS = ("B", "KB", "MB", "GB", "TB")
TAGS = {"user": "user", "assistant": "asst", "system": "sys", "mode": "mode", "other": "other"}
BLANK_TIME = " " * 8
WHERE_ALL = frozenset({"text", "thinking", "tools"})


def truncate(text: str, width: int) -> str:
    collapsed = " ".join(text.split())
    return collapsed if width == 0 or len(collapsed) <= width else collapsed[: width - 1] + "…"


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


def tool_names(events: Sequence[TranscriptEvent]) -> dict[ToolUseId, str]:
    return {tid: block.name for tid, block in tool_uses(events).items()}


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
            return f"{name}({truncate(primary_arg(input), width)})"
        case ToolResultBlock():
            return ""


def event_dict(index: int, event: TranscriptEvent) -> dict[str, Any]:
    return {"i": index, "kind": event_kind(event)} | asdict(event)


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
