from __future__ import annotations

from datetime import UTC, datetime, timedelta

import anyio

from cc_transcript.models import SessionId
from cc_transcript.sentiment import (
    AssistantMessage,
    ConversationBucketer,
    FilteredEngine,
    FrustrationFilter,
    ScoreFilter,
    SentimentScore,
    SessionResumeFilter,
    UserMessage,
    extract_bucket_keys,
)

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
    # session "t" has only one user turn -> dropped (MIN_USER_TURNS_PER_SESSION)
    assert {b.session_id for b in buckets} == {SessionId("s")}
    assert len(buckets) == 1
    keys = extract_bucket_keys(messages)
    assert [k.session_id for k in keys] == [SessionId("s")]


def test_bucketer_drops_sub_min_user_chars_bucket() -> None:
    messages = [
        user("ok", minutes=0),
        assistant("ack", minutes=0.2),
        user("actually fix the bug now", minutes=3),
        assistant("sure", minutes=3.2),
    ]
    keys = extract_bucket_keys(messages)
    assert sorted(k.bucket_index for k in keys) == [1]


def test_frustration_short_circuits_to_one() -> None:
    bucket = ConversationBucketer.bucket_messages(
        [user("wtf this is broken", minutes=0), assistant("sorry", minutes=0.5), user("still broken", minutes=1)]
    )[0]
    assert FrustrationFilter().short_circuit(bucket) == SentimentScore(1)


def test_session_resume_clamps_to_three() -> None:
    bucket = ConversationBucketer.bucket_messages(
        [user("go ahead", minutes=0), assistant("continuing", minutes=0.5), user("continue", minutes=1)]
    )[0]
    assert SessionResumeFilter().post_process(bucket, SentimentScore(5)) == SentimentScore(3)


def test_filtered_engine_short_circuit_bypasses_inference() -> None:
    buckets = ConversationBucketer.bucket_messages(
        [user("wtf broken garbage", minutes=0), assistant("x", minutes=0.5), user("fix it now please", minutes=1)]
    )
    filters: tuple[ScoreFilter, ...] = (FrustrationFilter(), SessionResumeFilter())
    engine = FilteredEngine(StubEngine(4), filters)
    scores = anyio.run(engine.score, buckets)
    # frustration short-circuits to 1 without hitting the stub's 4
    assert scores == [SentimentScore(1)]


def test_filtered_engine_falls_through_to_inference() -> None:
    buckets = ConversationBucketer.bucket_messages(
        [
            user("add a docstring to this function", minutes=0),
            assistant("ok", minutes=0.5),
            user("and a test", minutes=1),
        ]
    )
    engine = FilteredEngine(StubEngine(4), (FrustrationFilter(), SessionResumeFilter()))
    assert anyio.run(engine.score, buckets) == [SentimentScore(4)]
