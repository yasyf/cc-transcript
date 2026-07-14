"""Rust ↔ Python parity for durable context windows over the frozen golden.

Each case asserts twice: the Python ``cc_transcript.context`` reference still produces
the frozen value (a drift guard), and the Rust ``_parser_rs`` port produces the
identical bytes. ``captures`` pins ``capture_window(...).to_json()`` and
``render_preview`` over synthesized transcripts; ``windows`` pins the ``from_json`` →
``to_json`` round-trip and ``render_preview`` over hand-built windows; ``rejects`` pins
schema rejection. Regenerate with ``scripts/gen_context_golden.py``.
"""

from __future__ import annotations

import base64
import json
from pathlib import Path

import pytest

from cc_transcript import _parser_rs
from cc_transcript.activity import SessionActivity
from cc_transcript.context import ContextWindow, SchemaError, capture_window
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.render import Budget
from tests.support import requires_rust

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "context_golden.json").read_text("utf-8"))
CAPTURES = GOLDEN["captures"]
WINDOWS = GOLDEN["windows"]
REJECTS = GOLDEN["rejects"]

CAPTURE_CASES = [
    (section["jsonl_b64"], section["session_id"], case)
    for section in CAPTURES
    for case in section["cases"]
]


def anchor_ref(sid: str, uuid: str, tool_use_id: str | None) -> EventRef:
    return EventRef(SessionId(sid), EventUuid(uuid), None if tool_use_id is None else ToolUseId(tool_use_id))


@requires_rust
@pytest.mark.parametrize(
    "b64, sid, case",
    CAPTURE_CASES,
    ids=[f"{i}:{c['anchor_uuid']}:{c['before']}-{c['after']}-{c['preview_chars']}" for i, (_, _, c) in enumerate(CAPTURE_CASES)],
)
def test_capture_parity(b64: str, sid: str, case: dict[str, object]) -> None:
    raw = base64.b64decode(b64)
    activity = SessionActivity.from_events(SessionId(sid), parse_events_from_bytes(raw))
    window = capture_window(
        activity,
        anchor_ref(sid, case["anchor_uuid"], case["anchor_tool_use_id"]),
        before=case["before"],
        after=case["after"],
        preview_chars=case["preview_chars"],
    )
    assert window.to_json() == case["to_json"]
    assert (
        _parser_rs.context_capture_window(
            raw, sid, case["anchor_uuid"], case["anchor_tool_use_id"], case["before"], case["after"], case["preview_chars"]
        )
        == case["to_json"]
    )
    assert ContextWindow.from_json(case["to_json"]).to_json() == case["to_json"]
    assert _parser_rs.context_roundtrip(case["to_json"]) == case["to_json"]
    for preview in case["previews"]:
        tc = preview["turn_chars"]
        assert window.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc)) == preview["expected"]
        assert _parser_rs.context_render_preview(case["to_json"], tc) == preview["expected"]


@requires_rust
@pytest.mark.parametrize("window", WINDOWS, ids=[f"window-{i}" for i in range(len(WINDOWS))])
def test_window_round_trip_parity(window: dict[str, object]) -> None:
    data = window["to_json"]
    assert ContextWindow.from_json(data).to_json() == data
    assert _parser_rs.context_roundtrip(data) == data
    restored = ContextWindow.from_json(data)
    for preview in window["previews"]:
        tc = preview["turn_chars"]
        assert restored.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc)) == preview["expected"]
        assert _parser_rs.context_render_preview(data, tc) == preview["expected"]


@requires_rust
@pytest.mark.parametrize("data", REJECTS, ids=[f"reject-{i}" for i in range(len(REJECTS))])
def test_schema_rejection_parity(data: str) -> None:
    with pytest.raises(SchemaError):
        ContextWindow.from_json(data)
    with pytest.raises(ValueError):
        _parser_rs.context_roundtrip(data)
