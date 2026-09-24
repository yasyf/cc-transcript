from __future__ import annotations

from collections.abc import Sequence
from datetime import UTC, datetime
from typing import Any

import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    ModeEvent,
    QueuedCommand,
    SessionId,
    UserEvent,
)
from cc_transcript.render import (
    Budget,
    clip,
    render_session,
    render_tool_call,
    render_turn,
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


def queued(prompt: str, *, mode: str = "prompt", origin: str | None = "human") -> AttachmentEvent:
    attachment = {"type": "queued_command", "prompt": prompt, "commandMode": mode}
    line = {"type": "attachment", "attachment": attachment | ({"origin": {"kind": origin}} if origin else {})}
    event = testkit.parse_event(line | testkit.meta_fields("uuid-1", session_id="sess-1", timestamp=TS))
    assert isinstance(event, AttachmentEvent)
    return event


def mode(value: str = "plan", *, session_id: str = "sess-1") -> ModeEvent:
    event = testkit.parse_event(testkit.mode_line(value, session_id=session_id))
    assert isinstance(event, ModeEvent)
    return event


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
        pytest.param("Read", {"file_path": "/x"}, 'Read({"file_path":"/x"})', id="other-read-compact"),
        pytest.param("Agent", {"prompt": "do it"}, 'Agent({"prompt":"do it"})', id="other-task-compact"),
        pytest.param(
            "mcp__github__search",
            {"query": "x"},
            'mcp__github__search({"query":"x"})',
            id="other-mcp-compact",
        ),
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


def test_queued_command_exposes_origin() -> None:
    detail = queued("send it", origin="peer").detail
    assert isinstance(detail, QueuedCommand)
    assert (detail.prompt, detail.command_mode, detail.origin) == ("send it", "prompt", "peer")
    assert queued("<task-notification>", mode="task-notification", origin=None).detail.origin is None


def test_render_turn_renders_the_users_mid_turn_messages_in_order() -> None:
    act = SessionActivity.from_events(
        SessionId("sess-1"),
        (
            user("post an ack in the thread"),
            assistant("drafting"),
            queued("<task-notification>drafted</task-notification>", mode="task-notification", origin=None),
            queued('<agent-message from="lead">send it</agent-message>', origin="peer"),
            queued('<channel source="plugin">send it</channel>', origin="channel"),
            queued("once you have the draft, send it"),
            assistant("sending"),
        ),
    )
    assert render_turn(act.turns[0], budget=Budget()) == (
        "user: post an ack in the thread\nassistant: drafting\nuser: once you have the draft, send it\nassistant: sending"
    )


def test_render_turn_renders_the_users_ask_user_question_answer() -> None:
    questions = [{"question": "Send it?", "header": "Send", "multiSelect": False, "options": [{"label": "Send"}]}]
    answer = 'User has answered your questions: "Send it?"="Send" selected preview:\nhello'
    act = SessionActivity.from_events(
        SessionId("sess-1"),
        (
            user("draft a reply"),
            assistant(blocks=(testkit.tool_use("q1", "AskUserQuestion", {"questions": questions}),)),
            user(blocks=(testkit.tool_result("q1", answer),)),
            assistant(blocks=(testkit.tool_use("b1", "Bash", {"command": "ls"}),)),
            user(blocks=(testkit.tool_result("b1", "a.py"),)),
        ),
    )
    ask = render_tool_call(parse_tool_call("AskUserQuestion", {"questions": questions}), budget=Budget())
    assert render_turn(act.turns[0], budget=Budget()) == (
        f"user: draft a reply\n{ask}\nuser answered: {answer}\nls"
    )


def test_render_turn_skips_a_failed_ask_user_question() -> None:
    act = SessionActivity.from_events(
        SessionId("sess-1"),
        (
            user("draft a reply"),
            assistant(blocks=(testkit.tool_use("q1", "AskUserQuestion", {"questions": []}),)),
            user(blocks=(testkit.tool_result("q1", "dismissed", is_error=True),)),
        ),
    )
    assert render_turn(act.turns[0], budget=Budget()) == 'user: draft a reply\nAskUserQuestion({"questions":[]})'
