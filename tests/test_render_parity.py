"""Rust ↔ Python parity for the renderer over the frozen golden.

``render_tool_call`` stays a Python object renderer, so each of its cases asserts twice:
the Python ``cc_transcript.render`` reference still produces the frozen value (a drift
guard), and the Rust ``_native`` port produces the identical string. The raw-path
renderers — ``render_compact_lines``/``render_haystacks``/``render_stats`` over embedded
raw JSONL — live only in the native core, so they pin native-vs-golden. Regenerate with
``scripts/gen_render_golden.py``.
"""

from __future__ import annotations

import base64
import json
from pathlib import Path

import pytest

from cc_transcript import _native
from cc_transcript.render import Budget, render_tool_call
from cc_transcript.tools import parse_tool_call
from tests.support import requires_rust

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "render_golden.json").read_text("utf-8"))
TOOL_CALLS = GOLDEN["tool_calls"]
DUP_KEY_CALLS = GOLDEN["dup_key_calls"]
RAW_JSON_CALLS = GOLDEN["raw_json_calls"]
TRANSCRIPTS = GOLDEN["transcripts"]


@requires_rust
@pytest.mark.parametrize(
    "case", TOOL_CALLS, ids=[f"{i}:{c['name']}:{c['budget'][1]}" for i, c in enumerate(TOOL_CALLS)]
)
def test_tool_call_parity(case: dict[str, object]) -> None:
    name, input_json = case["name"], case["input_json"]
    turn, tool = case["budget"]
    expected = case["expected"]
    call = parse_tool_call(name, json.loads(input_json), on_error="other")
    assert render_tool_call(call, budget=Budget(turn_chars=turn, tool_chars=tool)) == expected
    assert _native.render_tool_call(name, input_json, turn, tool) == expected


@requires_rust
@pytest.mark.parametrize(
    "case", DUP_KEY_CALLS, ids=[f"{i}:{c['name']}:{c['budget'][1]}" for i, c in enumerate(DUP_KEY_CALLS)]
)
def test_dup_key_call_parity(case: dict[str, object]) -> None:
    name, input_json = case["name"], case["input_json"]
    turn, tool = case["budget"]
    expected = case["expected"]
    call = parse_tool_call(name, json.loads(input_json), on_error="other")
    assert render_tool_call(call, budget=Budget(turn_chars=turn, tool_chars=tool)) == expected
    assert _native.render_tool_call(name, input_json, turn, tool) == expected


@requires_rust
@pytest.mark.parametrize(
    "case", RAW_JSON_CALLS, ids=[f"{i}:{c['name']}:{c['budget'][1]}" for i, c in enumerate(RAW_JSON_CALLS)]
)
def test_raw_json_call_parity(case: dict[str, object]) -> None:
    name, input_json = case["name"], case["input_json"]
    turn, tool = case["budget"]
    expected = case["expected"]
    call = parse_tool_call(name, json.loads(input_json), on_error="other")
    assert render_tool_call(call, budget=Budget(turn_chars=turn, tool_chars=tool)) == expected
    assert _native.render_tool_call(name, input_json, turn, tool) == expected


@requires_rust
@pytest.mark.parametrize("tc", TRANSCRIPTS, ids=[t["id"] for t in TRANSCRIPTS])
def test_transcript_parity(tc: dict[str, object]) -> None:
    raw = base64.b64decode(tc["jsonl_b64"])
    for combo in tc["compact"]:
        width, thinking, uuids = combo["width"], combo["thinking"], combo["uuids"]
        assert _native.render_compact_lines(raw, width, thinking, uuids) == combo["lines"]
    for combo in tc["haystack"]:
        assert _native.render_haystacks(raw, combo["where"]) == combo["lines"]
    assert _native.render_stats([raw]) == tc["stats"]


@requires_rust
def test_stats_all_parity() -> None:
    raws = [base64.b64decode(tc["jsonl_b64"]) for tc in TRANSCRIPTS]
    assert _native.render_stats(raws) == GOLDEN["stats_all"]
