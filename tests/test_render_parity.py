"""Rust ↔ Python parity for the renderer over the frozen golden.

Each case asserts twice: the Python ``cc_transcript.render`` reference still produces the
frozen value (a drift guard), and the Rust ``_native`` port produces the identical
string. ``tool_calls`` pins ``render_tool_call`` under a budget; ``transcripts`` pins
``compact_line``/``haystack``/``render_stats`` over embedded raw JSONL. Regenerate with
``scripts/gen_render_golden.py``.
"""

from __future__ import annotations

import base64
import json
from pathlib import Path

import pytest

from cc_transcript import _native
from cc_transcript.backend import ParsedTranscript
from cc_transcript.filterspec import tool_names
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.render import Budget, collect_stats, compact_line, haystack, render_stats, render_tool_call
from cc_transcript.tools import parse_tool_call
from tests.support import requires_rust

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "render_golden.json").read_text("utf-8"))
TOOL_CALLS = GOLDEN["tool_calls"]
DUP_KEY_CALLS = GOLDEN["dup_key_calls"]
RAW_JSON_CALLS = GOLDEN["raw_json_calls"]
TRANSCRIPTS = GOLDEN["transcripts"]


def parsed_of(raw: bytes, id_: str) -> ParsedTranscript:
    return ParsedTranscript(path=Path(id_), mtime=0.0, events=tuple(parse_events_from_bytes(raw)))


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
    events = parse_events_from_bytes(raw)
    names = tool_names(events)
    for combo in tc["compact"]:
        width, thinking, uuids = combo["width"], combo["thinking"], combo["uuids"]
        py = [compact_line(i, event, names=names, width=width, thinking=thinking, uuids=uuids) for i, event in enumerate(events)]
        assert py == combo["lines"]
        assert _native.render_compact_lines(raw, width, thinking, uuids) == combo["lines"]
    for combo in tc["haystack"]:
        wheres = combo["where"]
        py = [haystack(event, where=frozenset(wheres)) for event in events]
        assert py == combo["lines"]
        assert _native.render_haystacks(raw, wheres) == combo["lines"]
    assert render_stats(collect_stats([parsed_of(raw, tc["id"])])) == tc["stats"]
    assert _native.render_stats([raw]) == tc["stats"]


@requires_rust
def test_stats_all_parity() -> None:
    raws = [base64.b64decode(tc["jsonl_b64"]) for tc in TRANSCRIPTS]
    assert render_stats(collect_stats([parsed_of(raw, tc["id"]) for raw, tc in zip(raws, TRANSCRIPTS)])) == GOLDEN["stats_all"]
    assert _native.render_stats(raws) == GOLDEN["stats_all"]
