from __future__ import annotations

from typing import Any

import orjson

from cc_transcript.activity import SessionActivity
from cc_transcript.context import ContextWindow
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.mining import sample_windows
from cc_transcript.parser import parse_events_from_bytes
from tests import testkit

SESSION = SessionId("33333333-3333-3333-3333-333333333333")


def prompted_lines(i: int) -> list[dict[str, Any]]:
    return [
        testkit.user_line(f"u{i}", f"ask {i}", session_id=str(SESSION), secs=2 * i),
        testkit.assistant_line(f"a{i}", f"work {i}", session_id=str(SESSION), secs=2 * i + 1),
    ]


def to_bytes(lines: list[dict[str, Any]]) -> bytes:
    return b"\n".join(orjson.dumps(line) for line in lines)


def make_transcript(turns: int = 16) -> bytes:
    return to_bytes([line for i in range(turns) for line in prompted_lines(i)])


def make_activity(raw: bytes) -> SessionActivity:
    return SessionActivity.from_events(SESSION, parse_events_from_bytes(raw))


def anchor_index(activity: SessionActivity, window: ContextWindow) -> int:
    anchored = activity.turn_of(window.anchor)
    assert anchored is not None
    return anchored.index


def test_sample_windows_deterministic_for_seed() -> None:
    raw = make_transcript()
    first = sample_windows(raw, n=3, seed=7)
    assert first == sample_windows(raw, n=3, seed=7)
    other = sample_windows(raw, n=3, seed=8)
    assert [w.anchor for w in other] != [w.anchor for w in first]


def test_sample_windows_sorted_by_turn_index() -> None:
    raw = make_transcript()
    activity = make_activity(raw)
    indexes = [anchor_index(activity, w) for w in sample_windows(raw, n=5, seed=2)]
    assert len(indexes) == 5
    assert indexes == sorted(indexes)


def test_sample_windows_returns_all_candidates_when_n_exceeds_them() -> None:
    raw = make_transcript(turns=4)
    activity = make_activity(raw)
    windows = sample_windows(raw, n=99)
    assert [anchor_index(activity, w) for w in windows] == [0, 1, 2]
    assert sample_windows(raw, n=0) == []


def test_sample_windows_never_samples_the_final_turn() -> None:
    raw = make_transcript()
    activity = make_activity(raw)
    indexes = [anchor_index(activity, w) for w in sample_windows(raw, n=99)]
    assert indexes == list(range(len(activity.turns) - 1))


def test_sample_windows_are_triggerless_with_the_sampled_turn_folded_into_before() -> None:
    raw = make_transcript()
    activity = make_activity(raw)
    for window in sample_windows(raw, n=99, before=3):
        index = anchor_index(activity, window)
        assert window.anchor == EventRef(SESSION, EventUuid(f"u{index}"))
        assert window.trigger is None
        assert len(window.before) <= 3
        assert window.anchor in window.before[-1].refs
        assert window.before[-1].preview.splitlines()[0] == f"user: ask {index}"


def test_sample_windows_exclusion_radius_is_backward_only() -> None:
    raw = make_transcript()
    activity = make_activity(raw)
    positive = EventRef(SESSION, EventUuid("a7"))  # resolves to turn index 7
    windows = sample_windows(raw, n=99, exclude=(positive,), exclusion_radius=3)
    indexes = [anchor_index(activity, w) for w in windows]
    assert indexes
    # The pre-steer approach (turns 4-7) is label-conflicted with positive
    # rewinds and stays excluded; turns after the steer are genuine negatives.
    assert all(not (4 <= index <= 7) for index in indexes)
    assert any(index > 7 for index in indexes)


def test_sample_windows_exclusion_on_the_final_turn_clears_the_preceding_radius() -> None:
    raw = make_transcript()
    activity = make_activity(raw)
    final = EventRef(SESSION, EventUuid("a15"))  # resolves to the final turn, itself never a candidate
    windows = sample_windows(raw, n=99, exclude=(final,), exclusion_radius=3)
    assert [anchor_index(activity, w) for w in windows] == list(range(12))


def test_sample_windows_accepts_arbitrary_int_seeds() -> None:
    raw = make_transcript()
    huge = sample_windows(raw, n=3, seed=2**63)
    assert huge == sample_windows(raw, n=3, seed=2**63)
    assert [w.anchor for w in huge] != [w.anchor for w in sample_windows(raw, n=3, seed=7)]


def test_sample_windows_ignores_unresolvable_exclusions() -> None:
    raw = make_transcript()
    ghost = EventRef(SESSION, EventUuid("compacted-away"))
    assert sample_windows(raw, n=4, seed=3, exclude=(ghost,)) == sample_windows(raw, n=4, seed=3)


def test_sample_windows_skips_turns_without_resolvable_meta() -> None:
    # A leading mode envelope forms turn 0 with no resolvable meta (no anchor);
    # the final turn may still be in flight. Every other turn is a candidate.
    raw = to_bytes(
        [testkit.mode_line("default", session_id=str(SESSION)), *(line for i in range(4) for line in prompted_lines(i))]
    )
    activity = make_activity(raw)
    assert [anchor_index(activity, w) for w in sample_windows(raw, n=99)] == [1, 2, 3]


def test_sample_windows_native_draw_is_pinned() -> None:
    # Freezes the exact Rust-native seeded draw for one (seed, transcript) pair, so
    # the deterministic sample stays stable across processes and releases.
    raw = make_transcript()
    activity = make_activity(raw)
    indexes = [anchor_index(activity, w) for w in sample_windows(raw, n=3, seed=7)]
    assert indexes == [0, 5, 14]
