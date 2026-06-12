from __future__ import annotations

from datetime import UTC, datetime, timedelta

import anyio

from cc_transcript.models import SessionId
from cc_transcript.sentiment import (
    AssistantMessage,
    ConversationBucketer,
    FilteredEngine,
    SentimentScore,
    UserMessage,
    build_score_spec,
    clamp_resume,
    extract_bucket_keys,
    flag_frustration,
)
from cc_transcript.sentiment.scorespec import py_post_process, py_short_circuit

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)


def user(text: str, *, minutes: float = 0.0, session: str = "s") -> UserMessage:
    return UserMessage(
        content=text,
        timestamp=BASE + timedelta(minutes=minutes),
        session_id=SessionId(session),
        uuid=f"u{minutes}",
        tool_calls=(),
        thinking_chars=0,
        cc_version="1.0",
    )


def assistant(text: str, *, minutes: float = 0.0, session: str = "s") -> AssistantMessage:
    return AssistantMessage(
        content=text,
        timestamp=BASE + timedelta(minutes=minutes),
        session_id=SessionId(session),
        uuid=f"a{minutes}",
        tool_calls=(),
        thinking_chars=0,
        claude_model="claude-opus-4-7",
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
    messages = [
        user("please fix the login bug", minutes=0),
        assistant("on it", minutes=0.5),
        user("now add validation too", minutes=1),
        assistant("done", minutes=1.5),
        user("other session message here", minutes=0, session="t"),
    ]
    buckets = ConversationBucketer.bucket_messages(messages)
    assert {b.session_id for b in buckets} == {SessionId("s")}
    assert len(buckets) == 1
    assert [k.session_id for k in extract_bucket_keys(messages)] == [SessionId("s")]


def test_bucketer_drops_sub_min_user_chars_bucket() -> None:
    messages = [
        user("ok", minutes=0),
        assistant("ack", minutes=0.2),
        user("actually fix the bug now", minutes=3),
        assistant("sure", minutes=3.2),
    ]
    assert sorted(k.bucket_index for k in extract_bucket_keys(messages)) == [1]


def test_frustration_stage_short_circuits_to_one() -> None:
    spec = build_score_spec(flag_frustration())
    assert py_short_circuit(spec, [["wtf this is broken", "still broken"]]) == [SentimentScore(1)]
    assert py_short_circuit(spec, [["please fix the bug"]]) == [None]


def test_resume_stage_clamps_to_three() -> None:
    spec = build_score_spec(clamp_resume())
    assert py_post_process(spec, [["go ahead", "continue"]], [SentimentScore(5)]) == [SentimentScore(3)]
    assert py_post_process(spec, [["a longer real message"]], [SentimentScore(5)]) == [SentimentScore(5)]


def test_filtered_engine_short_circuit_bypasses_inference() -> None:
    buckets = ConversationBucketer.bucket_messages(
        [user("wtf broken garbage", minutes=0), assistant("x", minutes=0.5), user("fix it now please", minutes=1)]
    )
    engine = FilteredEngine(StubEngine(4), build_score_spec(flag_frustration(), clamp_resume()))
    assert anyio.run(engine.score, buckets) == [SentimentScore(1)]


def test_filtered_engine_falls_through_to_inference() -> None:
    buckets = ConversationBucketer.bucket_messages(
        [
            user("add a docstring to this function", minutes=0),
            assistant("ok", minutes=0.5),
            user("and a test", minutes=1),
        ]
    )
    engine = FilteredEngine(StubEngine(4), build_score_spec(flag_frustration(), clamp_resume()))
    assert anyio.run(engine.score, buckets) == [SentimentScore(4)]
