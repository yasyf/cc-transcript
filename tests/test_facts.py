from __future__ import annotations

from datetime import timedelta
from typing import TYPE_CHECKING, Any

import orjson

from cc_transcript.facts import ToolFact, command_prefix_counts, mcp_summary, tool_facts
from cc_transcript.filterspec import (
    DENIAL_KIND_PERMISSION_RULE,
    DENIAL_KIND_USER_REJECTED,
    DENIAL_PREFIX,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)
from cc_transcript.ids import ToolUseId
from tests import testkit
from tests.support import BASE, SESSION

if TYPE_CHECKING:
    from collections.abc import Mapping
    from pathlib import Path

MAX_EVENTS = 150


def use(id: str, name: str, **input: Any) -> dict[str, Any]:
    return testkit.tool_use(id, name, input)


def bash(id: str, command: str) -> dict[str, Any]:
    return use(id, "Bash", command=command)


def asst(uuid: str, *blocks: dict[str, Any], secs: int = 0) -> dict[str, Any]:
    return testkit.assistant_line(uuid, blocks=blocks, session_id=str(SESSION), timestamp=BASE, secs=secs)


def usr(uuid: str, text: str = "", *, blocks: tuple[dict[str, Any], ...] = (), secs: int = 0, **flags: Any) -> dict[str, Any]:
    return testkit.user_line(uuid, text, blocks=blocks, session_id=str(SESSION), timestamp=BASE, secs=secs, **flags)


def denial(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def write(tmp_path: Path, name: str, *lines: Mapping[str, Any]) -> Path:
    path = tmp_path / name
    path.write_bytes(b"\n".join(orjson.dumps(dict(line)) for line in lines))
    return path


def facts_of(*paths: Path, max_events: int = MAX_EVENTS) -> list[ToolFact]:
    return list(tool_facts(paths, max_events=max_events))


def test_bash_call_yields_prefixes_and_command(tmp_path: Path) -> None:
    path = write(tmp_path, "s.jsonl", usr("u0", "run"), asst("a0", bash("t1", "git add . && pytest"), secs=1))
    (fact,) = facts_of(path)
    assert fact.tool == "Bash"
    assert fact.tool_use_id == ToolUseId("t1")
    assert fact.command == "git add . && pytest"
    assert fact.command_prefixes == ("git add", "pytest")
    assert (fact.mcp_server, fact.mcp_tool, fact.mcp_access) == (None, None, None)
    assert fact.file_path is None
    assert (fact.is_error, fact.denied, fact.user_said) == (False, False, None)
    assert fact.duration_ms is None
    assert (fact.session_id, fact.path, fact.ts) == (SESSION, path, BASE + timedelta(seconds=1))


def test_mcp_call_populates_server_tool_and_access(tmp_path: Path) -> None:
    path = write(tmp_path, "s.jsonl", usr("u0", "search"), asst("a0", use("t1", "mcp__semble__search", query="x"), secs=1))
    (fact,) = facts_of(path)
    assert fact.tool == "mcp__semble__search"
    assert (fact.mcp_server, fact.mcp_tool, fact.mcp_access) == ("semble", "search", "read")
    assert fact.command is None
    assert fact.command_prefixes == ()


def test_batched_prefixes_stay_aligned_across_interleaved_calls(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "go"),
        asst(
            "a0",
            bash("t1", "git add ."),
            use("t2", "mcp__semble__search", query="x"),
            bash("t3", "sudo git push -f && echo hi"),
            secs=1,
        ),
    )
    facts = facts_of(path)
    assert [fact.tool_use_id for fact in facts] == [ToolUseId("t1"), ToolUseId("t2"), ToolUseId("t3")]
    assert [fact.command_prefixes for fact in facts] == [("git add",), (), ("git push", "echo")]


def test_denied_result_sets_denied_and_extracts_user_said(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "cleanup"),
        asst("a0", bash("t1", "rm -rf /tmp/x"), secs=1),
        usr("u1", blocks=(testkit.tool_result("t1", denial("do not run that"), is_error=True),), secs=2),
    )
    (fact,) = facts_of(path)
    assert fact.denied is True
    assert fact.denial_kind == DENIAL_KIND_USER_REJECTED
    assert fact.user_said == "do not run that"
    assert fact.is_error is True
    assert fact.duration_ms == 1000


def test_structured_user_rejection_without_banner_is_a_denial(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "cleanup"),
        asst("a0", bash("t1", "rm -rf /tmp/x"), secs=1),
        usr(
            "u1",
            blocks=(testkit.tool_result("t1", "just stop", is_error=True),),
            tool_denial_kind=DENIAL_KIND_USER_REJECTED,
            secs=2,
        ),
    )
    (fact,) = facts_of(path)
    assert fact.denied is True
    assert fact.denial_kind == DENIAL_KIND_USER_REJECTED
    assert fact.is_error is True


def test_permission_rule_block_is_not_a_denial(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "cleanup"),
        asst("a0", bash("t1", "rm -rf /tmp/x"), secs=1),
        usr(
            "u1",
            blocks=(testkit.tool_result("t1", "Error: BLOCKED: dangerous", is_error=True),),
            tool_denial_kind=DENIAL_KIND_PERMISSION_RULE,
            secs=2,
        ),
    )
    (fact,) = facts_of(path)
    assert fact.is_error is True
    assert fact.denied is False
    assert fact.denial_kind == DENIAL_KIND_PERMISSION_RULE
    assert fact.user_said is None


def test_error_result_without_denial_sets_is_error_only(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "read"),
        asst("a0", use("t1", "Read", file_path="/missing.py"), secs=1),
        usr("u1", blocks=(testkit.tool_result("t1", "ENOENT: no such file", is_error=True),), secs=2),
    )
    (fact,) = facts_of(path)
    assert fact.is_error is True
    assert fact.denied is False
    assert fact.denial_kind is None
    assert fact.user_said is None
    assert fact.file_path == "/missing.py"


def test_duration_ms_from_matched_result_timestamp(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "go"),
        asst("a0", bash("t1", "sleep 1"), secs=3),
        usr("u1", blocks=(testkit.tool_result("t1", "done"),), secs=5),
    )
    (fact,) = facts_of(path)
    assert fact.ts == BASE + timedelta(seconds=3)
    assert fact.duration_ms == 2000
    assert (fact.is_error, fact.denied) == (False, False)


def test_transcript_without_meta_is_skipped(tmp_path: Path) -> None:
    path = write(tmp_path, "s.jsonl", testkit.mode_line("plan", session_id=str(SESSION)))
    assert facts_of(path) == []


def test_facts_span_multiple_transcripts_with_their_paths(tmp_path: Path) -> None:
    a = write(tmp_path, "a.jsonl", usr("u0", "a"), asst("a0", bash("t1", "ls"), secs=1))
    b = write(tmp_path, "b.jsonl", usr("u0", "b"), asst("a0", bash("t2", "pwd"), secs=1))
    facts = facts_of(a, b)
    assert [(f.command, f.path) for f in facts] == [("ls", a), ("pwd", b)]
    assert all(isinstance(f, ToolFact) for f in facts)


def test_command_prefix_counts_flattens_and_orders_by_frequency(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "work"),
        asst(
            "a0",
            bash("t1", "git status"),
            bash("t2", "git status"),
            bash("t3", "git status"),
            bash("t4", "pytest"),
            bash("t5", "pytest"),
            bash("t6", "ls"),
            use("t7", "mcp__semble__search", query="x"),
            secs=1,
        ),
    )
    counts = command_prefix_counts(facts_of(path))
    assert counts == {"git status": 3, "pytest": 2, "ls": 1}
    assert list(counts) == ["git status", "pytest", "ls"]


def test_mcp_summary_groups_counts_and_orders(tmp_path: Path) -> None:
    path = write(
        tmp_path,
        "s.jsonl",
        usr("u0", "go"),
        asst(
            "a0",
            use("t1", "mcp__semble__search", query="a"),
            use("t2", "mcp__semble__search", query="b"),
            use("t3", "mcp__semble__find_related", ref="x"),
            use("t4", "mcp__railway__deploy"),
            use("t5", "mcp__railway__get_logs"),
            use("t6", "mcp__aaa__list_items"),
            use("t7", "mcp__aaa__set_config"),
            bash("t8", "ls"),
            secs=1,
        ),
    )
    summary = mcp_summary(facts_of(path))
    assert summary == {
        "semble": {"read": 3, "write": 0, "total": 3, "tools": {"search": 2, "find_related": 1}},
        "aaa": {"read": 1, "write": 1, "total": 2, "tools": {"list_items": 1, "set_config": 1}},
        "railway": {"read": 1, "write": 1, "total": 2, "tools": {"deploy": 1, "get_logs": 1}},
    }
    assert list(summary) == ["semble", "aaa", "railway"]
    assert list(summary["semble"]["tools"]) == ["search", "find_related"]
