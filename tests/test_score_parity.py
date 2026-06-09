"""Rust score executor == Python score interpreter, over a stage battery.

Both backends call the same Rust udpipe lexicon for lexicon stages, so a confirmed
match proves the dual-backend score pipeline agrees. Skips when the extension or the
udpipe model is unavailable.
"""

from __future__ import annotations

import pytest

from cc_transcript.domains.sentiment.buckets import SentimentScore
from cc_transcript.domains.sentiment.lexicon import rust_lexicon
from cc_transcript.domains.sentiment.scorespec import (
    build_score_spec,
    clamp_positive,
    clamp_resume,
    demote_mild_irritation,
    flag_frustration,
    py_post_process,
    py_short_circuit,
    score_spec_to_json,
)
from tests.test_backend_parity import requires_rust

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


@requires_rust
def test_rust_python_score_executor_parity() -> None:
    from cc_transcript import _parser_rs

    if not hasattr(_parser_rs, "score_short_circuit"):
        pytest.skip("score executor not built")
    if rust_lexicon() is None:
        pytest.skip("udpipe lexicon model unavailable")

    spec = build_score_spec(flag_frustration(), clamp_positive(), demote_mild_irritation(), clamp_resume())
    spec_json = score_spec_to_json(spec)

    expected_sc = [None if s is None else int(s) for s in py_short_circuit(spec, BUCKETS)]
    assert _parser_rs.score_short_circuit(spec_json, BUCKETS) == expected_sc

    for raw_value in range(1, 6):
        raw = [raw_value] * len(BUCKETS)
        expected_pp = [int(s) for s in py_post_process(spec, BUCKETS, [SentimentScore(r) for r in raw])]
        assert _parser_rs.score_post_process(spec_json, BUCKETS, raw) == expected_pp, raw_value
