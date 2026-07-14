"""Rust ↔ Python parity for the typed tool-call hierarchy over the frozen golden.

Each golden case carries the raw ``(tool, input)`` / ``(tool, payload)`` and the
Python reference projection (``cls``, every field including ``raw``, and the derived
``questions``). Every case asserts twice: the Python reference still projects to the
frozen value (a drift guard on ``tools.py``), and the Rust ``toolcall_parse`` /
``toolresult_parse`` lift produces the identical dict. ``raise_calls`` pins the strict
``ToolInputError`` type + message across both backends, and the probe test guards the
MCP-alias closure in ``matches_names``. Regenerate with ``scripts/gen_toolcall_golden.py``.
"""

from __future__ import annotations

import json
from pathlib import Path

import orjson
import pytest

from cc_transcript import _parser_rs
from cc_transcript.tools import ToolInputError, parse_tool_call, parse_tool_result
from tests.support import requires_rust
from tests.toolcall_cases import project

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "toolcall_golden.json").read_text("utf-8"))
CALLS = GOLDEN["calls"]
RESULTS = GOLDEN["results"]
RAISE_CALLS = GOLDEN["raise_calls"]
DUP_KEY_CALLS = GOLDEN["dup_key_calls"]
DUP_KEY_RESULTS = GOLDEN["dup_key_results"]


@requires_rust
@pytest.mark.parametrize("case", CALLS, ids=[f"{i}:{c['tool']}" for i, c in enumerate(CALLS)])
def test_call_parity(case: dict[str, object]) -> None:
    name, tool_input, expected = case["tool"], case["input"], case["expected"]
    assert project(parse_tool_call(name, tool_input, on_error="other")) == expected
    assert _parser_rs.toolcall_parse(name, json.dumps(tool_input), "other") == expected


@requires_rust
@pytest.mark.parametrize("case", RESULTS, ids=[f"{i}:{c['tool']}" for i, c in enumerate(RESULTS)])
def test_result_parity(case: dict[str, object]) -> None:
    name, payload, expected = case["tool"], case["payload"], case["expected"]
    assert project(parse_tool_result(name, payload, on_error="other")) == expected
    assert _parser_rs.toolresult_parse(name, json.dumps(payload)) == expected


@requires_rust
@pytest.mark.parametrize("case", RAISE_CALLS, ids=[f"{i}:{c['tool']}" for i, c in enumerate(RAISE_CALLS)])
def test_strict_raise_parity(case: dict[str, object]) -> None:
    name, tool_input, message = case["tool"], case["input"], case["message"]
    with pytest.raises(ToolInputError) as py_exc:
        parse_tool_call(name, tool_input, on_error="raise")
    assert str(py_exc.value) == message
    with pytest.raises(ToolInputError) as rs_exc:
        _parser_rs.toolcall_parse(name, json.dumps(tool_input), "raise")
    assert str(rs_exc.value) == message


@requires_rust
@pytest.mark.parametrize("case", DUP_KEY_CALLS, ids=[f"{i}:{c['tool']}" for i, c in enumerate(DUP_KEY_CALLS)])
def test_dup_key_call_parity(case: dict[str, object]) -> None:
    name, input_json, expected = case["tool"], case["input_json"], case["expected"]
    assert project(parse_tool_call(name, orjson.loads(input_json), on_error="other")) == expected
    assert _parser_rs.toolcall_parse(name, input_json, "other") == expected


@requires_rust
@pytest.mark.parametrize("case", DUP_KEY_RESULTS, ids=[f"{i}:{c['tool']}" for i, c in enumerate(DUP_KEY_RESULTS)])
def test_dup_key_result_parity(case: dict[str, object]) -> None:
    name, payload_json, expected = case["tool"], case["payload_json"], case["expected"]
    assert project(parse_tool_result(name, orjson.loads(payload_json), on_error="other")) == expected
    assert _parser_rs.toolresult_parse(name, payload_json) == expected


def write_transcript(path: Path, tool_name: str) -> None:
    lines = [
        {
            "type": "user",
            "uuid": "u1",
            "sessionId": "s",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": "please make the edit"},
        },
        {
            "type": "assistant",
            "uuid": "a1",
            "parentUuid": "u1",
            "sessionId": "s",
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": "tu1", "name": tool_name, "input": {"file": "/a"}}],
            },
        },
    ]
    path.write_bytes(b"\n".join(orjson.dumps(line) for line in lines))


@requires_rust
def test_probe_matches_mcp_alias_waiting_tool(tmp_path: Path) -> None:
    """A pending ``mcp__…__ccx_code_edit`` matches ``waiting_tools={"Edit"}`` via the alias."""
    path = tmp_path / "session.jsonl"
    write_transcript(path, "mcp__cc-context__ccx_code_edit")
    matched = _parser_rs.session_activity_probe(str(path), ["Edit"])
    assert matched["is_waiting"] is True
    assert [(p["name"], p["kind"]) for p in matched["pending"]] == [
        ("mcp__cc-context__ccx_code_edit", "waiting_tool")
    ]
    # A non-aliased waiting set does not treat the same tool as a waiting tool.
    unmatched = _parser_rs.session_activity_probe(str(path), ["Grep"])
    assert [p["kind"] for p in unmatched["pending"]] == ["mid_tool"]
