from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import anyio
import orjson
import pytest

from cc_transcript import parse_event
from cc_transcript.models import (
    AssistantEvent,
    EntryUuid,
    ModeEvent,
    OtherEvent,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    UserEvent,
)
from cc_transcript.parser import build_event, parse_events_async, parse_events_from_bytes


def envelope(**overrides: Any) -> dict[str, Any]:
    return {
        "uuid": "uuid-1",
        "parentUuid": "uuid-0",
        "sessionId": "sess-1",
        "timestamp": "2026-01-02T03:04:05.000Z",
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "1.2.3",
        "isSidechain": False,
        "entrypoint": "cli",
    } | overrides


def user_str() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "  fix the bug  "})


def user_blocks() -> dict[str, Any]:
    return envelope(
        type="user",
        message={
            "role": "user",
            "content": [
                {"type": "text", "text": "here is context"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok output", "is_error": False},
            ],
        },
    )


def user_tool_result_list_content() -> dict[str, Any]:
    return envelope(
        type="user",
        message={
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_2",
                    "content": [{"type": "text", "text": "line a"}, {"type": "text", "text": "line b"}],
                    "is_error": True,
                }
            ],
        },
    )


def user_async_tool_result() -> dict[str, Any]:
    return envelope(
        type="user",
        toolUseResult={"isAsync": True, "status": "async_launched"},
        message={
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "toolu_async", "content": "launched", "is_error": False}
            ],
        },
    )


def user_sidechain() -> dict[str, Any]:
    return envelope(type="user", isSidechain=True, message={"role": "user", "content": "subagent prompt"})


def user_meta() -> dict[str, Any]:
    return envelope(type="user", isMeta=True, message={"role": "user", "content": "meta note"})


def user_interrupt() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "[Request interrupted by user]"})


def assistant_text() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "done"}],
        },
    )


def assistant_thinking_tool() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "tool_use", "id": "toolu_9", "name": "Read", "input": {"file_path": "/x"}},
            ],
        },
    )


def assistant_synthetic() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "<synthetic>",
            "stop_reason": None,
            "content": [{"type": "text", "text": "noop"}],
        },
    )


def system_entry() -> dict[str, Any]:
    return envelope(type="system", subtype="stop_hook_summary", content="hook ran")


def mode_entry() -> dict[str, Any]:
    return {"type": "mode", "mode": "normal", "sessionId": "sess-1"}


def permission_mode_entry() -> dict[str, Any]:
    return {"type": "permission-mode", "permissionMode": "bypassPermissions", "sessionId": "sess-1"}


def summary_entry() -> dict[str, Any]:
    return {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x"}


def attachment_entry() -> dict[str, Any]:
    return envelope(type="attachment", attachment={"kind": "file"})


def queue_operation_entry() -> dict[str, Any]:
    return {"type": "queue-operation", "operation": "enqueue"}


def test_user_str_content() -> None:
    event = build_event(user_str())
    assert isinstance(event, UserEvent)
    assert event.text == "  fix the bug  "
    assert event.blocks == ()
    assert event.interrupted is False
    assert event.meta.uuid == EntryUuid("uuid-1")
    assert event.meta.parent_uuid == EntryUuid("uuid-0")
    assert event.meta.session_id == SessionId("sess-1")
    assert event.meta.timestamp == datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)
    assert event.meta.cwd == "/repo"
    assert event.meta.git_branch == "main"
    assert event.meta.cc_version == "1.2.3"
    assert event.meta.entrypoint == "cli"


def test_user_block_content() -> None:
    event = build_event(user_blocks())
    assert isinstance(event, UserEvent)
    assert event.text == "here is context"
    assert event.blocks == (
        TextBlock("here is context"),
        ToolResultBlock(tool_use_id=ToolUseId("toolu_1"), content="ok output", is_error=False),
    )


def test_tool_result_list_content_flattens_and_errors() -> None:
    event = build_event(user_tool_result_list_content())
    assert isinstance(event, UserEvent)
    assert event.text == ""
    assert event.blocks == (ToolResultBlock(tool_use_id=ToolUseId("toolu_2"), content="line aline b", is_error=True),)


def test_tool_result_is_async_from_entry_level_flag() -> None:
    event = parse_event(user_async_tool_result())
    assert isinstance(event, UserEvent)
    assert event.blocks == (
        ToolResultBlock(tool_use_id=ToolUseId("toolu_async"), content="launched", is_error=False, is_async=True),
    )


def test_tool_result_is_async_defaults_false() -> None:
    event = parse_event(user_blocks())
    assert isinstance(event, UserEvent)
    (block,) = (b for b in event.blocks if isinstance(b, ToolResultBlock))
    assert block.is_async is False


def test_parse_event_is_public_alias_of_build_event() -> None:
    assert parse_event is build_event


def test_user_sidechain_meta_flags() -> None:
    event = build_event(user_sidechain())
    assert isinstance(event, UserEvent)
    assert event.meta.is_sidechain is True
    meta_event = build_event(user_meta())
    assert isinstance(meta_event, UserEvent)
    assert meta_event.meta.is_meta is True


def test_user_interrupt() -> None:
    event = build_event(user_interrupt())
    assert isinstance(event, UserEvent)
    assert event.interrupted is True


def test_assistant_text() -> None:
    event = build_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert event.model == "claude-opus-4-7"
    assert event.text == "done"
    assert event.stop_reason == "end_turn"
    assert event.blocks == (TextBlock("done"),)


def test_assistant_thinking_and_tool_use() -> None:
    event = build_event(assistant_thinking_tool())
    assert isinstance(event, AssistantEvent)
    assert event.text == ""
    assert event.stop_reason == "tool_use"
    assert event.blocks == (
        ThinkingBlock("let me think"),
        ToolUseBlock(id=ToolUseId("toolu_9"), name="Read", input={"file_path": "/x"}),
    )


def test_assistant_synthetic_is_not_filtered() -> None:
    event = build_event(assistant_synthetic())
    assert isinstance(event, AssistantEvent)
    assert event.model == "<synthetic>"
    assert event.stop_reason is None


def test_system_entry() -> None:
    event = build_event(system_entry())
    assert isinstance(event, SystemEvent)
    assert event.subtype == "stop_hook_summary"
    assert event.content == "hook ran"


def test_mode_entry() -> None:
    event = build_event(mode_entry())
    assert event == ModeEvent(session_id=SessionId("sess-1"), channel="mode", value="normal")


def test_permission_mode_entry() -> None:
    event = build_event(permission_mode_entry())
    assert event == ModeEvent(session_id=SessionId("sess-1"), channel="permission-mode", value="bypassPermissions")


@pytest.mark.parametrize(
    ("data", "expected_type"),
    [
        pytest.param(summary_entry(), "summary", id="summary"),
        pytest.param(attachment_entry(), "attachment", id="attachment"),
        pytest.param(queue_operation_entry(), "queue-operation", id="queue-operation"),
    ],
)
def test_other_events(data: dict[str, Any], expected_type: str) -> None:
    event = build_event(data)
    assert isinstance(event, OtherEvent)
    assert event.type == expected_type
    assert event.raw == data


def test_parse_events_from_bytes_skips_blank_and_undecodable() -> None:
    raw = b"\n".join(
        [
            orjson.dumps(user_str()),
            b"",
            b"   ",
            b"{not valid json",
            orjson.dumps(assistant_text()),
        ]
    )
    events = parse_events_from_bytes(raw)
    assert [type(e).__name__ for e in events] == ["UserEvent", "AssistantEvent"]


def test_parse_events_from_bytes_skips_non_dict_json() -> None:
    raw = b"\n".join([b"null", b"42", b'"text"', b"[1, 2]", orjson.dumps(user_str())])
    events = parse_events_from_bytes(raw)
    assert [type(e).__name__ for e in events] == ["UserEvent"]


def test_unexpected_user_content_shape_raises_value_error() -> None:
    with pytest.raises(ValueError, match="unexpected user content shape: NoneType"):
        build_event(envelope(type="user", message={"role": "user", "content": None}))


def test_parse_events_async_reads_file(tmp_path: Path) -> None:
    path = tmp_path / "t.jsonl"
    path.write_bytes(b"\n".join([orjson.dumps(user_str()), orjson.dumps(mode_entry())]))
    events = anyio.run(parse_events_async, path)
    assert [type(e).__name__ for e in events] == ["UserEvent", "ModeEvent"]
