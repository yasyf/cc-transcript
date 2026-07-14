"""The single tool-call analytics substrate.

A pure projection of a parsed transcript's tool activity: :func:`tool_facts`
lifts every :class:`~cc_transcript.activity.SessionActivity` tool use into a
flat :class:`ToolFact` — command prefixes, MCP server/tool/access, file path,
error and denial state, and duration — and the aggregators
(:func:`command_prefix_counts`, :func:`mcp_summary`) roll those facts up. Every
function is pure over :attr:`~cc_transcript.backend.ParsedTranscript.events`,
so the CLI's stats surface and any downstream analytics share one substrate.
"""

from __future__ import annotations

from collections import Counter, defaultdict
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from cc_transcript.activity import SessionActivity
from cc_transcript.filterspec import DENIAL_KIND_USER_REJECTED, embedded_user_text, session_id_of
from cc_transcript.tools import BashCall, file_path_of, mcp_access, mcp_parts

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator
    from datetime import datetime
    from pathlib import Path

    from cc_transcript.activity import ToolUse
    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.ids import SessionId, ToolUseId
    from cc_transcript.models import ToolResultBlock, Transcript


def is_denial(block: ToolResultBlock) -> bool:
    return block.denial_kind == DENIAL_KIND_USER_REJECTED


def denial_fields(result: ToolResultBlock | None) -> tuple[bool, str | None]:
    if result is None or not is_denial(result):
        return False, None
    return True, embedded_user_text(result.content)


def mcp_split(name: str) -> tuple[str | None, str | None, Literal["read", "write"] | None]:
    match mcp_parts(name):
        case (server, tool):
            return server, tool, mcp_access(tool)
        case None:
            return None, None, None


@dataclass(frozen=True, slots=True)
class ToolFact:
    """One tool call flattened for analytics, lifted from a parsed transcript.

    Attributes:
        ts: The timestamp of the assistant entry that made the call, or None.
        session_id: The Claude session the call belongs to.
        path: The transcript file the call was lifted from.
        tool_use_id: The tool-use block id — the join key back to the event stream.
        tool: The tool name exactly as invoked.
        command_prefixes: The permission-style prefixes of each Bash command
            segment; ``()`` for non-Bash calls.
        command: The shell command for a Bash call, else None.
        mcp_server: The MCP server segment for an ``mcp__server__tool`` call, else None.
        mcp_tool: The MCP tool segment for an ``mcp__server__tool`` call, else None.
        mcp_access: Whether the MCP tool reads or writes, else None.
        file_path: The file the call targets, when it targets one.
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
    tool_use_id: ToolUseId
    tool: str
    command_prefixes: tuple[str, ...]
    command: str | None
    mcp_server: str | None
    mcp_tool: str | None
    mcp_access: Literal["read", "write"] | None
    file_path: str | None
    is_error: bool
    denied: bool
    denial_kind: str | None
    user_said: str | None
    duration_ms: int | None


def fact_of(use: ToolUse, session_id: SessionId, path: Path, prefixes: tuple[str, ...]) -> ToolFact:
    call = use.call
    server, tool, access = mcp_split(call.name)
    denied, user_said = denial_fields(use.result)
    assert use.ref.tool_use_id is not None, "ToolUse refs always carry the tool-use id"
    return ToolFact(
        ts=use.ts,
        session_id=session_id,
        path=path,
        tool_use_id=use.ref.tool_use_id,
        tool=call.name,
        command_prefixes=prefixes,
        command=call.command if isinstance(call, BashCall) else None,
        mcp_server=server,
        mcp_tool=tool,
        mcp_access=access,
        file_path=file_path_of(call),
        is_error=use.result.is_error if use.result is not None else False,
        denied=denied,
        denial_kind=use.result.denial_kind if use.result is not None else None,
        user_said=user_said,
        duration_ms=use.duration_ms,
    )


def tool_facts(transcripts: Iterable[Transcript | ParsedTranscript]) -> Iterator[ToolFact]:
    """Yields one :class:`ToolFact` per tool call across every transcript.

    Each transcript is lifted into a :class:`~cc_transcript.activity.SessionActivity`
    keyed by the session of its first meta-bearing event; transcripts carrying no
    such event are skipped. Bash commands are prefix-parsed in one
    :func:`~cc_transcript.command.bulk_command_prefixes` batch per transcript.
    Calls are yielded in turn order, then call order.

    Args:
        transcripts: The parsed transcripts to project.

    Yields:
        A flattened fact per tool use, in file order.
    """
    from cc_transcript.command import bulk_command_prefixes

    for parsed in transcripts:
        session_id = session_id_of(parsed.events)
        if session_id is None:
            continue
        activity = SessionActivity.from_events(session_id, parsed.events)
        uses = [use for turn in activity.turns for use in turn.tool_uses]
        prefixes = iter(bulk_command_prefixes([use.call.command for use in uses if isinstance(use.call, BashCall)]))
        yield from (
            fact_of(use, session_id, parsed.path, next(prefixes) if isinstance(use.call, BashCall) else ())
            for use in uses
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
