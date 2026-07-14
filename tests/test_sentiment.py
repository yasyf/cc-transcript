from __future__ import annotations

from datetime import UTC, datetime, timedelta

import anyio

from cc_transcript.models import (
    AssistantEvent,
    SessionId,
    UserEvent,
)
from cc_transcript.sentiment import (
    FilteredEngine,
    SentimentScore,
    bucket_events,
    build_score_spec,
    clamp_resume,
    extract_bucket_keys,
    flag_frustration,
)
from cc_transcript import _parser_rs
from cc_transcript.sentiment.scorespec import ScoreSpec, score_spec_to_json
from tests import support, testkit

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)


def short_circuit(spec: ScoreSpec, buckets: list[list[str]]) -> list[SentimentScore | None]:
    return [None if s is None else SentimentScore(s) for s in _parser_rs.score_short_circuit(score_spec_to_json(spec), buckets)]


def post_process(spec: ScoreSpec, buckets: list[list[str]], raw: list[SentimentScore]) -> list[SentimentScore]:
    scored = _parser_rs.score_post_process(score_spec_to_json(spec), buckets, [int(s) for s in raw])
    return [SentimentScore(s) for s in scored]


def user(text: str, *, minutes: float = 0.0, session: str = "s") -> UserEvent:
    return support.user(f"u{minutes}", text, session=SessionId(session), base=BASE, secs=int(minutes * 60))


def assistant(text: str, *, minutes: float = 0.0, session: str = "s") -> AssistantEvent:
    return support.assistant(f"a{minutes}", text, session=SessionId(session), base=BASE, secs=int(minutes * 60))


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
        testkit.parse_event(testkit.system_line("hook", content="noise", uuid="sys0")),
        assistant("on it", minutes=0.5),
        testkit.parse_event(testkit.mode_line("normal", session_id="s")),
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
    assert short_circuit(spec, [["wtf this is broken", "still broken"]]) == [SentimentScore(1)]
    assert short_circuit(spec, [["please fix the bug"]]) == [None]


def test_resume_stage_clamps_to_three() -> None:
    spec = build_score_spec(clamp_resume())
    assert post_process(spec, [["go ahead", "continue"]], [SentimentScore(5)]) == [SentimentScore(3)]
    assert post_process(spec, [["a longer real message"]], [SentimentScore(5)]) == [SentimentScore(5)]


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
