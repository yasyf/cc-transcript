"""The single tool-call analytics substrate, rehydrated from the native core.

A pure projection of a parsed transcript's tool activity: :func:`tool_facts`
runs the native tool-fact aggregator over transcript paths and rehydrates every
tool call into a flat :class:`ToolFact` — command prefixes, MCP server/tool/access,
file path, error and denial state, and duration — and the aggregators
(:func:`command_prefix_counts`, :func:`mcp_summary`) roll those facts up. The
projection runs entirely in the native core, so the CLI's stats surface and any
downstream analytics share one substrate.
"""

from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript.ids import SessionId, ToolUseId

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator, Mapping, Sequence
    from typing import Any, Literal


@dataclass(frozen=True, slots=True)
class ToolFact:
    """One tool call flattened for analytics, lifted from a parsed transcript.

    Attributes:
        ts: The timestamp of the assistant entry that made the call, or None.
        session_id: The Claude session the call belongs to.
        path: The transcript file the call was lifted from.
        cwd: The working directory recorded on the originating event, or None.
        tool_use_id: The tool-use block id — the join key back to the event stream.
        tool: The tool name exactly as invoked.
        command_prefixes: The permission-style prefixes of each Bash command
            segment; ``()`` for non-Bash calls.
        command: The shell command for a Bash call, else None.
        mcp_server: The MCP server segment for an ``mcp__server__tool`` call, else None.
        mcp_tool: The MCP tool segment for an ``mcp__server__tool`` call, else None.
        mcp_access: Whether the MCP tool reads or writes, else None.
        file_path: The file the call targets, when it targets one.
        file_paths: Every file the call touches, apply_patch multi-file included;
            ``file_path`` mirrors the first entry.
        is_error: Whether the call's result reported a failure.
        denied: Whether the result is a user rejection of the tool use.
        denial_kind: The structured tool-denial kind (``user-rejected`` for a human
            rejection, ``permission-rule`` for a hook/guard block), or None.
        user_said: The user's verbatim instruction embedded in a denial, else None.
        duration_ms: Milliseconds from the call to its result, or None without one.
    """

    ts: datetime | None
    session_id: SessionId
    path: Path
    cwd: str | None
    tool_use_id: ToolUseId
    tool: str
    command_prefixes: tuple[str, ...]
    command: str | None
    mcp_server: str | None
    mcp_tool: str | None
    mcp_access: Literal["read", "write"] | None
    file_path: str | None
    file_paths: tuple[str, ...]
    is_error: bool
    denied: bool
    denial_kind: str | None
    user_said: str | None
    duration_ms: int | None


def rehydrate_fact(fact: Mapping[str, Any], path: Path) -> ToolFact:
    return ToolFact(
        ts=datetime.fromisoformat(fact["ts"]),
        session_id=SessionId(fact["session_id"]),
        path=path,
        cwd=fact["cwd"],
        tool_use_id=ToolUseId(fact["tool_use_id"]),
        tool=fact["tool"],
        command_prefixes=tuple(fact["command_prefixes"]),
        command=fact["command"],
        mcp_server=fact["mcp_server"],
        mcp_tool=fact["mcp_tool"],
        mcp_access=fact["mcp_access"],
        file_path=fact["file_path"],
        file_paths=tuple(fact["file_paths"]),
        is_error=fact["is_error"],
        denied=fact["denied"],
        denial_kind=fact["denial_kind"],
        user_said=fact["user_said"],
        duration_ms=fact["duration_ms"],
    )


def tool_facts(paths: Sequence[str | Path], *, max_events: int) -> Iterator[ToolFact]:
    """Yields one :class:`ToolFact` per tool call across every transcript file.

    Each path is parsed and projected by the native core over its first
    ``max_events`` events; a file whose events carry no session identity yields
    nothing. Every fact re-attaches its source ``path`` and rehydrates the native
    projection — command prefixes, MCP split, denial fields, and duration — into
    the typed :class:`ToolFact`. Calls are yielded per file, then in turn order,
    then call order.

    Args:
        paths: The transcript files to project, in order.
        max_events: The per-file cap on parsed events to project.

    Yields:
        A flattened fact per tool use, in file order.
    """
    resolved = [Path(path) for path in paths]
    return (
        rehydrate_fact(fact, path)
        for path, entry in zip(resolved, _native.tool_facts([str(path) for path in resolved], max_events), strict=True)
        for fact in entry["facts"]
    )


def command_prefix_counts(facts: Iterable[ToolFact]) -> dict[str, int]:
    """Counts every Bash command prefix across ``facts``, most frequent first.

    Flattens each fact's :attr:`ToolFact.command_prefixes`, so a piped command
    contributes one count per segment.

    Args:
        facts: The tool facts to tally.

    Returns:
        Prefix-to-count pairs ordered by descending frequency.
    """
    return dict(Counter(prefix for fact in facts for prefix in fact.command_prefixes).most_common())


def mcp_summary(facts: Iterable[ToolFact]) -> dict[str, dict[str, int | dict[str, int]]]:
    """Summarizes MCP usage per server across ``facts``.

    Groups the facts whose :attr:`ToolFact.mcp_server` is set, tallying read and
    write access, the total call count, and a per-tool frequency map. Servers are
    ordered by descending total then name; each ``tools`` map is ordered by
    descending frequency.

    Args:
        facts: The tool facts to summarize.

    Returns:
        A mapping from server to ``{"read", "write", "total", "tools"}``.
    """
    grouped: defaultdict[str, list[tuple[str, str]]] = defaultdict(list)
    for fact in facts:
        match fact.mcp_server, fact.mcp_tool, fact.mcp_access:
            case (str() as server, str() as tool, str() as access):
                grouped[server].append((tool, access))
    return {
        server: {
            "read": sum(access == "read" for _, access in calls),
            "write": sum(access == "write" for _, access in calls),
            "total": len(calls),
            "tools": dict(Counter(tool for tool, _ in calls).most_common()),
        }
        for server, calls in sorted(grouped.items(), key=lambda item: (-len(item[1]), item[0]))
    }
