from __future__ import annotations

from typing import Any

import pytest

from cc_transcript.ids import ToolUseId, tool_digest
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    ContentBlock,
    DeferredToolsDelta,
    ModeEvent,
    OtherEvent,
    SystemEvent,
    ToolResultBlock,
    TranscriptEvent,
    UserEvent,
    thinking_chars,
    tool_uses,
)
from cc_transcript.tools import EditCall, ToolInputError
from tests import support, testkit

EDIT_INPUT = {"file_path": "a.py", "old_string": "x = 1", "new_string": "x = 2"}


def block(content: dict[str, Any]) -> ContentBlock:
    return support.assistant("a", "", blocks=(content,)).blocks[0]


def event_kind(event: TranscriptEvent) -> str:
    match event:
        case UserEvent():
            return "user"
        case AssistantEvent():
            return "assistant"
        case SystemEvent():
            return "system"
        case ModeEvent():
            return "mode"
        case OtherEvent():
            return "other"
        case AttachmentEvent():
            return "attachment"


def test_blocks_are_frozen() -> None:
    text = block(testkit.text_block("hi"))
    with pytest.raises(AttributeError):
        text.text = "bye"  # type: ignore[misc]


def test_slots_have_no_instance_dict() -> None:
    assert not hasattr(block(testkit.tool_use("t1", "Read", {"file_path": "x"})), "__dict__")


def test_event_is_frozen() -> None:
    event = support.user("u1", "hello")
    with pytest.raises(AttributeError):
        event.text = "changed"  # type: ignore[misc]


@pytest.mark.parametrize(
    ("event", "expected"),
    [
        pytest.param(support.user("u1", "x"), "user", id="user"),
        pytest.param(support.assistant("a1", "x", stop_reason="end_turn"), "assistant", id="assistant"),
        pytest.param(testkit.parse_event(testkit.system_line("hook")), "system", id="system"),
        pytest.param(testkit.parse_event(testkit.mode_line("normal", session_id="s1")), "mode", id="mode"),
        pytest.param(testkit.parse_event(testkit.other_line("summary")), "other", id="other"),
        pytest.param(
            testkit.parse_event(
                {"type": "attachment", "attachment": {"type": "queued_command", "prompt": "go"}}
                | testkit.meta_fields("att")
            ),
            "attachment",
            id="attachment",
        ),
    ],
)
def test_match_narrows_union(event: TranscriptEvent, expected: str) -> None:
    assert event_kind(event) == expected


def test_mode_event_has_no_meta() -> None:
    assert not hasattr(testkit.parse_event(testkit.mode_line("normal", session_id="s1")), "meta")


def deferred_tools_delta(extra: str) -> DeferredToolsDelta:
    event = testkit.parse_event(
        testkit.meta_fields("att")
        | {
            "type": "attachment",
            "attachment": {
                "type": "deferred_tools_delta",
                "addedNames": ["Read"],
                "removedNames": ["Bash"],
                "extra": extra,
            },
        }
    )
    assert isinstance(event, AttachmentEvent)
    assert isinstance(event.detail, DeferredToolsDelta)
    return event.detail


def test_deferred_tools_delta_equality_includes_raw_record() -> None:
    assert deferred_tools_delta("first") != deferred_tools_delta("second")


def test_deferred_tools_delta_remains_unhashable_with_raw_mapping() -> None:
    with pytest.raises(TypeError):
        hash(deferred_tools_delta("first"))


def test_tool_use_block_call_parses_to_edit_call() -> None:
    call = block(testkit.tool_use("t1", "Edit", EDIT_INPUT)).call
    assert isinstance(call, EditCall)
    assert (call.file_path, call.old, call.new) == ("a.py", "x = 1", "x = 2")
    assert call.raw == EDIT_INPUT


def test_tool_use_block_call_is_strict() -> None:
    with pytest.raises(ToolInputError):
        _ = block(testkit.tool_use("t1", "Edit", {"file_path": "a.py"})).call


def test_tool_use_block_digest_round_trips_through_call() -> None:
    blk = block(testkit.tool_use("t1", "Edit", EDIT_INPUT))
    assert blk.digest == tool_digest("Edit", EDIT_INPUT)
    assert blk.digest == blk.call.digest


def test_tool_uses_keeps_only_tool_use_blocks_in_order() -> None:
    event = support.assistant(
        "a",
        "",
        blocks=(
            testkit.text_block("hi"),
            testkit.tool_use("t1", "Read", {"file_path": "x"}),
            testkit.thinking_block("hmm"),
            testkit.tool_use("t2", "Bash", {"command": "ls"}),
        ),
    )
    assert tool_uses(event) == (event.blocks[1], event.blocks[3])


def test_tool_uses_empty_for_no_tool_blocks() -> None:
    event = support.user("u1", "", blocks=(testkit.text_block("hello"),))
    assert tool_uses(event) == ()


def test_tool_uses_field_values_independent_oracle() -> None:
    # Independent oracle: hand-written (id, name) pairs, not blocks re-read from the event.
    event = support.assistant(
        "a",
        "",
        blocks=(
            testkit.tool_use("t1", "Read", {"file_path": "x"}),
            testkit.text_block("mid"),
            testkit.tool_use("t2", "Bash", {"command": "ls"}),
        ),
    )
    assert [(block.id, block.name) for block in tool_uses(event)] == [("t1", "Read"), ("t2", "Bash")]


def test_tool_result_block_field_values_independent_oracle() -> None:
    # Independent oracle: hand-written field values, not an expected block from the parser.
    event = support.user("u1", "", blocks=(testkit.tool_result("t1", "the output", is_error=True),))
    block = event.blocks[0]
    assert isinstance(block, ToolResultBlock)
    assert block.tool_use_id == ToolUseId("t1")
    assert block.content == "the output"
    assert block.is_error is True
    assert block.is_async is False


def test_thinking_chars_sums_thinking_block_lengths() -> None:
    event = support.assistant(
        "a",
        "",
        blocks=(testkit.thinking_block("abc"), testkit.text_block("ignored"), testkit.thinking_block("de")),
    )
    assert thinking_chars(event) == 5


def test_thinking_chars_zero_without_thinking() -> None:
    assert thinking_chars(support.user("u1", "hello")) == 0
