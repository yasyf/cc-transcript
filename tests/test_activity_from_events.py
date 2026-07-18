"""Direct coverage for the ``_native.activity_lift_from_events`` index skeleton.

The events-in binding returns a pure positional skeleton — per turn
``{prompt, start, end, started_idx, ended_idx, tool_uses:[{event_idx, tool_use_id,
result_event_idx}]}`` — computed straight from the core segments walk. Nothing in the
contract guarantees the input slice holds unique entries or uuids, so these tests feed
it repeated view objects (the same parsed event listed twice) to prove no index is
reconstructed by a pointer/uuid reverse map that would collapse to a single position.
They also pin that ``opener_flags`` override the native turn classifier.
"""

from __future__ import annotations

from cc_transcript import _native
from tests import testkit
from tests.support import assistant, requires_rust, user


@requires_rust
def test_duplicate_entry_does_not_collapse_turn_indices() -> None:
    # The same view listed twice: a pointer reverse-map would report both turns at
    # start=1,end=2; positional provenance keeps them distinct.
    u = user("u0", "hello")
    turns = _native.activity_lift_from_events([u, u], opener_flags=[True, True])
    assert [(t["start"], t["end"]) for t in turns] == [(0, 1), (1, 2)]
    assert [(t["started_idx"], t["ended_idx"]) for t in turns] == [(0, 0), (1, 1)]
    assert [t["prompt"] for t in turns] == ["hello", "hello"]
    assert all(t["tool_uses"] == [] for t in turns)


@requires_rust
def test_repeated_assistant_view_keeps_positional_provenance() -> None:
    # The same assistant (same uuid) at positions 1 and 3: a uuid reverse-map would push
    # the first turn's ended_idx and tool-use event_idx to 3, past its own [0,2) span.
    u1 = user("u1", "first ask")
    u2 = user("u2", "second ask")
    a = assistant("a0", blocks=(testkit.tool_use("t1", "Bash", {"command": "ls"}),), secs=1)
    turns = _native.activity_lift_from_events([u1, a, u2, a], opener_flags=[True, False, True, False])

    assert len(turns) == 2
    first, second = turns
    assert (first["start"], first["end"]) == (0, 2)
    assert first["started_idx"] == 0
    assert first["ended_idx"] == 1
    assert [tu["event_idx"] for tu in first["tool_uses"]] == [1]
    assert all(first["start"] <= tu["event_idx"] < first["end"] for tu in first["tool_uses"])
    assert first["tool_uses"][0]["result_event_idx"] is None

    assert (second["start"], second["end"]) == (2, 4)
    assert second["ended_idx"] == 3
    assert [tu["event_idx"] for tu in second["tool_uses"]] == [3]


@requires_rust
def test_opener_flags_override_the_native_classifier() -> None:
    # entry 0 is a real prompt the classifier opens; entry 1 is meta it folds in.
    real = user("u0", "first ask")
    meta = user("u1", "second ask", is_meta=True)

    by_classifier = _native.activity_lift_from_events([real, meta])
    assert [t["prompt"] for t in by_classifier] == ["first ask"]

    overridden = _native.activity_lift_from_events([real, meta], opener_flags=[False, True])
    assert [t["prompt"] for t in overridden] == ["", "second ask"]
    assert [(t["start"], t["end"]) for t in overridden] == [(0, 1), (1, 2)]
