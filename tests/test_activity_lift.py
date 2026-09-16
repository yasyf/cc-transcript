"""Parity of the incremental lift with the cold one.

``ActivityLift.extend`` over a prefix and then its suffix must equal
``SessionActivity.from_events`` over the whole sequence at every event boundary,
including cuts mid-turn, between a tool use and its result, before a late result
that answers an earlier turn's use, and inside sidechain and compact runs; chaining
one-event and small-chunk extends must hold the same equality at every prefix.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript import _native
from cc_transcript.activity import ActivityLift, SessionActivity, native_user_classifier
from cc_transcript.ids import SessionId
from cc_transcript.models import TranscriptEvent, UserEvent
from cc_transcript.parser import parse, parse_events_from_bytes
from cc_transcript.query import Session
from scripts.gen_activity_golden import SYNTHETIC_CASES
from tests import testkit
from tests.corpus import CORPUS_MANIFEST
from tests.support import SESSION, assistant, fixture_bytes, user

TESTDATA = Path(__file__).resolve().parent / "testdata"
FIXTURE_FILES = [
    *sorted((TESTDATA / "views_edge").glob("*.jsonl")),
    TESTDATA / "mcp_root" / "-Users-dev-webapp" / "mcp-fix-1.jsonl",
]
CORPUS = Path(__file__).resolve().parent.parent / ".fixtures" / "corpus"
CORPUS_SCRATCH = sorted(rel for rel in CORPUS_MANIFEST if rel.startswith("-Users-dev-Code-scratch/"))


def bash(uuid: str, tool_use_id: str, *, secs: int) -> TranscriptEvent:
    return assistant(uuid, blocks=(testkit.tool_use(tool_use_id, "Bash", {"command": "ls"}),), secs=secs)


def result(uuid: str, tool_use_id: str, *, secs: int, content: str = "ok") -> TranscriptEvent:
    return user(uuid, blocks=(testkit.tool_result(tool_use_id, content),), secs=secs)


def late_result_past_compact() -> tuple[TranscriptEvent, ...]:
    return (
        user("u0", "run it", secs=0),
        bash("a0", "t0", secs=1),
        result("r0", "t0", secs=2),
        bash("a1", "t1", secs=3),
        user("c0", "/compact", secs=4),
        user("s0", "compact recap", is_compact_summary=True, secs=5),
        result("r1", "t1", secs=6),
        bash("a2", "t2", secs=7),
        result("r2", "t2", secs=8),
    )


def duplicate_result_repairs_a_paired_use() -> tuple[TranscriptEvent, ...]:
    return (
        user("u0", "go", secs=0),
        bash("a0", "t0", secs=1),
        result("r0", "t0", secs=2, content="first"),
        user("u1", "status?", secs=3),
        result("r1", "t0", secs=4, content="again"),
    )


def duplicate_use_ids_across_turns() -> tuple[TranscriptEvent, ...]:
    return (
        user("u0", "go", secs=0),
        bash("a0", "t0", secs=1),
        result("r0", "t0", secs=2, content="first"),
        user("u1", "retry", secs=3),
        bash("a1", "t0", secs=4),
        result("r1", "t0", secs=5, content="second"),
        user("u2", "once more", secs=6),
        result("r2", "t0", secs=7, content="third"),
    )


def orphans_and_prelude() -> tuple[TranscriptEvent, ...]:
    return (
        result("r-", "ghost", secs=0),
        bash("a0", "orphan", secs=1),
        user("s0", "sidechain ask", is_sidechain=True, secs=2),
        bash("a1", "t1", secs=3),
        user("i0", "[Request interrupted by user]", interrupted=True, secs=4),
        user("u0", "real prompt", secs=5),
        result("r1", "t1", secs=6),
        user("m0", "meta", is_meta=True, secs=7),
    )


def opener_carrying_a_result() -> tuple[TranscriptEvent, ...]:
    return (
        user("u0", "go", secs=0),
        bash("a0", "t0", secs=1),
        user("u1", "here is context", blocks=(testkit.tool_result("t0", "carried"),), secs=2),
        bash("a1", "t1", secs=3),
    )


def hand_built() -> dict[str, tuple[TranscriptEvent, ...]]:
    return {
        "late_result_past_compact": late_result_past_compact(),
        "duplicate_result_repairs_a_paired_use": duplicate_result_repairs_a_paired_use(),
        "duplicate_use_ids_across_turns": duplicate_use_ids_across_turns(),
        "orphans_and_prelude": orphans_and_prelude(),
        "opener_carrying_a_result": opener_carrying_a_result(),
    }


def every_other_user(event: UserEvent) -> bool:
    return event.meta.uuid.endswith(("0", "2", "4", "6", "8")) and not event.meta.is_meta


def cases() -> list[pytest.param]:
    return [
        *(pytest.param(events, id=name) for name, events in hand_built().items()),
        pytest.param(tuple(parse_events_from_bytes(fixture_bytes())), id="support_fixture_entries"),
        *(pytest.param(tuple(parse(path).events), id=path.name) for path in FIXTURE_FILES),
        *(
            pytest.param(tuple(parse_events_from_bytes(raw.encode())), id=f"synthetic_{name}")
            for name, raw in SYNTHETIC_CASES.items()
        ),
    ]


CASES = cases()
CLASSIFIERS = [
    pytest.param(native_user_classifier, id="native"),
    pytest.param(every_other_user, id="custom"),
]


def assert_split_parity(events: tuple[TranscriptEvent, ...], user_classifier=native_user_classifier) -> None:
    cold = SessionActivity.from_events(SESSION, events, user_classifier=user_classifier)
    for cut in range(len(events) + 1):
        lift = ActivityLift(SESSION, user_classifier=user_classifier)
        lift.extend(events[:cut])
        assert lift.extend(events[cut:]) == cold, f"cut={cut}"
        assert lift.activity == cold


def assert_chained_parity(
    events: tuple[TranscriptEvent, ...], chunk: int, user_classifier=native_user_classifier
) -> None:
    lift = ActivityLift(SESSION, user_classifier=user_classifier)
    for start in range(0, len(events), chunk):
        assert lift.extend(events[start : start + chunk]) == SessionActivity.from_events(
            SESSION, events[: start + chunk], user_classifier=user_classifier
        ), f"prefix={start + chunk}"


@pytest.mark.parametrize("events", CASES)
@pytest.mark.parametrize("user_classifier", CLASSIFIERS)
def test_extend_matches_cold_lift_at_every_split(events: tuple[TranscriptEvent, ...], user_classifier) -> None:
    assert_split_parity(events, user_classifier)


@pytest.mark.parametrize("events", CASES)
@pytest.mark.parametrize("chunk", [1, 2, 3, 7])
def test_chained_extends_match_cold_lift_at_every_prefix(events: tuple[TranscriptEvent, ...], chunk: int) -> None:
    assert_chained_parity(events, chunk)


@pytest.mark.parametrize("chunk", [1, 5])
def test_chained_extends_under_a_custom_classifier(chunk: int) -> None:
    assert_chained_parity(tuple(parse_events_from_bytes(fixture_bytes())), chunk, every_other_user)


@pytest.mark.parametrize("rel", CORPUS_SCRATCH)
def test_corpus_file_matches_cold_lift_at_every_split(rel: str) -> None:
    events = tuple(parse(CORPUS / rel).events)
    assert_split_parity(events)
    assert_chained_parity(events, 13)


def test_hand_built_cases_reach_the_subtle_boundaries() -> None:
    late = SessionActivity.from_events(SESSION, late_result_past_compact())
    assert [turn.prompt for turn in late.turns] == ["run it", "/compact"]
    assert late.turns[0].tool_uses[1].result is not None
    assert late.turns[0].tool_uses[1].result_ts == late.turns[1].events[2].meta.timestamp

    repaired = SessionActivity.from_events(SESSION, duplicate_result_repairs_a_paired_use())
    assert repaired.turns[0].tool_uses[0].result.content == "again"

    duplicated = SessionActivity.from_events(SESSION, duplicate_use_ids_across_turns())
    assert [use.result.content for turn in duplicated.turns for use in turn.tool_uses] == ["third", "third"]

    prelude = SessionActivity.from_events(SESSION, orphans_and_prelude())
    assert [turn.prompt for turn in prelude.turns] == ["", "real prompt"]
    assert [use.result is None for use in prelude.turns[0].tool_uses] == [True, False]


def test_empty_extend_returns_the_activity_unchanged() -> None:
    lift = ActivityLift(SESSION)
    assert lift.extend(()) == SessionActivity(session_id=SESSION, turns=())
    before = lift.extend(late_result_past_compact()[:3])
    assert lift.extend([]) is before


def test_extend_grows_the_open_turn_by_the_appended_events_alone() -> None:
    lift = ActivityLift(SESSION)
    lift.extend(late_result_past_compact()[:2])
    held = lift.activity.turns[0].tool_uses[0]
    lift.extend(late_result_past_compact()[2:4])
    assert lift.activity.turns[0].tool_uses[0] is not held
    assert lift.activity.turns[0].tool_uses[0].result is not None
    assert lift.activity.turns[0].tool_uses[1].result is None
    assert len(lift.activity.turns[0].events) == 4


def test_native_tail_reports_continuation_and_results() -> None:
    events = list(late_result_past_compact()[5:])
    tail = _native.activity_lift_tail(events, None, open_turn=True)
    assert tail["continued"] is True
    assert tail["results"] == [("t1", 1), ("t2", 3)]
    assert [(turn["start"], turn["end"]) for turn in tail["turns"]] == [(0, 4)]
    assert _native.activity_lift_tail(events, None, open_turn=False)["continued"] is False
    assert _native.activity_lift_tail(events, [True, False, False, False], open_turn=True)["continued"] is False
    with pytest.raises(ValueError, match="opener_flags"):
        _native.activity_lift_tail(events, [True], open_turn=True)


def test_session_over_an_extended_activity_derives_afresh() -> None:
    lift = ActivityLift(SessionId("s"))
    first = Session.from_activity(lift.extend(late_result_past_compact()[:4]))
    assert first.tool_calls.count() == 2
    second = Session.from_activity(lift.extend(late_result_past_compact()[4:]))
    assert "tool_calls" not in second.__dict__
    assert second.tool_calls.count() == 3
    assert first.tool_calls.count() == 2
