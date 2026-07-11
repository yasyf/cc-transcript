from __future__ import annotations

from dataclasses import FrozenInstanceError
from datetime import UTC, datetime

import pytest

from cc_transcript.ids import tool_digest
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    EntryMeta,
    EventUuid,
    ModeEvent,
    OtherEvent,
    OtherSystemDetail,
    QueuedCommand,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolUseBlock,
    ToolUseId,
    TranscriptEvent,
    UserEvent,
    thinking_chars,
    tool_uses,
)
from cc_transcript.tools import EditCall, ToolInputError

EDIT_INPUT = {"file_path": "a.py", "old_string": "x = 1", "new_string": "x = 2"}


def make_meta() -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid("u1"),
        parent_uuid=None,
        session_id=SessionId("s1"),
        timestamp=datetime(2026, 1, 1, tzinfo=UTC),
        cwd="/repo",
        git_branch="main",
        cc_version=None,
        is_sidechain=False,
        is_meta=False,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


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
    block = TextBlock("hi")
    with pytest.raises(FrozenInstanceError):
        block.text = "bye"  # type: ignore[misc]


def test_slots_have_no_instance_dict() -> None:
    block = ToolUseBlock(id=ToolUseId("t1"), name="Read", input={"file_path": "x"})
    assert not hasattr(block, "__dict__")


def test_event_is_frozen() -> None:
    event = UserEvent(meta=make_meta(), text="hello", blocks=(), interrupted=False)
    with pytest.raises(FrozenInstanceError):
        event.text = "changed"  # type: ignore[misc]


@pytest.mark.parametrize(
    ("event", "expected"),
    [
        pytest.param(UserEvent(meta=make_meta(), text="x", blocks=(), interrupted=False), "user", id="user"),
        pytest.param(
            AssistantEvent(meta=make_meta(), model="claude", text="x", blocks=(), stop_reason="end_turn", usage=None),
            "assistant",
            id="assistant",
        ),
        pytest.param(
            SystemEvent(meta=make_meta(), subtype="hook", content=None, level=None, detail=OtherSystemDetail(raw={})),
            "system",
            id="system",
        ),
        pytest.param(ModeEvent(session_id=SessionId("s1"), channel="mode", value="normal"), "mode", id="mode"),
        pytest.param(OtherEvent(type="summary", raw={"type": "summary"}), "other", id="other"),
        pytest.param(
            AttachmentEvent(
                meta=make_meta(), attachment_type="queued_command", detail=QueuedCommand(prompt="go")
            ),
            "attachment",
            id="attachment",
        ),
    ],
)
def test_match_narrows_union(event: TranscriptEvent, expected: str) -> None:
    assert event_kind(event) == expected


def test_mode_event_has_no_meta() -> None:
    assert not hasattr(ModeEvent(session_id=SessionId("s1"), channel="mode", value="normal"), "meta")


def test_tool_use_block_call_parses_to_edit_call() -> None:
    call = ToolUseBlock(id=ToolUseId("t1"), name="Edit", input=EDIT_INPUT).call
    assert isinstance(call, EditCall)
    assert (call.file_path, call.old, call.new) == ("a.py", "x = 1", "x = 2")
    assert call.raw == EDIT_INPUT


def test_tool_use_block_call_is_strict() -> None:
    with pytest.raises(ToolInputError):
        _ = ToolUseBlock(id=ToolUseId("t1"), name="Edit", input={"file_path": "a.py"}).call


def test_tool_use_block_digest_round_trips_through_call() -> None:
    block = ToolUseBlock(id=ToolUseId("t1"), name="Edit", input=EDIT_INPUT)
    assert block.digest == tool_digest("Edit", EDIT_INPUT)
    assert block.digest == block.call.digest


def test_tool_uses_keeps_only_tool_use_blocks_in_order() -> None:
    first = ToolUseBlock(id=ToolUseId("t1"), name="Read", input={"file_path": "x"})
    second = ToolUseBlock(id=ToolUseId("t2"), name="Bash", input={"command": "ls"})
    event = AssistantEvent(
        meta=make_meta(),
        model="claude",
        text="hi",
        blocks=(TextBlock("hi"), first, ThinkingBlock("hmm"), second),
        stop_reason=None,
        usage=None,
    )
    assert tool_uses(event) == (first, second)


def test_tool_uses_empty_for_no_tool_blocks() -> None:
    event = UserEvent(meta=make_meta(), text="hello", blocks=(TextBlock("hello"),), interrupted=False)
    assert tool_uses(event) == ()


def test_thinking_chars_sums_thinking_block_lengths() -> None:
    event = AssistantEvent(
        meta=make_meta(),
        model="claude",
        text="",
        blocks=(ThinkingBlock("abc"), TextBlock("ignored"), ThinkingBlock("de")),
        stop_reason=None,
        usage=None,
    )
    assert thinking_chars(event) == 5


def test_thinking_chars_zero_without_thinking() -> None:
    event = UserEvent(meta=make_meta(), text="hello", blocks=(TextBlock("hello"),), interrupted=False)
    assert thinking_chars(event) == 0
