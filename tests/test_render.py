from __future__ import annotations

from collections.abc import Sequence
from datetime import UTC, datetime
from typing import Any

import orjson
import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.filterspec import tool_names
from cc_transcript.models import (
    AssistantEvent,
    ModeEvent,
    OtherEvent,
    SessionId,
    ToolUseId,
    UserEvent,
)
from cc_transcript.render import (
    Budget,
    clip,
    event_dict,
    primary_arg,
    render_session,
    render_tool_call,
    render_turn,
    view_asdict,
    view_field,
)
from cc_transcript.tools import parse_tool_call
from tests import testkit

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


def mode(value: str = "plan", *, session_id: str = "sess-1") -> ModeEvent:
    event = testkit.parse_event(testkit.mode_line(value, session_id=session_id))
    assert isinstance(event, ModeEvent)
    return event


def other(type: str) -> OtherEvent:
    event = testkit.parse_event(testkit.other_line(type))
    assert isinstance(event, OtherEvent)
    return event


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


def test_view_field_recurses_records_and_passes_non_record_views_through() -> None:
    from cc_transcript.parser import parse

    transcript = parse(orjson.dumps(testkit.user_line("u1", "hi")) + b"\n")
    events = transcript.events
    assert view_field(transcript) is transcript
    assert view_field(events) is events
    event = events[0]
    assert view_field(event) == view_asdict(event)
    assert view_field(event.meta)["uuid"] == "u1"
