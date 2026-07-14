"""The cc-transcript investigation CLI: list, show, grep, and stats over Claude Code transcripts."""

from __future__ import annotations

import os
import re
import sys
import time
from datetime import datetime
from functools import partial, reduce
from itertools import chain, islice
from pathlib import Path
from typing import TYPE_CHECKING, NamedTuple, get_args

import anyio
import click
import orjson

from cc_transcript.builders import (
    NOISE_SPEC,
    build_spec,
    drop_empty,
    drop_junk,
    drop_sidechain,
    drop_synthetic,
    keep_only,
)
from cc_transcript.corrections_cli import corrections
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, TranscriptDiscovery, find_transcript_sync
from cc_transcript.facts import command_prefix_counts, mcp_summary, tool_facts
from cc_transcript.filterspec import ASSISTANTS, USERS, EventKind, event_kind, event_meta, keep, tool_names
from cc_transcript.ids import SessionId, tool_digest
from cc_transcript.models import AssistantEvent, ToolResultBlock, ToolUseBlock, UserEvent
from cc_transcript.parser import TranscriptParser
from cc_transcript.render import (
    BLANK_TIME,
    TAGS,
    WHERE_ALL,
    Budget,
    collect_stats,
    compact_line,
    denial_dict,
    denial_line,
    display_path,
    event_dict,
    event_payload,
    fact_dict,
    fact_line,
    haystack,
    human_size,
    render_counts,
    render_mcp,
    render_stats,
    render_tool_call,
    stats_dict,
    transcript_header,
    truncate,
)
from cc_transcript.tools import file_path_of, parse_tool_call, tool_name_matches
from cc_transcript.watch import Watcher

if TYPE_CHECKING:
    from collections.abc import Iterable, Mapping, Sequence
    from typing import Any

    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.facts import ToolFact
    from cc_transcript.filterspec import FilterSpec
    from cc_transcript.models import EntryMeta, ToolUseId, TranscriptEvent
    from cc_transcript.watch import WatchEvent

    type Row = tuple[int, TranscriptEvent]

KINDS = get_args(EventKind)
SHOW_CAP = 200
SLICE_SCHEMA = "cc-transcript.slice/1"
SIGNAL_SPEC = build_spec(
    keep_only("user", "assistant"),
    drop_junk("structural", "agent_injection", "command_echo"),
    drop_synthetic(),
    drop_sidechain(),
    drop_empty(only_from=USERS),
    drop_empty(only_from=ASSISTANTS),
)
DISCOVERY_OPTIONS = (
    click.option(
        "--root",
        type=click.Path(file_okay=False, path_type=Path),
        default=CLAUDE_PROJECTS_DIR,
        help="Projects directory to search [default: ~/.claude/projects].",
    ),
    click.option("--project", help="Substring filter over project directory names."),
    click.option("--contains", help="Substring filter over transcript file names."),
    click.option("--limit", default=50, show_default=True, help="Keep only the newest N transcripts."),
    click.option("--all", "all_", is_flag=True, help="Ignore --limit."),
)


def discovery_options[C](command: C) -> C:
    return reduce(lambda decorated, option: option(decorated), reversed(DISCOVERY_OPTIONS), command)


def emit(lines: Iterable[str | bytes]) -> None:
    try:
        for line in lines:
            click.echo(line)
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        raise SystemExit(0) from None


def parse_transcripts(targets: Sequence[tuple[Path, float]]) -> list[ParsedTranscript]:
    async def collect() -> list[ParsedTranscript]:
        return [parsed async for parsed in TranscriptParser.stream_transcripts(targets)]

    by_path = {parsed.path: parsed for parsed in anyio.run(collect)}
    if missing := [path for path, _ in targets if path not in by_path]:
        click.echo(
            f"warning: skipped {len(missing)} unparseable transcript(s): "
            f"{', '.join(display_path(path) for path in missing)}",
            err=True,
        )
    return [by_path[path] for path, _ in targets if path in by_path]


def parse_single(path: Path) -> tuple[TranscriptEvent, ...]:
    parsed = parse_transcripts([(path, path.stat().st_mtime)])
    if not parsed:
        raise click.ClickException(f"failed to parse {display_path(path)}")
    return parsed[0].events


def discover(root: Path, *, project: str | None, contains: str | None) -> list[tuple[Path, float]]:
    found = anyio.run(partial(TranscriptDiscovery.find_in, root, name_contains=contains))
    return sorted(
        (pair for pair in found if project_matches(pair[0], root, project)),
        key=lambda pair: pair[1],
        reverse=True,
    )


def project_matches(path: Path, root: Path, project: str | None) -> bool:
    return project is None or any(project in part for part in path.relative_to(root).parts[:-1])


class Targets(NamedTuple):
    paths: list[tuple[Path, float]]
    total: int


def resolve_targets(
    paths: Sequence[Path], *, root: Path, project: str | None, contains: str | None, limit: int | None
) -> Targets:
    if paths:
        return Targets([(path, path.stat().st_mtime) for path in paths], len(paths))
    matched = discover(root, project=project, contains=contains)
    return Targets(matched if limit is None else matched[:limit], len(matched))


def scope_note(targets: Targets) -> str:
    return (
        f"searched {len(targets.paths)} of {targets.total} transcripts — use --all"
        if len(targets.paths) < targets.total
        else ""
    )


def filter_rows(rows: list[Row], *, kinds: frozenset[str], spec: FilterSpec | None) -> list[Row]:
    return [
        (index, event)
        for index, event in rows
        if not kinds or event_kind(event) in kinds
        if spec is None or keep(event, spec)
    ]


def parse_bounds(value: str | None) -> tuple[int | None, int | None] | None:
    if value is None:
        return None
    match value.split(":"):
        case [a, b]:
            try:
                return (int(a) if a else None, int(b) if b else None)
            except ValueError as error:
                raise click.UsageError(f"invalid --range {value!r}; expected A:B") from error
        case _:
            raise click.UsageError(f"invalid --range {value!r}; expected A:B")


def slice_rows(
    rows: list[Row], *, head: int | None, tail: int | None, bounds: tuple[int | None, int | None] | None, all_: bool
) -> tuple[list[Row], str | None]:
    match (head, tail, bounds):
        case (int(n), _, _):
            return rows[:n], None
        case (_, int(n), _):
            return (rows[-n:] if n else []), None
        case (_, _, (lo, hi)):
            return [(i, event) for i, event in rows if (lo is None or i >= lo) and (hi is None or i < hi)], None
        case _ if all_ or len(rows) <= SHOW_CAP:
            return rows, None
        case _:
            return rows[-SHOW_CAP:], f"… {len(rows) - SHOW_CAP} earlier events hidden — use --head/--range/--all"


def uses_tool(event: TranscriptEvent, tool: str, names: Mapping[ToolUseId, str]) -> bool:
    match event:
        case AssistantEvent(blocks=blocks):
            return any(isinstance(block, ToolUseBlock) and tool_name_matches(block.name, tool) for block in blocks)
        case UserEvent(blocks=blocks):
            return any(
                isinstance(block, ToolResultBlock)
                and (name := names.get(block.tool_use_id)) is not None
                and tool_name_matches(name, tool)
                for block in blocks
            )
        case _:
            return False


def event_facts(event: TranscriptEvent, facts: Mapping[ToolUseId, ToolFact]) -> list[ToolFact]:
    match event:
        case AssistantEvent(blocks=blocks):
            return [fact for block in blocks if isinstance(block, ToolUseBlock) and (fact := facts.get(block.id))]
        case _:
            return []


def result_key(fact: ToolFact) -> dict[str, Any]:
    return {"is_error": fact.is_error, "denied": fact.denied, "duration_ms": fact.duration_ms}


def outcome_marker(fact: ToolFact) -> str:
    status = "[denied]" if fact.denied else "[err]" if fact.is_error else ""
    dur = f"({fact.duration_ms}ms)" if fact.duration_ms is not None else ""
    return " ".join(part for part in (status, dur) if part)


def result_suffix(event: TranscriptEvent, facts: Mapping[ToolUseId, ToolFact]) -> str:
    markers = [marker for fact in event_facts(event, facts) if (marker := outcome_marker(fact))]
    return f" {' '.join(markers)}" if markers else ""


def merge_windows(hits: list[int], *, context: int, size: int) -> list[tuple[int, int]]:
    merged: list[tuple[int, int]] = []
    for lo, hi in ((max(0, i - context), min(size, i + context + 1)) for i in hits):
        match merged:
            case [*_, (last_lo, last_hi)] if lo <= last_hi:
                merged[-1] = (last_lo, max(last_hi, hi))
            case _:
                merged.append((lo, hi))
    return merged


def compile_pattern(pattern: str, ignore_case: bool) -> re.Pattern[str]:
    try:
        return re.compile(pattern, re.IGNORECASE if ignore_case else 0)
    except re.error as error:
        raise click.UsageError(f"invalid pattern: {error}") from error


def parse_rfc3339(option: str, value: str) -> datetime:
    try:
        stamp = datetime.fromisoformat(value)
    except ValueError as error:
        raise click.UsageError(f"invalid {option} {value!r}; expected an RFC 3339 timestamp") from error
    if stamp.tzinfo is None:
        raise click.UsageError(f"invalid {option} {value!r}; RFC 3339 requires a UTC offset")
    return stamp


def ts_ms_of(stamp: datetime) -> int:
    return round(stamp.timestamp() * 1000)


def slice_line(meta: EntryMeta, block: ToolUseBlock) -> dict[str, Any]:
    call = parse_tool_call(block.name, block.input, on_error="other")
    return {
        "schema": SLICE_SCHEMA,
        "event_uuid": meta.uuid,
        "tool_use_id": block.id,
        "ts_ms": ts_ms_of(meta.timestamp),
        "tool_name": block.name,
        "tool_digest": block.digest,
        "file_path": file_path_of(call),
        "summary": render_tool_call(call, budget=Budget()),
    }



def watch_dict(item: WatchEvent) -> dict[str, Any]:
    meta = event_meta(item.event)
    kind = event_kind(item.event)
    return {
        "path": str(item.path),
        "session_id": item.session_id,
        "is_sidechain": item.is_sidechain,
        "uuid": meta.uuid if meta is not None else None,
        "kind": kind,
        "role": kind if kind in ("user", "assistant") else None,
        "preview": truncate(event_payload(item.event, names={}, width=120, thinking=False), 120),
    }


def watch_line(item: WatchEvent) -> str:
    meta = event_meta(item.event)
    time = meta.timestamp.strftime("%H:%M:%S") if meta is not None else BLANK_TIME
    tag = TAGS[event_kind(item.event)] + ("*" if item.is_sidechain else "")
    payload = event_payload(item.event, names={}, width=100, thinking=False)
    return f"{time} {item.session_id[:8]} {tag:<5} {payload}".rstrip()


@click.group()
@click.version_option(package_name="cc-transcript")
def cli() -> None:
    """Investigate Claude Code transcripts: list, show, grep, and stats."""


cli.add_command(corrections)


@cli.command("list")
@discovery_options
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per transcript.")
def list_(root: Path, project: str | None, contains: str | None, limit: int, all_: bool, as_json: bool) -> None:
    """List discovered transcripts, newest first."""
    matched = discover(root, project=project, contains=contains)
    shown = matched if all_ else matched[:limit]
    if as_json:
        emit(orjson.dumps({"path": str(path), "mtime": mtime, "size": path.stat().st_size}) for path, mtime in shown)
        return
    count = str(len(matched)) if len(shown) == len(matched) else f"{len(shown)} of {len(matched)}"
    emit(
        chain(
            (
                f"{datetime.fromtimestamp(mtime):%Y-%m-%d %H:%M} {human_size(path.stat().st_size):>8} "
                f"{display_path(path)}"
                for path, mtime in shown
            ),
            (f"{count} transcripts under {display_path(root)}",),
        )
    )


@cli.command()
@click.argument("path", type=click.Path(exists=True, dir_okay=False, path_type=Path))
@click.option("--head", type=click.IntRange(min=0), help="Show only the first N matching events.")
@click.option("--tail", type=click.IntRange(min=0), help="Show only the last N matching events.")
@click.option("--range", "range_", help="Show raw-index range A:B (half-open; A: and :B work).")
@click.option("--all", "all_", is_flag=True, help="Disable the default 200-event cap.")
@click.option("--kind", "kinds", multiple=True, type=click.Choice(KINDS), help="Keep only these event kinds.")
@click.option("--signal", is_flag=True, help="Keep only substantive user/assistant turns.")
@click.option("--no-junk", is_flag=True, help="Drop structural junk events.")
@click.option("--thinking", is_flag=True, help="Render thinking text inline.")
@click.option("--width", default=100, show_default=True, help="Truncation width per chunk (0 = no cut).")
@click.option("--uuids", is_flag=True, help="Append each event's uuid.")
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per event.")
def show(
    path: Path,
    head: int | None,
    tail: int | None,
    range_: str | None,
    all_: bool,
    kinds: tuple[str, ...],
    signal: bool,
    no_junk: bool,
    thinking: bool,
    width: int,
    uuids: bool,
    as_json: bool,
) -> None:
    """Show a transcript's events, one compact line per event."""
    if sum(slicer is not None for slicer in (head, tail, range_)) > 1:
        raise click.UsageError("--head, --tail, and --range are mutually exclusive")
    events = parse_single(path)
    spec = SIGNAL_SPEC if signal else NOISE_SPEC if no_junk else None
    rows = filter_rows(list(enumerate(events)), kinds=frozenset(kinds), spec=spec)
    selected, notice = slice_rows(rows, head=head, tail=tail, bounds=parse_bounds(range_), all_=all_)
    if as_json:
        if notice:
            click.echo(notice, err=True)
        emit(orjson.dumps(event_dict(index, event)) for index, event in selected)
        return
    names = tool_names(events)
    emit(
        chain(
            (notice,) if notice else (),
            (
                compact_line(index, event, names=names, width=width, thinking=thinking, uuids=uuids)
                for index, event in selected
            ),
        )
    )


@cli.command()
@click.argument("pattern")
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--kind", "kinds", multiple=True, type=click.Choice(KINDS), help="Keep only these event kinds.")
@click.option("--tool", help="Keep only events using this tool, or carrying its results.")
@click.option("-i", "--ignore-case", is_flag=True, help="Case-insensitive matching.")
@click.option(
    "--where",
    "wheres",
    multiple=True,
    type=click.Choice(sorted(WHERE_ALL)),
    help="Search only these areas [default: all].",
)
@click.option(
    "-C",
    "--context",
    type=click.IntRange(min=0),
    default=0,
    show_default=True,
    help="Events of context around each hit.",
)
@click.option(
    "--max-matches", type=click.IntRange(min=0), default=20, show_default=True, help="Stop after this many matches."
)
@click.option("--width", default=100, show_default=True, help="Truncation width per chunk (0 = no cut).")
@click.option("--uuids", is_flag=True, help="Append each event's uuid.")
@click.option("--with-result", is_flag=True, help="Annotate tool-use hits with their result's outcome.")
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per match.")
def grep(
    pattern: str,
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    kinds: tuple[str, ...],
    tool: str | None,
    ignore_case: bool,
    wheres: tuple[str, ...],
    context: int,
    max_matches: int,
    width: int,
    uuids: bool,
    with_result: bool,
    as_json: bool,
) -> None:
    """Search transcript events for a regex pattern."""
    regex = compile_pattern(pattern, ignore_case)
    where = frozenset(wheres) or WHERE_ALL
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    out: list[str | bytes] = []
    files_matched = matched = 0
    budget = max_matches
    for parsed in parse_transcripts(targets.paths):
        if budget == 0:
            break
        names = tool_names(parsed.events)
        hits = list(
            islice(
                (
                    index
                    for index, event in enumerate(parsed.events)
                    if not kinds or event_kind(event) in kinds
                    if tool is None or uses_tool(event, tool, names)
                    if regex.search(haystack(event, where=where))
                ),
                budget,
            )
        )
        if not hits:
            continue
        facts = {fact.tool_use_id: fact for fact in tool_facts([parsed])} if with_result else {}
        files_matched += 1
        matched += len(hits)
        budget -= len(hits)
        if as_json:
            hit_set = set(hits)
            out.extend(
                orjson.dumps(
                    {"path": str(parsed.path)}
                    | event_dict(i, parsed.events[i])
                    | ({} if i in hit_set else {"context": True})
                    | (
                        {"results": rk}
                        if with_result
                        and (rk := {fact.tool_use_id: result_key(fact) for fact in event_facts(parsed.events[i], facts)})
                        else {}
                    )
                )
                for lo, hi in merge_windows(hits, context=context, size=len(parsed.events))
                for i in range(lo, hi)
            )
            continue
        out.append(transcript_header(parsed.path))
        for n, (lo, hi) in enumerate(merge_windows(hits, context=context, size=len(parsed.events))):
            if context and n:
                out.append("--")
            out.extend(
                compact_line(i, parsed.events[i], names=names, width=width, thinking=False, uuids=uuids)
                + (result_suffix(parsed.events[i], facts) if with_result else "")
                for i in range(lo, hi)
            )
    if not as_json:
        out.append(
            f"{files_matched} files, {matched} matches" + (f" · {note}" if (note := scope_note(targets)) else "")
        )
    emit(out)
    if matched == 0:
        raise SystemExit(1)


@cli.command()
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--per-file", is_flag=True, help="One stats block per transcript.")
@click.option("--json", "as_json", is_flag=True, help="Emit stats as JSON.")
def stats(
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    per_file: bool,
    as_json: bool,
) -> None:
    """Summarize event, model, and tool statistics."""
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    transcripts = parse_transcripts(targets.paths)
    if per_file and as_json:
        emit(orjson.dumps({"path": str(parsed.path)} | stats_dict(collect_stats([parsed]))) for parsed in transcripts)
    elif per_file:
        emit(
            chain(
                (
                    block
                    for parsed in transcripts
                    for block in (transcript_header(parsed.path), render_stats(collect_stats([parsed])), "")
                ),
                (note,) if (note := scope_note(targets)) else (),
            )
        )
    elif as_json:
        emit((orjson.dumps(stats_dict(collect_stats(transcripts))),))
    else:
        emit((render_stats(collect_stats(transcripts)), *((note,) if (note := scope_note(targets)) else ())))


@cli.command()
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--tool", help="Keep only calls to this tool.")
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per tool call.")
def tools(
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    tool: str | None,
    as_json: bool,
) -> None:
    """List every tool call across the matched transcripts, one compact line each."""
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    facts = [
        fact
        for fact in tool_facts(parse_transcripts(targets.paths))
        if tool is None or tool_name_matches(fact.tool, tool)
    ]
    if as_json:
        emit(orjson.dumps(fact_dict(fact)) for fact in facts)
        return
    emit(chain((fact_line(fact) for fact in facts), (note,) if (note := scope_note(targets)) else ()))


@cli.command()
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per prefix.")
def commands(
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    as_json: bool,
) -> None:
    """Tally Bash command prefixes across the matched transcripts, most frequent first."""
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    counts = command_prefix_counts(tool_facts(parse_transcripts(targets.paths)))
    if as_json:
        emit(orjson.dumps({"prefix": prefix, "count": count}) for prefix, count in counts.items())
        return
    emit(chain(render_counts(counts), (note,) if (note := scope_note(targets)) else ()))


@cli.command()
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per denial.")
def permissions(
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    as_json: bool,
) -> None:
    """List tool uses the user denied, with the instruction they gave instead."""
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    facts = [
        fact
        for fact in tool_facts(parse_transcripts(targets.paths))
        if fact.denied
        if not tool_name_matches(fact.tool, "ExitPlanMode|AskUserQuestion")
    ]
    if as_json:
        emit(orjson.dumps(denial_dict(fact)) for fact in facts)
        return
    emit(chain((denial_line(fact) for fact in facts), (note,) if (note := scope_note(targets)) else ()))


@cli.command()
@click.argument("paths", nargs=-1, type=click.Path(exists=True, dir_okay=False, path_type=Path))
@discovery_options
@click.option("--json", "as_json", is_flag=True, help="Emit one JSON object per server.")
def mcp(
    paths: tuple[Path, ...],
    root: Path,
    project: str | None,
    contains: str | None,
    limit: int,
    all_: bool,
    as_json: bool,
) -> None:
    """Summarize MCP server and tool usage across the matched transcripts."""
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    summary = mcp_summary(tool_facts(parse_transcripts(targets.paths)))
    if as_json:
        emit(orjson.dumps({"server": server} | data) for server, data in summary.items())
        return
    emit(chain(render_mcp(summary), (note,) if (note := scope_note(targets)) else ()))


@cli.command("slice")
@click.option("--session", required=True, help="Claude session UUID.")
@click.option("--since", required=True, help="Window start, RFC 3339 (inclusive).")
@click.option("--until", required=True, help="Window end, RFC 3339 (exclusive).")
@click.option(
    "--root",
    type=click.Path(file_okay=False, path_type=Path),
    default=CLAUDE_PROJECTS_DIR,
    help="Projects directory to search [default: ~/.claude/projects].",
)
def slice_(session: str, since: str, until: str, root: Path) -> None:
    """Emit a session window's tool calls, one cc-transcript.slice/1 JSON line each."""
    start, end = parse_rfc3339("--since", since), parse_rfc3339("--until", until)
    if (path := find_transcript_sync(SessionId(session), root=root)) is None:
        raise SystemExit(1)
    if not (parsed := parse_transcripts([(path, path.stat().st_mtime)])):
        raise SystemExit(2)
    emit(
        orjson.dumps(slice_line(event.meta, block))
        for event in parsed[0].events
        if isinstance(event, AssistantEvent) and start <= event.meta.timestamp < end
        for block in event.blocks
        if isinstance(block, ToolUseBlock)
    )


@cli.command()
@click.option(
    "--check",
    type=click.Path(exists=True, dir_okay=False, path_type=Path),
    help="Verify an existing fixture file instead of generating.",
)
def digest(check: Path | None) -> None:
    """Generate the tool-digest fixture corpus from stdin, or verify one with --check."""
    if check is not None:
        mismatched = [
            (row, actual)
            for row in orjson.loads(check.read_bytes())
            if (actual := tool_digest(row["tool"], row["input"])) != row["digest"]
        ]
        for row, actual in mismatched:
            click.echo(f"mismatch: {row['tool']} expected {row['digest']}, computed {actual}", err=True)
        if mismatched:
            raise SystemExit(1)
        return
    try:
        rows = orjson.loads(click.get_binary_stream("stdin").read())
    except orjson.JSONDecodeError as error:
        raise click.UsageError(f"invalid JSON on stdin: {error}") from error
    emit(
        (
            orjson.dumps(
                [
                    {"tool": row["tool"], "input": row["input"], "digest": tool_digest(row["tool"], row["input"])}
                    for row in rows
                ],
                option=orjson.OPT_INDENT_2,
            ),
        )
    )
@cli.command("watch")
@click.option(
    "--root",
    "roots",
    multiple=True,
    type=click.Path(file_okay=False, path_type=Path),
    help="Projects directory to tail; repeatable [default: ~/.claude/projects].",
)
@click.option("--poll", default=1.0, show_default=True, help="Seconds between filesystem polls.")
@click.option("--from-start", is_flag=True, help="Replay preexisting transcript content instead of tailing from EOF.")
@click.option("--json", "as_json", is_flag=True, help="Emit one NDJSON object per event.")
def watch_(roots: tuple[Path, ...], poll: float, from_start: bool, as_json: bool) -> None:
    """Tail transcripts live, one line per newly appended event, until interrupted."""
    watcher = Watcher(roots or (CLAUDE_PROJECTS_DIR,), from_start=from_start)
    try:
        while True:
            for item in watcher.tick():
                click.echo(orjson.dumps(watch_dict(item)) if as_json else watch_line(item))
            time.sleep(poll)
    except KeyboardInterrupt:
        return
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        raise SystemExit(0) from None
