"""Rust ↔ Python parity for durable context windows over the frozen golden.

``capture_window`` is a native facade in v14, so ``captures`` pins the native
``context_capture_window`` against the frozen ``to_json`` alongside the Python
``from_json`` → ``to_json`` round-trip and ``render_preview`` over it; ``windows``
pins the same round-trip and ``render_preview`` over hand-built windows; ``rejects``
pins schema rejection on both sides. Regenerate with ``scripts/gen_context_golden.py``.
"""

from __future__ import annotations

import base64
import json
from pathlib import Path

import pytest

from cc_transcript import _native
from cc_transcript.context import ContextWindow, SchemaError
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


@requires_rust
@pytest.mark.parametrize(
    "b64, sid, case",
    CAPTURE_CASES,
    ids=[f"{i}:{c['anchor_uuid']}:{c['before']}-{c['after']}-{c['preview_chars']}" for i, (_, _, c) in enumerate(CAPTURE_CASES)],
)
def test_capture_parity(b64: str, sid: str, case: dict[str, object]) -> None:
    raw = base64.b64decode(b64)
    assert (
        _native.context_capture_window(
            raw, sid, case["anchor_uuid"], case["anchor_tool_use_id"], case["before"], case["after"], case["preview_chars"]
        )
        == case["to_json"]
    )
    restored = ContextWindow.from_json(case["to_json"])
    assert restored.to_json() == case["to_json"]
    assert _native.context_roundtrip(case["to_json"]) == case["to_json"]
    for preview in case["previews"]:
        tc = preview["turn_chars"]
        assert restored.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc)) == preview["expected"]
        assert _native.context_render_preview(case["to_json"], tc) == preview["expected"]


@requires_rust
@pytest.mark.parametrize("window", WINDOWS, ids=[f"window-{i}" for i in range(len(WINDOWS))])
def test_window_round_trip_parity(window: dict[str, object]) -> None:
    data = window["to_json"]
    assert ContextWindow.from_json(data).to_json() == data
    assert _native.context_roundtrip(data) == data
    restored = ContextWindow.from_json(data)
    for preview in window["previews"]:
        tc = preview["turn_chars"]
        assert restored.render_preview(budget=Budget(turn_chars=tc, tool_chars=tc)) == preview["expected"]
        assert _native.context_render_preview(data, tc) == preview["expected"]


@requires_rust
@pytest.mark.parametrize("data", REJECTS, ids=[f"reject-{i}" for i in range(len(REJECTS))])
def test_schema_rejection_parity(data: str) -> None:
    with pytest.raises(SchemaError):
        ContextWindow.from_json(data)
    with pytest.raises(ValueError):
        _native.context_roundtrip(data)
