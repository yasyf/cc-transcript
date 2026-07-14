from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING

from cc_transcript.activity import SessionActivity, Turn
from cc_transcript.context import ContextWindow
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.mining import sample_windows
from cc_transcript.models import AssistantEvent, UserEvent
from tests import support, testkit

if TYPE_CHECKING:
    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 2, 1, 9, 0, 0, tzinfo=UTC)
SESSION = SessionId("33333333-3333-3333-3333-333333333333")


def user(uuid: str, text: str, *, secs: int = 0) -> UserEvent:
    return support.user(uuid, text, session=SESSION, base=BASE, secs=secs)


def assistant(uuid: str, text: str, *, secs: int = 0) -> AssistantEvent:
    return support.assistant(uuid, text, session=SESSION, base=BASE, secs=secs)


def turn(index: int, prompt: str, events: tuple[TranscriptEvent, ...]) -> Turn:
    return Turn(index=index, prompt=prompt, started_at=None, ended_at=None, events=events, tool_uses=())


def prompted(i: int) -> Turn:
    return turn(i, f"ask {i}", (user(f"u{i}", f"ask {i}", secs=2 * i), assistant(f"a{i}", f"work {i}", secs=2 * i + 1)))


def make_activity(turns: int = 16) -> SessionActivity:
    """Prompted turns, each a user ask answered by the assistant."""
    return SessionActivity(session_id=SESSION, turns=tuple(prompted(i) for i in range(turns)))


def anchor_index(activity: SessionActivity, window: ContextWindow) -> int:
    anchored = activity.turn_of(window.anchor)
    assert anchored is not None
    return anchored.index


def test_sample_windows_deterministic_for_seed() -> None:
    activity = make_activity()
    first = sample_windows(activity, n=3, seed=7)
    assert first == sample_windows(activity, n=3, seed=7)
    other = sample_windows(activity, n=3, seed=8)
    assert [w.anchor for w in other] != [w.anchor for w in first]


def test_sample_windows_sorted_by_turn_index() -> None:
    activity = make_activity()
    indexes = [anchor_index(activity, w) for w in sample_windows(activity, n=5, seed=2)]
    assert len(indexes) == 5
    assert indexes == sorted(indexes)


def test_sample_windows_returns_all_candidates_when_n_exceeds_them() -> None:
    activity = make_activity(turns=4)
    windows = sample_windows(activity, n=99)
    assert [anchor_index(activity, w) for w in windows] == [0, 1, 2]
    assert sample_windows(activity, n=0) == []


def test_sample_windows_never_samples_the_final_turn() -> None:
    activity = make_activity()
    indexes = [anchor_index(activity, w) for w in sample_windows(activity, n=99)]
    assert indexes == list(range(len(activity.turns) - 1))


def test_sample_windows_are_triggerless_with_the_sampled_turn_folded_into_before() -> None:
    activity = make_activity()
    for window in sample_windows(activity, n=99, before=3):
        index = anchor_index(activity, window)
        assert window.anchor == EventRef(SESSION, EventUuid(f"u{index}"))
        assert window.trigger is None
        assert len(window.before) <= 3
        assert window.anchor in window.before[-1].refs
        assert window.before[-1].preview.splitlines()[0] == f"user: ask {index}"


def test_sample_windows_exclusion_radius_is_backward_only() -> None:
    activity = make_activity()
    positive = EventRef(SESSION, EventUuid("a7"))  # resolves to turn index 7
    windows = sample_windows(activity, n=99, exclude=(positive,), exclusion_radius=3)
    indexes = [anchor_index(activity, w) for w in windows]
    assert indexes
    # The pre-steer approach (turns 4-7) is label-conflicted with positive
    # rewinds and stays excluded; turns after the steer are genuine negatives.
    assert all(not (4 <= index <= 7) for index in indexes)
    assert any(index > 7 for index in indexes)


def test_sample_windows_ignores_unresolvable_exclusions() -> None:
    activity = make_activity()
    ghost = EventRef(SESSION, EventUuid("compacted-away"))
    assert sample_windows(activity, n=4, seed=3, exclude=(ghost,)) == sample_windows(activity, n=4, seed=3)


def test_sample_windows_skips_turns_without_resolvable_meta() -> None:
    base = make_activity(turns=4)
    bare = turn(2, "", (testkit.parse_event(testkit.mode_line("default", session_id=str(SESSION))),))
    turns = (*base.turns[:2], bare, *(turn(t.index + 1, t.prompt, t.events) for t in base.turns[2:]))
    activity = SessionActivity(session_id=SESSION, turns=turns)
    assert [anchor_index(activity, w) for w in sample_windows(activity, n=99)] == [0, 1, 3]
