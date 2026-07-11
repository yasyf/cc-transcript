from __future__ import annotations

from datetime import UTC, datetime, timedelta

import anyio

from cc_transcript.models import AssistantEvent, EntryMeta, EventUuid, ModeEvent, SessionId, SystemEvent, UserEvent
from cc_transcript.sentiment import (
    FilteredEngine,
    SentimentScore,
    bucket_events,
    build_score_spec,
    clamp_resume,
    extract_bucket_keys,
    flag_frustration,
)
from cc_transcript.sentiment.scorespec import py_post_process, py_short_circuit

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)


def meta(uuid: str, *, minutes: float = 0.0, session: str = "s") -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SessionId(session),
        timestamp=BASE + timedelta(minutes=minutes),
        cwd="/repo",
        git_branch="main",
        cc_version=None,
        is_sidechain=False,
        is_meta=False,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(text: str, *, minutes: float = 0.0, session: str = "s") -> UserEvent:
    return UserEvent(
        meta=meta(f"u{minutes}", minutes=minutes, session=session), text=text, blocks=(), interrupted=False
    )


def assistant(text: str, *, minutes: float = 0.0, session: str = "s") -> AssistantEvent:
    return AssistantEvent(
        meta=meta(f"a{minutes}", minutes=minutes, session=session),
        model="claude-opus-4-7",
        text=text,
        blocks=(),
        stop_reason=None,
        usage=None,
    )


class StubEngine:
    def __init__(self, score: int) -> None:
        self.score_value = score

    async def score(self, buckets, on_progress=lambda _: None):  # noqa: ANN001, ANN201
        on_progress(len(buckets))
        return [SentimentScore(self.score_value) for _ in buckets]

    def peak_memory_gb(self) -> float:
        return 0.0

    async def close(self) -> None:
        return None


def test_bucketer_groups_by_session_and_window() -> None:
    events = [
        user("please fix the login bug", minutes=0),
        assistant("on it", minutes=0.5),
        user("now add validation too", minutes=1),
        assistant("done", minutes=1.5),
        user("other session message here", minutes=0, session="t"),
    ]
    buckets = bucket_events(events)
    assert {b.session_id for b in buckets} == {SessionId("s")}
    assert len(buckets) == 1
    assert [k.session_id for k in extract_bucket_keys(events)] == [SessionId("s")]


def test_bucketer_drops_sub_min_user_chars_bucket() -> None:
    events = [
        user("ok", minutes=0),
        assistant("ack", minutes=0.2),
        user("actually fix the bug now", minutes=3),
        assistant("sure", minutes=3.2),
    ]
    assert sorted(k.bucket_index for k in extract_bucket_keys(events)) == [1]


def test_bucketer_ignores_whitespace_only_user_turns() -> None:
    def events(first_turn: str) -> list[UserEvent | AssistantEvent]:
        return [
            user(first_turn, minutes=0),
            assistant("ack", minutes=0.2),
            user("actually fix the bug now", minutes=3),
            assistant("sure", minutes=3.2),
        ]

    assert sorted(k.bucket_index for k in extract_bucket_keys(events(" \t \n  "))) == [1]
    assert sorted(k.bucket_index for k in extract_bucket_keys(events("hi ok"))) == [0, 1]


def test_bucketer_ignores_non_conversational_events() -> None:
    events = [
        user("please fix the login bug", minutes=0),
        SystemEvent(meta=meta("sys0"), subtype="hook", content="noise"),
        assistant("on it", minutes=0.5),
        ModeEvent(session_id=SessionId("s"), channel="mode", value="normal"),
        user("now add validation too", minutes=1),
    ]
    (only,) = bucket_events(events)
    assert len(only.events) == 3
    assert all(isinstance(e, UserEvent | AssistantEvent) for e in only.events)


def test_bucketer_drops_junk_user_turns() -> None:
    # A protocol-noise turn must not pad MIN_USER_TURNS eligibility: one genuine
    # user turn plus one junk turn leaves the session below the floor, so nothing
    # buckets — before this the junk turn would have counted and yielded a bucket.
    padded = [
        user("please fix the login bug", minutes=0),
        assistant("on it", minutes=0.5),
        user("<local-command-stderr>fatal: not a git repo</local-command-stderr>", minutes=1),
    ]
    assert bucket_events(padded) == []

    # Two genuine turns clear the floor; the junk turn is dropped from the bucket.
    kept = [
        user("please fix the login bug", minutes=0),
        assistant("on it", minutes=0.5),
        user("<local-command-stdout>3 files changed</local-command-stdout>", minutes=1),
        user("now add validation too", minutes=1.5),
        assistant("done", minutes=2),
    ]
    (bucket,) = bucket_events(kept)
    assert [e.text for e in bucket.events if isinstance(e, UserEvent)] == [
        "please fix the login bug",
        "now add validation too",
    ]


def test_bucketer_keeps_mid_text_bash_tag_mention() -> None:
    # A genuine turn that merely mentions a bash tag mid-sentence is authored prose,
    # not a command echo. Dropping it as junk would leave one genuine turn, push the
    # session below MIN_USER_TURNS_PER_SESSION, and kill the bucket outright.
    events = [
        user("please fix the login bug", minutes=0),
        assistant("on it", minutes=0.5),
        user("why does <bash-input>uv run pytest</bash-input> appear in my transcript?", minutes=1),
    ]
    (bucket,) = bucket_events(events)
    assert [e.text for e in bucket.events if isinstance(e, UserEvent)] == [
        "please fix the login bug",
        "why does <bash-input>uv run pytest</bash-input> appear in my transcript?",
    ]


def test_frustration_stage_short_circuits_to_one() -> None:
    spec = build_score_spec(flag_frustration())
    assert py_short_circuit(spec, [["wtf this is broken", "still broken"]]) == [SentimentScore(1)]
    assert py_short_circuit(spec, [["please fix the bug"]]) == [None]


def test_resume_stage_clamps_to_three() -> None:
    spec = build_score_spec(clamp_resume())
    assert py_post_process(spec, [["go ahead", "continue"]], [SentimentScore(5)]) == [SentimentScore(3)]
    assert py_post_process(spec, [["a longer real message"]], [SentimentScore(5)]) == [SentimentScore(5)]


def test_filtered_engine_short_circuit_bypasses_inference() -> None:
    buckets = bucket_events(
        [user("wtf broken garbage", minutes=0), assistant("x", minutes=0.5), user("fix it now please", minutes=1)]
    )
    engine = FilteredEngine(StubEngine(4), build_score_spec(flag_frustration(), clamp_resume()))
    assert anyio.run(engine.score, buckets) == [SentimentScore(1)]


def test_filtered_engine_falls_through_to_inference() -> None:
    buckets = bucket_events(
        [
            user("add a docstring to this function", minutes=0),
            assistant("ok", minutes=0.5),
            user("and a test", minutes=1),
        ]
    )
    engine = FilteredEngine(StubEngine(4), build_score_spec(flag_frustration(), clamp_resume()))
    assert anyio.run(engine.score, buckets) == [SentimentScore(4)]
