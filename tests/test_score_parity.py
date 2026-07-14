"""Golden regression for the Rust score executor over a stage battery.

The Rust backend is the sole score executor. Its output for the full four-stage spec
over a fixed bucket battery is frozen in ``testdata/score_golden.json`` (captured from
the historical Python reference, proven equal), so this asserts the Rust executor
stays stable across short-circuit and post-process — no model, fully deterministic.
"""

from __future__ import annotations

import json
from pathlib import Path

from cc_transcript import _native
from cc_transcript.sentiment.scorespec import (
    build_score_spec,
    clamp_positive,
    clamp_resume,
    demote_mild_irritation,
    flag_frustration,
    score_spec_to_json,
)
from tests.support import requires_rust

BUCKETS: list[list[str]] = [
    ["wtf this is broken"],
    ["amazing!"],
    ["ok"],
    ["status?"],
    ["and again, fix it"],
    ["and again, this is a nightmare"],
    ["continue"],
    ["go ahead."],
    ["a perfectly normal longer message with no triggers"],
    ["please refactor the parser", "looks great honestly"],
    ["héllo 🤖 漢字"],
]

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "score_golden.json").read_text(encoding="utf-8"))


@requires_rust
def test_rust_score_executor_golden() -> None:
    spec_json = score_spec_to_json(
        build_score_spec(flag_frustration(), clamp_positive(), demote_mild_irritation(), clamp_resume())
    )
    assert _native.score_short_circuit(spec_json, BUCKETS) == GOLDEN["short_circuit"]
    for raw_value in range(1, 6):
        raw = [raw_value] * len(BUCKETS)
        assert _native.score_post_process(spec_json, BUCKETS, raw) == GOLDEN["post_process"][str(raw_value)], raw_value
