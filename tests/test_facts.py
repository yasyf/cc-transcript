from __future__ import annotations

from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any

from cc_transcript.backend import ParsedTranscript
from cc_transcript.facts import ToolFact, command_prefix_counts, is_denial, mcp_summary, tool_facts
from cc_transcript.filterspec import (
    DENIAL_KIND_PERMISSION_RULE,
    DENIAL_KIND_USER_REJECTED,
    DENIAL_PREFIX,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
)
from cc_transcript.ids import ToolUseId
from cc_transcript.models import ToolResultBlock
from tests import testkit
from tests.support import BASE, SESSION, assistant, user

if TYPE_CHECKING:
    from cc_transcript.models import TranscriptEvent

PATH = Path("/proj/session.jsonl")


def tool_use(id: str, name: str, **input: Any) -> dict[str, Any]:
    return testkit.tool_use(id, name, input)


def bash(id: str, command: str) -> dict[str, Any]:
    return tool_use(id, "Bash", command=command)


def result(
    id: str, content: str = "ok", *, is_error: bool = False, denial_kind: str | None = None
) -> ToolResultBlock:
    event = testkit.parse_event(
        testkit.user_line(
            "_r", "", blocks=(testkit.tool_result(id, content, is_error=is_error),), tool_denial_kind=denial_kind
        )
    )
    return event.blocks[0]


def denial(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def parsed(*events: TranscriptEvent, path: Path = PATH) -> ParsedTranscript:
    return ParsedTranscript(path=path, mtime=0.0, events=events)


def test_is_denial_keys_on_structured_user_rejected_kind() -> None:
    # Structured user-rejected without the legacy banner is still a denial.
    assert is_denial(result("t1", "just stop", is_error=True, denial_kind=DENIAL_KIND_USER_REJECTED)) is True
    # Legacy banner-only (no structured field) resolves to user-rejected at the parse layer.
    assert is_denial(result("t1", denial("stop"), is_error=True)) is True
    # A permission-rule hook/guard block is automation, never a user denial.
    assert is_denial(result("t1", "Error: BLOCKED: x", is_error=True, denial_kind=DENIAL_KIND_PERMISSION_RULE)) is False
    # A plain error carries no denial kind.
    assert is_denial(result("t1", "boom", is_error=True)) is False
    assert is_denial(result("t1", denial("stop"), is_error=False)) is False


def test_bash_call_yields_prefixes_and_command() -> None:
    (fact,) = tool_facts(
        [parsed(user("u0", "run"), assistant("a0", blocks=(bash("t1", "git add . && pytest"),), secs=1))]
    )
    assert fact.tool == "Bash"
    assert fact.tool_use_id == ToolUseId("t1")
    assert fact.command == "git add . && pytest"
    assert fact.command_prefixes == ("git add", "pytest")
    assert (fact.mcp_server, fact.mcp_tool, fact.mcp_access) == (None, None, None)
    assert fact.file_path is None
    assert (fact.is_error, fact.denied, fact.user_said) == (False, False, None)
    assert fact.duration_ms is None
    assert (fact.session_id, fact.path, fact.ts) == (SESSION, PATH, BASE + timedelta(seconds=1))


def test_mcp_call_populates_server_tool_and_access() -> None:
    (fact,) = tool_facts(
        [
            parsed(
                user("u0", "search"),
                assistant("a0", blocks=(tool_use("t1", "mcp__semble__search", query="x"),), secs=1),
            )
        ]
    )
    assert fact.tool == "mcp__semble__search"
    assert (fact.mcp_server, fact.mcp_tool, fact.mcp_access) == ("semble", "search", "read")
    assert fact.command is None
    assert fact.command_prefixes == ()


def test_batched_prefixes_stay_aligned_across_interleaved_calls() -> None:
    facts = list(
        tool_facts(
            [
                parsed(
                    user("u0", "go"),
                    assistant(
                        "a0",
                        blocks=(
                            bash("t1", "git add ."),
                            tool_use("t2", "mcp__semble__search", query="x"),
                            bash("t3", "sudo git push -f && echo hi"),
                        ),
                        secs=1,
                    ),
                )
            ]
        )
    )
    assert [fact.tool_use_id for fact in facts] == [ToolUseId("t1"), ToolUseId("t2"), ToolUseId("t3")]
    assert [fact.command_prefixes for fact in facts] == [("git add",), (), ("git push", "echo")]


def test_denied_result_sets_denied_and_extracts_user_said() -> None:
    (fact,) = tool_facts(
        [
            parsed(
                user("u0", "cleanup"),
                assistant("a0", blocks=(bash("t1", "rm -rf /tmp/x"),), secs=1),
                user("u1", blocks=(testkit.tool_result("t1", denial("do not run that"), is_error=True),), secs=2),
            )
        ]
    )
    assert fact.denied is True
    assert fact.denial_kind == DENIAL_KIND_USER_REJECTED
    assert fact.user_said == "do not run that"
    assert fact.is_error is True
    assert fact.duration_ms == 1000


def test_permission_rule_block_is_not_a_denial() -> None:
    (fact,) = tool_facts(
        [
            parsed(
                user("u0", "cleanup"),
                assistant("a0", blocks=(bash("t1", "rm -rf /tmp/x"),), secs=1),
                user(
                    "u1",
                    blocks=(testkit.tool_result("t1", "Error: BLOCKED: dangerous", is_error=True),),
                    tool_denial_kind=DENIAL_KIND_PERMISSION_RULE,
                    secs=2,
                ),
            )
        ]
    )
    assert fact.is_error is True
    assert fact.denied is False
    assert fact.denial_kind == DENIAL_KIND_PERMISSION_RULE
    assert fact.user_said is None


def test_error_result_without_denial_sets_is_error_only() -> None:
    (fact,) = tool_facts(
        [
            parsed(
                user("u0", "read"),
                assistant("a0", blocks=(tool_use("t1", "Read", file_path="/missing.py"),), secs=1),
                user("u1", blocks=(testkit.tool_result("t1", "ENOENT: no such file", is_error=True),), secs=2),
            )
        ]
    )
    assert fact.is_error is True
    assert fact.denied is False
    assert fact.denial_kind is None
    assert fact.user_said is None
    assert fact.file_path == "/missing.py"


def test_duration_ms_from_matched_result_timestamp() -> None:
    (fact,) = tool_facts(
        [
            parsed(
                user("u0", "go"),
                assistant("a0", blocks=(bash("t1", "sleep 1"),), secs=3),
                user("u1", blocks=(testkit.tool_result("t1", "done"),), secs=5),
            )
        ]
    )
    assert fact.ts == BASE + timedelta(seconds=3)
    assert fact.duration_ms == 2000
    assert (fact.is_error, fact.denied) == (False, False)


def test_transcript_without_meta_is_skipped() -> None:
    assert list(tool_facts([parsed(testkit.parse_event(testkit.mode_line("plan", session_id=str(SESSION))))])) == []


def test_facts_span_multiple_transcripts_with_their_paths() -> None:
    facts = list(
        tool_facts(
            [
                parsed(user("u0", "a"), assistant("a0", blocks=(bash("t1", "ls"),), secs=1), path=Path("/a.jsonl")),
                parsed(user("u0", "b"), assistant("a0", blocks=(bash("t2", "pwd"),), secs=1), path=Path("/b.jsonl")),
            ]
        )
    )
    assert [(f.command, str(f.path)) for f in facts] == [("ls", "/a.jsonl"), ("pwd", "/b.jsonl")]
    assert all(isinstance(f, ToolFact) for f in facts)


def test_command_prefix_counts_flattens_and_orders_by_frequency() -> None:
    facts = list(
        tool_facts(
            [
                parsed(
                    user("u0", "work"),
                    assistant(
                        "a0",
                        blocks=(
                            bash("t1", "git status"),
                            bash("t2", "git status"),
                            bash("t3", "git status"),
                            bash("t4", "pytest"),
                            bash("t5", "pytest"),
                            bash("t6", "ls"),
                            tool_use("t7", "mcp__semble__search", query="x"),
                        ),
                        secs=1,
                    ),
                )
            ]
        )
    )
    counts = command_prefix_counts(facts)
    assert counts == {"git status": 3, "pytest": 2, "ls": 1}
    assert list(counts) == ["git status", "pytest", "ls"]


def test_mcp_summary_groups_counts_and_orders() -> None:
    facts = list(
        tool_facts(
            [
                parsed(
                    user("u0", "go"),
                    assistant(
                        "a0",
                        blocks=(
                            tool_use("t1", "mcp__semble__search", query="a"),
                            tool_use("t2", "mcp__semble__search", query="b"),
                            tool_use("t3", "mcp__semble__find_related", ref="x"),
                            tool_use("t4", "mcp__railway__deploy"),
                            tool_use("t5", "mcp__railway__get_logs"),
                            tool_use("t6", "mcp__aaa__list_items"),
                            tool_use("t7", "mcp__aaa__set_config"),
                            bash("t8", "ls"),
                        ),
                        secs=1,
                    ),
                )
            ]
        )
    )
    summary = mcp_summary(facts)
    assert summary == {
        "semble": {"read": 3, "write": 0, "total": 3, "tools": {"search": 2, "find_related": 1}},
        "aaa": {"read": 1, "write": 1, "total": 2, "tools": {"list_items": 1, "set_config": 1}},
        "railway": {"read": 1, "write": 1, "total": 2, "tools": {"deploy": 1, "get_logs": 1}},
    }
    assert list(summary) == ["semble", "aaa", "railway"]
    assert list(summary["semble"]["tools"]) == ["search", "find_related"]
