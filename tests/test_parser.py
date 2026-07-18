from __future__ import annotations

from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.ids import EventUuid
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    CacheCreation,
    McpServer,
    ModeEvent,
    ModelUsage,
    OtherEvent,
    ServerToolUse,
    SessionId,
    SystemEvent,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    Usage,
    UserEvent,
)
from cc_transcript.parser import parse, parse_event, parse_events_from_bytes, parse_print_result, stream
from tests import testkit
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


def test_parse_reads_file_and_carries_path(tmp_path: Path) -> None:
    path = tmp_path / "t.jsonl"
    path.write_bytes(b"\n".join([orjson.dumps(user_str()), orjson.dumps(mode_entry())]))
    transcript = parse(path)
    assert [type(e).__name__ for e in transcript.events] == ["UserEvent", "ModeEvent"]
    assert transcript.path == path
    assert transcript.mtime == path.stat().st_mtime


def test_parse_bytes_source_has_no_path() -> None:
    transcript = parse(orjson.dumps(user_str()))
    assert transcript.path is None
    assert [type(e).__name__ for e in transcript.events] == ["UserEvent"]


def test_parse_missing_path_raises() -> None:
    with pytest.raises(OSError):
        parse(Path("/nonexistent/never/t.jsonl"))


def test_stream_skips_a_pruned_file_and_parses_the_rest(tmp_path: Path) -> None:
    healthy = [tmp_path / "a.jsonl", tmp_path / "b.jsonl"]
    for path in healthy:
        path.write_bytes(orjson.dumps(user_str()) + b"\n")
    parsed = list(stream([healthy[0], tmp_path / "pruned.jsonl", healthy[1]]))
    assert sorted(t.path for t in parsed) == healthy
    assert all(len(t.events) == 1 for t in parsed)


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

    first_assistant = next(m for m in env.messages if m.role == "assistant")
    assert first_assistant.id == "msg_01AmGLcqfEDeNty3QMFWQQEw"
    message_usage = first_assistant.usage
    assert isinstance(message_usage, Usage)
    assert message_usage.input_tokens == 10
    assert message_usage.output_tokens == 7
    assert message_usage.cache_creation_input_tokens == 25437
    assert message_usage.cache_read_input_tokens == 0
    assert isinstance(message_usage.cache_creation, CacheCreation)
    assert message_usage.cache_creation.ephemeral_1h_input_tokens == 25437
    assert message_usage.service_tier == "standard"
    assert message_usage.inference_geo == "not_available"
    first_user = next(m for m in env.messages if m.role == "user")
    assert first_user.id is None
    assert first_user.usage is None


def test_print_message_without_id_or_usage_yields_none() -> None:
    raw = (
        b'[{"type":"assistant","session_id":"s",'
        b'"message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}},'
        b'{"type":"result","total_cost_usd":0.0,"modelUsage":{},'
        b'"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,'
        b'"cache_creation_input_tokens":0},"num_turns":1,"is_error":false,'
        b'"session_id":"s","permission_denials":[]}]'
    )
    message = parse_print_result(raw).messages[0]
    assert message.role == "assistant"
    assert message.id is None
    assert message.usage is None


@pytest.mark.parametrize(
    ("line", "cls"),
    [
        pytest.param(testkit.user_line("u1", "hi"), UserEvent, id="user"),
        pytest.param(testkit.assistant_line("a1", "hi", stop_reason="end_turn"), AssistantEvent, id="assistant"),
        pytest.param(testkit.system_line("stop_hook_summary"), SystemEvent, id="system"),
        pytest.param(testkit.mode_line("normal", session_id="s1"), ModeEvent, id="mode"),
        pytest.param(
            testkit.mode_line("plan", session_id="s1", channel="permission-mode"), ModeEvent, id="permission-mode"
        ),
        pytest.param(
            {"type": "attachment", "attachment": {"type": "queued_command", "prompt": "go"}} | testkit.meta_fields("att"),
            AttachmentEvent,
            id="attachment",
        ),
        pytest.param(testkit.other_line("summary"), OtherEvent, id="other"),
    ],
)
def test_parse_event_returns_view_per_type(line: dict[str, Any], cls: type) -> None:
    assert isinstance(parse_event(line), cls)


def test_parse_event_lifts_user_fields() -> None:
    # Independent oracle: hand-written expected values, not derived from the parser.
    event = parse_event(testkit.user_line("uuid-1", "fix the bug"))
    assert isinstance(event, UserEvent)
    assert event.text == "fix the bug"
    assert event.meta.uuid == EventUuid("uuid-1")


def test_parse_event_accepts_a_non_dict_mapping() -> None:
    from types import MappingProxyType

    event = parse_event(MappingProxyType(testkit.user_line("m1", "hi")))
    assert isinstance(event, UserEvent)
    assert event.text == "hi"


def test_parse_event_missing_type_raises() -> None:
    with pytest.raises(KeyError):
        parse_event({"foo": "bar"})


def test_parse_event_malformed_user_raises() -> None:
    with pytest.raises((KeyError, ValueError)):
        parse_event({"type": "user"} | testkit.meta_fields("u1"))


def test_parse_event_drops_below_minyear_timestamp_as_none() -> None:
    # The tolerant native parser drops a below-Python-MINYEAR timestamp; parse_event
    # surfaces that as None (v14 divergence: the old Python parser raised ValueError).
    line = testkit.user_line("u1", "hi") | {"timestamp": "0000-01-01T00:00:00+00:00"}
    assert parse_event(line) is None


# v14 accepted divergences below: the native backend differs from the old orjson parser
# per case (root dup keys first-wins per task e0ab2411 item 9).
def test_duplicate_root_type_is_first_wins() -> None:
    # old (orjson last-wins): AssistantEvent; native (first-wins): UserEvent.
    dup = (
        b'{"type":"user","type":"assistant","uuid":"x","sessionId":"s",'
        b'"timestamp":"2026-01-01T00:00:00+00:00","isSidechain":false,'
        b'"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}'
    )
    assert [type(e).__name__ for e in parse_events_from_bytes(dup)] == ["UserEvent"]


def test_year_zero_event_is_silently_dropped() -> None:
    # old public bytes parser raised ValueError (fromisoformat rejects year 0);
    # native tolerant parser drops the line and keeps the file.
    line = orjson.dumps(
        {"type": "user", "message": {"role": "user", "content": "hi"}}
        | {"uuid": "x", "sessionId": "s", "isSidechain": False, "timestamp": "0000-01-01T00:00:00+00:00"}
    )
    assert parse_events_from_bytes(line) == []


def test_non_string_mode_value_is_fail_fast() -> None:
    # A non-string mode value raises KeyError. Unreachable from a well-typed transcript
    # (every mode value is a string; the corpus carries zero), so pinned as fail-fast.
    with pytest.raises(KeyError):
        parse_events_from_bytes(orjson.dumps({"type": "mode", "sessionId": "s", "mode": 123}))


def test_duplicate_print_total_cost_usd_is_first_wins() -> None:
    # old (orjson last-wins): 2.0; native (first-wins): 1.0.
    raw = (
        b'[{"type":"result","total_cost_usd":1.0,"total_cost_usd":2.0,"modelUsage":{},'
        b'"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,'
        b'"cache_creation_input_tokens":0},"num_turns":1,"is_error":false,'
        b'"session_id":"s","permission_denials":[]}]'
    )
    assert parse_print_result(raw).total_cost_usd == 1.0


def test_invalid_print_json_raises_value_error() -> None:
    # old: orjson.JSONDecodeError; native: plain ValueError.
    with pytest.raises(ValueError):
        parse_print_result(b"{not json")


def test_empty_print_envelope_raises_value_error() -> None:
    # old: StopIteration (next() over no result element); native: ValueError.
    with pytest.raises(ValueError):
        parse_print_result(b"[]")
