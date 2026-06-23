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
    CacheCreation,
    EventUuid,
    FallbackBlock,
    McpServer,
    ModeEvent,
    ModelUsage,
    OtherBlock,
    OtherEvent,
    ServerToolUse,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    Usage,
    UserEvent,
)
from cc_transcript.parser import build_event, parse_events_async, parse_events_from_bytes, parse_print_result

TESTDATA = Path(__file__).parent / "testdata"


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


def assistant_with_usage() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "done"}],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 25437,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 25437},
                "service_tier": "standard",
                "inference_geo": "not_available",
                "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
            },
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


def assistant_fallback() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": "tool_use",
            "content": [{"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"}}],
        },
    )


def assistant_unknown_block() -> dict[str, Any]:
    return envelope(
        type="assistant",
        message={
            "role": "assistant",
            "model": "claude-opus-4-8",
            "stop_reason": None,
            "content": [{"type": "future_block", "payload": {"n": 1}}],
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
    assert event.meta.uuid == EventUuid("uuid-1")
    assert event.meta.parent_uuid == EventUuid("uuid-0")
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


def test_assistant_usage_parses_cache_creation_split() -> None:
    event = build_event(assistant_with_usage())
    assert isinstance(event, AssistantEvent)
    assert event.usage == Usage(
        input_tokens=10,
        output_tokens=7,
        cache_read_input_tokens=0,
        cache_creation_input_tokens=25437,
        cache_creation=CacheCreation(ephemeral_5m_input_tokens=0, ephemeral_1h_input_tokens=25437),
        service_tier="standard",
        inference_geo="not_available",
        server_tool_use=ServerToolUse(web_search_requests=0, web_fetch_requests=0),
    )


def test_assistant_without_usage_is_none() -> None:
    event = build_event(assistant_text())
    assert isinstance(event, AssistantEvent)
    assert "usage" not in assistant_text()["message"]
    assert event.usage is None


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


def test_assistant_fallback_block() -> None:
    event = build_event(assistant_fallback())
    assert isinstance(event, AssistantEvent)
    assert event.text == ""
    assert event.blocks == (FallbackBlock(from_model="claude-fable-5", to_model="claude-opus-4-8"),)


def test_assistant_unknown_block_degrades_to_other() -> None:
    event = build_event(assistant_unknown_block())
    assert isinstance(event, AssistantEvent)
    assert event.blocks == (OtherBlock(type="future_block", raw={"type": "future_block", "payload": {"n": 1}}),)


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


def test_parse_print_result_haiku_envelope() -> None:
    env = parse_print_result((TESTDATA / "haiku_envelope.json").read_bytes())

    assert env.total_cost_usd == 0.05759
    assert env.model_usage["claude-haiku-4-5-20251001"] == ModelUsage(
        input_tokens=28,
        output_tokens=250,
        cache_read_input_tokens=51020,
        cache_creation_input_tokens=25605,
        web_search_requests=0,
        cost_usd=0.05759,
        context_window=200000,
        max_output_tokens=32000,
    )

    assert env.usage.input_tokens == 28
    assert env.usage.output_tokens == 250
    assert env.usage.cache_read_input_tokens == 51020
    assert env.usage.cache_creation_input_tokens == 25605
    assert env.usage.cache_creation == CacheCreation(ephemeral_5m_input_tokens=0, ephemeral_1h_input_tokens=25605)
    assert env.usage.service_tier == "standard"
    assert env.usage.inference_geo == "not_available"
    assert env.usage.server_tool_use == ServerToolUse(web_search_requests=0, web_fetch_requests=0)

    assert env.structured_output == {"answer": "pong"}
    assert env.num_turns == 3
    assert env.is_error is False
    assert env.result == "pong"
    assert env.stop_reason == "end_turn"
    assert env.session_id == SessionId("01fb78f3-fc93-4eb1-b399-07e6a60c3e2c")
    assert env.fast_mode_state == "off"
    assert env.permission_denials == ()

    assert env.init is not None
    assert McpServer(name="plugin:cc-review:cc-review", status="connected") in env.init.mcp_servers
    assert "Bash" in env.init.tools and "Read" in env.init.tools
    assert "deep-research" in env.init.skills
    assert any(p.name == "cc-review" for p in env.init.plugins)

    assert any(
        isinstance(block, ToolUseBlock) and block.name == "StructuredOutput" and block.input == {"answer": "pong"}
        for message in env.messages
        for block in message.blocks
    )
    assert any(
        isinstance(block, ToolResultBlock) and block.content == "Structured output provided successfully"
        for message in env.messages
        for block in message.blocks
    )
    assert any(
        message.role == "assistant" and TextBlock("pong") in message.blocks for message in env.messages
    )
    assert any(message.model == "claude-haiku-4-5-20251001" for message in env.messages)
