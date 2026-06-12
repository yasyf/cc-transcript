"""The cc-transcript investigation CLI: list, show, grep, and stats over Claude Code transcripts."""

from __future__ import annotations

import os
import re
import sys
from datetime import datetime
from functools import partial, reduce
from itertools import chain, islice
from pathlib import Path
from typing import TYPE_CHECKING, get_args

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
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, TranscriptDiscovery
from cc_transcript.filterspec import ASSISTANTS, USERS, EventKind, event_kind, keep
from cc_transcript.models import AssistantEvent, ToolResultBlock, ToolUseBlock, UserEvent
from cc_transcript.parser import TranscriptParser
from cc_transcript.render import (
    WHERE_ALL,
    collect_stats,
    compact_line,
    display_path,
    event_dict,
    haystack,
    human_size,
    render_stats,
    stats_dict,
    tool_names,
    transcript_header,
)

if TYPE_CHECKING:
    from collections.abc import Iterable, Mapping, Sequence

    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.filterspec import FilterSpec
    from cc_transcript.models import ToolUseId, TranscriptEvent

    type Row = tuple[int, TranscriptEvent]

KINDS = get_args(EventKind)
SHOW_CAP = 200
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


def resolve_targets(
    paths: Sequence[Path], *, root: Path, project: str | None, contains: str | None, limit: int | None
) -> list[tuple[Path, float]]:
    if paths:
        return [(path, path.stat().st_mtime) for path in paths]
    matched = discover(root, project=project, contains=contains)
    return matched if limit is None else matched[:limit]


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
            return any(isinstance(block, ToolUseBlock) and block.name == tool for block in blocks)
        case UserEvent(blocks=blocks):
            return any(isinstance(block, ToolResultBlock) and names.get(block.tool_use_id) == tool for block in blocks)
        case _:
            return False


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


@click.group()
@click.version_option(package_name="cc-transcript")
def cli() -> None:
    """Investigate Claude Code transcripts: list, show, grep, and stats."""


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
    as_json: bool,
) -> None:
    """Search transcript events for a regex pattern."""
    regex = compile_pattern(pattern, ignore_case)
    where = frozenset(wheres) or WHERE_ALL
    targets = resolve_targets(paths, root=root, project=project, contains=contains, limit=None if all_ else limit)
    out: list[str | bytes] = []
    files_matched = matched = 0
    budget = max_matches
    for parsed in parse_transcripts(targets):
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
                for i in range(lo, hi)
            )
    if not as_json:
        out.append(f"{files_matched} files, {matched} matches")
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
    transcripts = parse_transcripts(targets)
    if per_file and as_json:
        emit(
            orjson.dumps({"path": str(parsed.path)} | stats_dict(collect_stats([parsed]))) for parsed in transcripts
        )
    elif per_file:
        emit(
            block
            for parsed in transcripts
            for block in (transcript_header(parsed.path), render_stats(collect_stats([parsed])), "")
        )
    elif as_json:
        emit((orjson.dumps(stats_dict(collect_stats(transcripts))),))
    else:
        emit((render_stats(collect_stats(transcripts)),))
