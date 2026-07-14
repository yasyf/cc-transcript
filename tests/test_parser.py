from __future__ import annotations

from pathlib import Path
from typing import Any

import anyio
import orjson

from cc_transcript.models import (
    CacheCreation,
    McpServer,
    ModelUsage,
    ServerToolUse,
    SessionId,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
)
from cc_transcript.parser import parse_events_async, parse_events_from_bytes, parse_print_result
from tests.support import raw_envelope as envelope

TESTDATA = Path(__file__).parent / "testdata"


def user_str() -> dict[str, Any]:
    return envelope(type="user", message={"role": "user", "content": "  fix the bug  "})


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


def mode_entry() -> dict[str, Any]:
    return {"type": "mode", "mode": "normal", "sessionId": "sess-1"}


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


def test_parse_events_async_reads_file(tmp_path: Path) -> None:
    path = tmp_path / "t.jsonl"
    path.write_bytes(b"\n".join([orjson.dumps(user_str()), orjson.dumps(mode_entry())]))
    events = anyio.run(parse_events_async, path)
    assert [type(e).__name__ for e in events] == ["UserEvent", "ModeEvent"]


def test_parse_print_result_haiku_envelope() -> None:
    env = parse_print_result((TESTDATA / "haiku_envelope.json").read_bytes())

    assert env.total_cost_usd == 0.05759
    usage = env.model_usage["claude-haiku-4-5-20251001"]
    assert isinstance(usage, ModelUsage)
    assert usage.input_tokens == 28
    assert usage.output_tokens == 250
    assert usage.cache_read_input_tokens == 51020
    assert usage.cache_creation_input_tokens == 25605
    assert usage.web_search_requests == 0
    assert usage.cost_usd == 0.05759
    assert usage.context_window == 200000
    assert usage.max_output_tokens == 32000

    assert env.usage.input_tokens == 28
    assert env.usage.output_tokens == 250
    assert env.usage.cache_read_input_tokens == 51020
    assert env.usage.cache_creation_input_tokens == 25605
    cache_creation = env.usage.cache_creation
    assert isinstance(cache_creation, CacheCreation)
    assert cache_creation.ephemeral_5m_input_tokens == 0
    assert cache_creation.ephemeral_1h_input_tokens == 25605
    assert env.usage.service_tier == "standard"
    assert env.usage.inference_geo == "not_available"
    server_tool_use = env.usage.server_tool_use
    assert isinstance(server_tool_use, ServerToolUse)
    assert server_tool_use.web_search_requests == 0
    assert server_tool_use.web_fetch_requests == 0

    assert env.structured_output == {"answer": "pong"}
    assert env.num_turns == 3
    assert env.is_error is False
    assert env.result == "pong"
    assert env.stop_reason == "end_turn"
    assert env.session_id == SessionId("01fb78f3-fc93-4eb1-b399-07e6a60c3e2c")
    assert env.fast_mode_state == "off"
    assert env.permission_denials == ()

    assert env.init is not None
    assert any(
        isinstance(server, McpServer) and server.name == "plugin:cc-review:cc-review" and server.status == "connected"
        for server in env.init.mcp_servers
    )
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
        message.role == "assistant"
        and any(isinstance(block, TextBlock) and block.text == "pong" for block in message.blocks)
        for message in env.messages
    )
    assert any(message.model == "claude-haiku-4-5-20251001" for message in env.messages)
