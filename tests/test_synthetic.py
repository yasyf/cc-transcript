from __future__ import annotations

from datetime import UTC, datetime

import pytest

from cc_transcript import synthetic
from cc_transcript.activity import SessionActivity
from cc_transcript.ids import EventUuid
from cc_transcript.models import (
    AssistantEvent,
    ModeEvent,
    OtherEvent,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.parser import parse_events
from cc_transcript.render import Budget, render_turn
from cc_transcript.synthetic import (
    other_line,
    synthetic_assistant_event,
    synthetic_user_event,
    system_line,
    text_block,
    thinking_block,
    tool_result,
    tool_use,
)


def test_user_line_round_trips_to_user_event() -> None:
    event = parse_events(synthetic.user_line("u1", "fix the bug"))[0]
    assert isinstance(event, UserEvent)
    assert event.text == "fix the bug"
    assert event.meta.uuid == EventUuid("u1")


def test_assistant_line_round_trips_with_stop_reason_and_usage() -> None:
    event = parse_events(
        synthetic.assistant_line(
            "a1",
            "working",
            stop_reason="end_turn",
            usage={
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        )
    )[0]
    assert isinstance(event, AssistantEvent)
    assert event.text == "working"
    assert event.stop_reason == "end_turn"
    assert event.usage is not None
    assert event.usage.input_tokens == 10


@pytest.mark.parametrize(
    ("channel", "expected_channel"),
    [("mode", "mode"), ("permission-mode", "permission-mode")],
)
def test_mode_line_round_trips(channel: str, expected_channel: str) -> None:
    event = parse_events(synthetic.mode_line("plan", session_id="s1", channel=channel))[0]
    assert isinstance(event, ModeEvent)
    assert event.value == "plan"
    assert event.channel == expected_channel
    assert event.session_id == SessionId("s1")


def test_system_line_round_trips_to_system_event() -> None:
    event = parse_events(system_line("stop_hook_summary"))[0]
    assert isinstance(event, SystemEvent)
    assert event.subtype == "stop_hook_summary"


def test_other_line_round_trips_to_other_event() -> None:
    event = parse_events(other_line("summary"))[0]
    assert isinstance(event, OtherEvent)
    assert event.raw["type"] == "summary"


def test_content_block_builders_lift_into_typed_blocks() -> None:
    user = synthetic_user_event(blocks=[text_block("hi"), tool_result("t1", "out", is_error=True)])
    text, result = user.blocks
    assert isinstance(text, TextBlock) and text.text == "hi"
    assert isinstance(result, ToolResultBlock)
    assert result.tool_use_id == "t1"
    assert result.content == "out"
    assert result.is_error is True

    assistant = synthetic_assistant_event(blocks=[thinking_block("hmm"), tool_use("t1", "Bash", {"command": "ls"})])
    thinking, use = assistant.blocks
    assert isinstance(thinking, ThinkingBlock) and thinking.thinking == "hmm"
    assert isinstance(use, ToolUseBlock)
    assert use.name == "Bash"
    assert use.input == {"command": "ls"}


def test_meta_forwards_to_the_event() -> None:
    timestamp = datetime(2026, 2, 3, 4, 5, 6, tzinfo=UTC)
    event = synthetic_user_event("hi", session_id="sess-x", timestamp=timestamp)
    assert event.meta.session_id == SessionId("sess-x")
    assert event.meta.timestamp == timestamp


def test_synthetic_user_event_rejects_a_non_user_parse(monkeypatch: pytest.MonkeyPatch) -> None:
    assistant = synthetic_assistant_event("on it")
    monkeypatch.setattr(synthetic, "parse_events", lambda *lines: [assistant])
    with pytest.raises(ValueError, match="UserEvent"):
        synthetic_user_event("hi")


def test_synthetic_assistant_event_rejects_an_empty_parse(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(synthetic, "parse_events", lambda *lines: [])
    with pytest.raises(ValueError, match="AssistantEvent"):
        synthetic_assistant_event("on it")


def test_from_events_and_render_turn_accept_synthetic_events() -> None:
    activity = SessionActivity.from_events(
        SessionId("s1"),
        (
            synthetic_user_event("fix the bug", uuid="u1", session_id="s1"),
            synthetic_assistant_event("on it", uuid="a1", session_id="s1", secs=1, stop_reason="end_turn"),
        ),
    )
    assert render_turn(activity.turns[0], budget=Budget()) == "user: fix the bug\nassistant: on it"
