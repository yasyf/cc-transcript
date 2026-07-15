from __future__ import annotations

from collections.abc import Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.backend import ParsedTranscript
from cc_transcript.filterspec import tool_names
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    ModeEvent,
    OtherEvent,
    SessionId,
    SystemEvent,
    ToolUseId,
    TranscriptEvent,
    UserEvent,
)
from cc_transcript.render import (
    Budget,
    Stats,
    clip,
    collect_stats,
    compact_line,
    event_dict,
    human_size,
    primary_arg,
    render_session,
    render_stats,
    render_tool_call,
    render_turn,
    truncate,
    view_asdict,
    view_field,
)
from cc_transcript.tools import parse_tool_call
from tests import testkit

NAMES = {ToolUseId("toolu_1"): "Bash"}

TS = datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)


def _mkw(
    *,
    timestamp: datetime = TS,
    session_id: str = "sess-1",
    is_sidechain: bool = False,
    is_meta: bool = False,
    is_compact_summary: bool = False,
) -> dict[str, Any]:
    return {
        "session_id": session_id,
        "timestamp": timestamp,
        "is_sidechain": is_sidechain,
        "is_meta": is_meta,
        "is_compact_summary": is_compact_summary,
    }


def user(text: str = "", *, blocks: Sequence[dict[str, Any]] = (), interrupted: bool = False, **mk: Any) -> UserEvent:
    event = testkit.parse_event(testkit.user_line("uuid-1", text, blocks=blocks, interrupted=interrupted, **_mkw(**mk)))
    assert isinstance(event, UserEvent)
    return event


def assistant(
    text: str = "",
    *,
    model: str = "claude-opus-4-7",
    blocks: Sequence[dict[str, Any]] = (),
    stop_reason: str | None = None,
    **mk: Any,
) -> AssistantEvent:
    event = testkit.parse_event(
        testkit.assistant_line("uuid-1", text, model=model, blocks=blocks, stop_reason=stop_reason, **_mkw(**mk))
    )
    assert isinstance(event, AssistantEvent)
    return event


def system(subtype: str, *, content: str | None = None, **mk: Any) -> SystemEvent:
    line: dict[str, Any] = {"type": "system", "subtype": subtype} | testkit.meta_fields("uuid-1", **_mkw(**mk))
    if content is not None:
        line["content"] = content
    event = testkit.parse_event(line)
    assert isinstance(event, SystemEvent)
    return event


def attachment(payload: dict[str, Any], **mk: Any) -> AttachmentEvent:
    event = testkit.parse_event(
        {"type": "attachment", "attachment": payload} | testkit.meta_fields("uuid-1", **_mkw(**mk))
    )
    assert isinstance(event, AttachmentEvent)
    return event


def mode(value: str = "plan", *, session_id: str = "sess-1") -> ModeEvent:
    event = testkit.parse_event(testkit.mode_line(value, session_id=session_id))
    assert isinstance(event, ModeEvent)
    return event


def other(type: str) -> OtherEvent:
    event = testkit.parse_event(testkit.other_line(type))
    assert isinstance(event, OtherEvent)
    return event


ASSISTANT_THINKING = assistant(
    blocks=(testkit.thinking_block("let me think"), testkit.tool_use("t9", "Read", {"file_path": "/x"})),
    stop_reason="tool_use",
)


@pytest.mark.parametrize(
    ("text", "width", "expected"),
    [
        pytest.param("a  b\n\tc", 100, "a b c", id="whitespace-collapse"),
        pytest.param("abcdef", 4, "abc…", id="width-cut"),
        pytest.param("a    bcdef", 4, "a b…", id="collapse-then-cut"),
        pytest.param("ab   cd", 0, "ab cd", id="width-zero-no-cut"),
        pytest.param("abcd", 4, "abcd", id="exact-fit"),
    ],
)
def test_truncate(text: str, width: int, expected: str) -> None:
    assert truncate(text, width) == expected


@pytest.mark.parametrize(
    ("n", "expected"),
    [
        pytest.param(0, "0B", id="zero"),
        pytest.param(1023, "1023B", id="bytes-max"),
        pytest.param(1024, "1.0KB", id="kb-min"),
        pytest.param(1536, "1.5KB", id="kb-fraction"),
        pytest.param(1024**2, "1.0MB", id="mb"),
        pytest.param(1024**3, "1.0GB", id="gb"),
        pytest.param(1024**4, "1.0TB", id="tb"),
        pytest.param(1024**5, "1024.0TB", id="beyond-tb-stays-tb"),
    ],
)
def test_human_size(n: int, expected: str) -> None:
    assert human_size(n) == expected


@pytest.mark.parametrize(
    ("input", "expected"),
    [
        pytest.param({"command": "ls", "file_path": "/x"}, "/x", id="file-path-beats-command"),
        pytest.param({"description": "d", "command": "ls"}, "ls", id="command-beats-description"),
        pytest.param({"weird": 42}, "42", id="fallback-first-value"),
        pytest.param({}, "", id="empty-input"),
    ],
)
def test_primary_arg(input: dict[str, Any], expected: str) -> None:
    assert primary_arg(input) == expected


def test_budget_defaults() -> None:
    assert Budget() == Budget(turn_chars=700, tool_chars=1500)


@pytest.mark.parametrize(
    ("text", "limit", "expected"),
    [
        pytest.param("abc", 4, "abc", id="under-limit-unchanged"),
        pytest.param("abcd", 4, "abcd", id="exact-fit-unchanged"),
        pytest.param("abcdef", 4, "abcd…(+2ch)", id="cut-marks-omitted-count"),
        pytest.param("a\nb\nc", 3, "a\nb…(+2ch)", id="preserves-newlines"),
    ],
)
def test_clip(text: str, limit: int, expected: str) -> None:
    assert clip(text, limit) == expected


@pytest.mark.parametrize(
    ("name", "input", "expected"),
    [
        pytest.param("Bash", {"command": "uv run pytest"}, "uv run pytest", id="bash-bare-command"),
        pytest.param(
            "Edit",
            {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"},
            "Edit /a.py\n- x = 1\n+ x = 2",
            id="edit-path-old-new",
        ),
        pytest.param(
            "Edit",
            {"file_path": "/a.py", "old_string": "a\nb", "new_string": "c"},
            "Edit /a.py\n- a\n- b\n+ c",
            id="edit-prefixes-every-line",
        ),
        pytest.param(
            "Edit",
            {"file_path": "/a.py", "old_string": "", "new_string": "x"},
            "Edit /a.py\n-\n+ x",
            id="edit-empty-old-keeps-marker",
        ),
        pytest.param("Write", {"file_path": "/b.py", "content": "print(1)"}, "Write /b.py\nprint(1)", id="write"),
        pytest.param("Read", {"file_path": "/x"}, "Read(/x)", id="other-read-compact"),
        pytest.param("Agent", {"prompt": "do it"}, "Agent(do it)", id="other-task-compact"),
        pytest.param("mcp__github__search", {"query": "x"}, "mcp__github__search(x)", id="other-mcp-compact"),
    ],
)
def test_render_tool_call(name: str, input: dict[str, Any], expected: str) -> None:
    assert render_tool_call(parse_tool_call(name, input), budget=Budget()) == expected


def test_render_tool_call_multiedit_marks_every_span() -> None:
    call = parse_tool_call(
        "MultiEdit",
        {
            "file_path": "/a.py",
            "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c", "new_string": "d"},
                {"old_string": "e", "new_string": "f"},
            ],
        },
    )
    assert render_tool_call(call, budget=Budget()) == (
        "MultiEdit /a.py\nedit 1/3\n- a\n+ b\nedit 2/3\n- c\n+ d\nedit 3/3\n- e\n+ f"
    )


def test_render_tool_call_multiedit_clips_each_span_to_tool_budget() -> None:
    call = parse_tool_call(
        "MultiEdit",
        {
            "file_path": "/a.py",
            "edits": [
                {"old_string": "o" * 12, "new_string": "n" * 12},
                {"old_string": "ppp", "new_string": "qqq"},
            ],
        },
    )
    assert render_tool_call(call, budget=Budget(tool_chars=8)) == (
        f"MultiEdit /a.py\nedit 1/2\n- {'o' * 8}…(+4ch)\n+ {'n' * 8}…(+4ch)\nedit 2/2\n- ppp\n+ qqq"
    )


def test_render_tool_call_clips_bash_command() -> None:
    call = parse_tool_call("Bash", {"command": "a" * 30})
    assert render_tool_call(call, budget=Budget(tool_chars=10)) == f"{'a' * 10}…(+20ch)"


def test_render_turn_orders_prompt_prose_and_tool_calls() -> None:
    act = SessionActivity.from_events(
        SessionId("sess-1"),
        (
            user("fix the bug"),
            assistant(
                "editing",
                blocks=(
                    testkit.tool_use(
                        "t1", "Edit", {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"}
                    ),
                    testkit.text_block("done"),
                ),
                stop_reason="tool_use",
            ),
        ),
    )
    assert render_turn(act.turns[0], budget=Budget()) == (
        "user: fix the bug\nassistant: editing\nEdit /a.py\n- x = 1\n+ x = 2\nassistant: done"
    )


def test_render_turn_clips_prose_to_turn_budget() -> None:
    act = SessionActivity.from_events(SessionId("sess-1"), (user("a" * 30),))
    assert render_turn(act.turns[0], budget=Budget(turn_chars=10)) == f"user: {'a' * 10}…(+20ch)"


def test_render_session_joins_turns_skipping_empty() -> None:
    act = SessionActivity.from_events(
        SessionId("sess-1"),
        (
            mode("plan", session_id="sess-1"),
            user("one"),
            assistant("ack", stop_reason="end_turn"),
            user("two"),
        ),
    )
    assert render_session(act, budget=Budget()) == "user: one\nassistant: ack\n\nuser: two"


@pytest.mark.parametrize(
    ("index", "event", "thinking", "expected"),
    [
        pytest.param(
            1,
            user("fix the bug"),
            False,
            "    1 user  03:04:05 fix the bug",
            id="user-plain",
        ),
        pytest.param(
            2,
            user("[Request interrupted by user]", interrupted=True),
            False,
            "    2 user  03:04:05 [int] [Request interrupted by user]",
            id="user-interrupted",
        ),
        pytest.param(
            3,
            user(blocks=(testkit.tool_result("toolu_1", "ok output"),)),
            False,
            "    3 user  03:04:05 <-Bash (9ch) ok output",
            id="user-tool-result-correlated-name",
        ),
        pytest.param(
            4,
            user(blocks=(testkit.tool_result("toolu_1", "boom", is_error=True),)),
            False,
            "    4 user  03:04:05 <-Bash[err] (4ch) boom",
            id="user-tool-result-error",
        ),
        pytest.param(
            5,
            user(blocks=(testkit.tool_result("missing", "boom"),)),
            False,
            "    5 user  03:04:05 <-? (4ch) boom",
            id="user-tool-result-unknown-id",
        ),
        pytest.param(
            6,
            ASSISTANT_THINKING,
            False,
            "    6 asst  03:04:05 [claude-opus-4-7] th(12ch) Read(/x)",
            id="assistant-thinking-marker",
        ),
        pytest.param(
            6,
            ASSISTANT_THINKING,
            True,
            "    6 asst  03:04:05 [claude-opus-4-7] th(12ch) let me think Read(/x)",
            id="assistant-thinking-inline",
        ),
        pytest.param(
            7,
            assistant("hi there", stop_reason="end_turn"),
            False,
            '    7 asst  03:04:05 [claude-opus-4-7] "hi there"',
            id="assistant-text",
        ),
        pytest.param(
            8,
            system("stop_hook_summary", content="hook ran"),
            False,
            "    8 sys   03:04:05 stop_hook_summary: hook ran",
            id="system-with-content",
        ),
        pytest.param(
            9,
            system("init"),
            False,
            "    9 sys   03:04:05 init",
            id="system-no-content",
        ),
        pytest.param(
            10,
            user("subagent prompt", is_sidechain=True),
            False,
            "   10 user* 03:04:05 subagent prompt",
            id="sidechain-star-tag",
        ),
        pytest.param(
            11,
            mode("plan", session_id="sess-1"),
            False,
            "   11 mode           mode=plan",
            id="mode-no-meta-blank-time",
        ),
        pytest.param(
            12,
            other("summary"),
            False,
            "   12 other          summary",
            id="other-no-meta-blank-time",
        ),
        pytest.param(
            18,
            attachment(
                {
                    "type": "hook_success",
                    "hookName": "PostToolUse:Bash",
                    "hookEvent": "PostToolUse",
                    "content": "hook ran ok",
                }
            ),
            False,
            "   18 att   03:04:05 hook_success hook ran ok",
            id="attachment-hook-success",
        ),
        pytest.param(
            13,
            assistant(blocks=(testkit.tool_use("t1", "Bash", {"command": "ls -la"}),), stop_reason="tool_use"),
            False,
            "   13 asst  03:04:05 [claude-opus-4-7] ls -la",
            id="bash-renders-bare-command",
        ),
        pytest.param(
            14,
            assistant(
                blocks=(
                    testkit.tool_use(
                        "t1", "Edit", {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"}
                    ),
                ),
                stop_reason="tool_use",
            ),
            False,
            "   14 asst  03:04:05 [claude-opus-4-7] Edit /a.py - x = 1 + x = 2",
            id="edit-delegates-to-tool-renderer-collapsed",
        ),
        pytest.param(
            15,
            assistant(blocks=(testkit.tool_use("t1", "Edit", {"file_path": "/a.py"}),), stop_reason="tool_use"),
            False,
            "   15 asst  03:04:05 [claude-opus-4-7] Edit(/a.py)",
            id="malformed-known-tool-degrades-to-compact",
        ),
        pytest.param(
            16,
            assistant(
                model="claude-opus-4-8",
                blocks=({"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"}},),
                stop_reason="tool_use",
            ),
            False,
            "   16 asst  03:04:05 [claude-opus-4-8] fallback claude-fable-5->claude-opus-4-8",
            id="fallback-block-renders-model-transition",
        ),
        pytest.param(
            17,
            assistant(model="claude-opus-4-8", blocks=({"type": "future_block"},)),
            False,
            "   17 asst  03:04:05 [claude-opus-4-8] future_block",
            id="other-block-renders-bare-type",
        ),
    ],
)
def test_compact_line(index: int, event: TranscriptEvent, thinking: bool, expected: str) -> None:
    assert compact_line(index, event, names=NAMES, width=100, thinking=thinking, uuids=False) == expected


def test_compact_line_tool_input_clips_with_omitted_count() -> None:
    event = assistant(blocks=(testkit.tool_use("t1", "Bash", {"command": "abcdefghij"}),), stop_reason="tool_use")
    assert (
        compact_line(1, event, names={}, width=8, thinking=False, uuids=False)
        == "    1 asst  03:04:05 [claude-opus-4-7] abcdefgh…(+2ch)"
    )


def test_compact_line_width_zero_never_clips_tool_input() -> None:
    event = assistant(blocks=(testkit.tool_use("t1", "Bash", {"command": "x" * 500}),), stop_reason="tool_use")
    assert compact_line(1, event, names={}, width=0, thinking=False, uuids=False).endswith("x" * 500)


def test_compact_line_appends_uuid_with_meta() -> None:
    event = user("hi")
    assert compact_line(1, event, names={}, width=100, thinking=False, uuids=True) == "    1 user  03:04:05 hi uuid-1"


def test_compact_line_uuids_flag_skips_metaless_events() -> None:
    event = mode("plan", session_id="sess-1")
    assert compact_line(2, event, names={}, width=100, thinking=False, uuids=True) == "    2 mode           mode=plan"


def test_tool_names_correlates_ids_across_events() -> None:
    events = (
        user("q"),
        assistant(
            blocks=(
                testkit.tool_use("t1", "Read", {"file_path": "/x"}),
                testkit.tool_use("t2", "Bash", {"command": "ls"}),
            ),
            stop_reason="tool_use",
        ),
        other("summary"),
    )
    assert tool_names(events) == {ToolUseId("t1"): "Read", ToolUseId("t2"): "Bash"}


def test_event_dict_user_round_trips_through_orjson() -> None:
    event = user(blocks=(testkit.text_block("hi"),))
    assert orjson.loads(orjson.dumps(event_dict(7, event))) == {
        "i": 7,
        "kind": "user",
        "meta": {
            "uuid": "uuid-1",
            "parent_uuid": None,
            "session_id": "sess-1",
            "timestamp": "2026-01-02T03:04:05+00:00",
            "cwd": "/repo",
            "git_branch": "main",
            "cc_version": "1.2.3",
            "is_sidechain": False,
            "is_meta": False,
            "entrypoint": "cli",
            "is_compact_summary": False,
            "is_visible_in_transcript_only": False,
            "user_type": None,
            "slug": None,
        },
        "text": "hi",
        "blocks": [{"text": "hi"}],
        "interrupted": False,
        "is_agent_injected": False,
        "prompt_id": None,
        "prompt_source": None,
        "queue_priority": None,
        "image_paste_ids": None,
        "source_tool_use_id": None,
        "source_tool_assistant_uuid": None,
        "mcp_meta": None,
        "permission_mode": None,
        "interrupted_message_id": None,
    }


def test_event_dict_mode_event_has_no_meta() -> None:
    event = mode("plan", session_id="sess-1")
    assert orjson.loads(orjson.dumps(event_dict(3, event))) == {
        "i": 3,
        "kind": "mode",
        "session_id": "sess-1",
        "channel": "mode",
        "value": "plan",
    }


STATS_TRANSCRIPTS = (
    ParsedTranscript(
        path=Path("/a.jsonl"),
        mtime=0.0,
        events=(
            user("hello"),
            assistant(
                "hi",
                blocks=(testkit.thinking_block("hmm"), testkit.tool_use("t1", "Read", {"file_path": "/x"})),
                stop_reason="tool_use",
                timestamp=datetime(2026, 1, 2, 3, 4, 6, tzinfo=UTC),
            ),
            user(
                blocks=(testkit.tool_result("t1", "out", is_error=True),),
                timestamp=datetime(2026, 1, 2, 3, 4, 7, tzinfo=UTC),
            ),
            user(
                "[int]",
                interrupted=True,
                is_sidechain=True,
                timestamp=datetime(2026, 1, 2, 3, 4, 8, tzinfo=UTC),
            ),
            system("stop_hook_summary", content="ran", timestamp=datetime(2026, 1, 2, 3, 4, 9, tzinfo=UTC)),
            mode("plan", session_id="sess-2"),
            other("summary"),
            attachment(
                {"type": "hook_success", "hookName": "Stop", "hookEvent": "Stop", "content": "ok"},
                timestamp=datetime(2026, 1, 2, 3, 4, 6, tzinfo=UTC),
            ),
        ),
    ),
    ParsedTranscript(
        path=Path("/b.jsonl"),
        mtime=0.0,
        events=(
            assistant(
                "yo",
                model="claude-haiku",
                stop_reason="end_turn",
                timestamp=datetime(2026, 1, 2, 3, 5, 0, tzinfo=UTC),
                session_id="sess-3",
            ),
        ),
    ),
)

EXPECTED_STATS = Stats(
    files=2,
    events=9,
    kinds={"user": 3, "assistant": 2, "system": 1, "mode": 1, "other": 1, "attachment": 1},
    models={"claude-opus-4-7": 1, "claude-haiku": 1},
    tools={"Read": 1},
    text_chars=14,
    thinking_chars=3,
    tool_io_chars=21,
    sessions=3,
    first_timestamp=datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC),
    last_timestamp=datetime(2026, 1, 2, 3, 5, 0, tzinfo=UTC),
    interrupts=1,
    tool_errors=1,
    sidechain=1,
)


def test_collect_stats_exact_counters() -> None:
    assert collect_stats(STATS_TRANSCRIPTS) == EXPECTED_STATS


def test_collect_stats_empty() -> None:
    assert collect_stats([]) == Stats(
        files=0,
        events=0,
        kinds={},
        models={},
        tools={},
        text_chars=0,
        thinking_chars=0,
        tool_io_chars=0,
        sessions=0,
        first_timestamp=None,
        last_timestamp=None,
        interrupts=0,
        tool_errors=0,
        sidechain=0,
    )


def test_render_stats_exact_output() -> None:
    assert render_stats(EXPECTED_STATS) == "\n".join(
        (
            "files        2",
            "events       9",
            "kinds        user 3 · assistant 2 · system 1 · mode 1 · other 1 · attachment 1",
            "models       claude-opus-4-7 1 · claude-haiku 1",
            "tools        Read 1",
            "text         14B",
            "thinking     3B",
            "tool io      21B",
            "sessions     3",
            "span         2026-01-02 03:04:05 → 2026-01-02 03:05:00",
            "interrupts   1",
            "tool errors  1",
            "sidechain    1",
        )
    )


def test_render_stats_empty_placeholders() -> None:
    rendered = render_stats(collect_stats([]))
    assert "kinds        -" in rendered
    assert "models       -" in rendered
    assert "tools        -" in rendered
    assert "span         -" in rendered


def test_view_field_recurses_records_and_passes_non_record_views_through() -> None:
    from cc_transcript.parser import parse

    transcript = parse(orjson.dumps(testkit.user_line("u1", "hi")) + b"\n")
    events = transcript.events
    assert view_field(transcript) is transcript
    assert view_field(events) is events
    event = events[0]
    assert view_field(event) == view_asdict(event)
    assert view_field(event.meta)["uuid"] == "u1"
