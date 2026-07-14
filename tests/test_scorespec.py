"""Score-stage behavior: frustration/mild-impatience/resume/positive-clamp logic
and FilteredEngine orchestration. Re-homed from cc-sentiment when the score pipeline
moved into cc-transcript. Every deterministic stage runs in the Rust executor over
the real vendored lexicon; the texts are chosen so the real lexicon supplies the
hit/no-hit each case needs (the frustration and resume stages are lexicon-free)."""

from __future__ import annotations

from datetime import UTC, datetime

import anyio
import pytest

from cc_transcript import _parser_rs
from cc_transcript.models import AssistantEvent, SessionId, UserEvent
from cc_transcript.sentiment import (
    ConversationBucket,
    FilteredEngine,
    ScoreSpec,
    SentimentScore,
    build_score_spec,
    clamp_positive,
    clamp_resume,
    demote_mild_irritation,
    flag_frustration,
)
from cc_transcript.sentiment.buckets import BucketIndex
from cc_transcript.sentiment.scorespec import score_spec_to_json
from tests import testkit

BASE = datetime(2026, 1, 1, tzinfo=UTC)


def short_circuit(spec: ScoreSpec, buckets: list[list[str]]) -> list[SentimentScore | None]:
    return [None if s is None else SentimentScore(s) for s in _parser_rs.score_short_circuit(score_spec_to_json(spec), buckets)]


def post_process(spec: ScoreSpec, buckets: list[list[str]], raw: list[SentimentScore]) -> list[SentimentScore]:
    scored = _parser_rs.score_post_process(score_spec_to_json(spec), buckets, [int(s) for s in raw])
    return [SentimentScore(s) for s in scored]

FRUSTRATION_SPEC = build_score_spec(flag_frustration())
RESUME_SPEC = build_score_spec(clamp_resume())
CLAMP_SPEC = build_score_spec(clamp_positive())
DEMOTE_SPEC = build_score_spec(demote_mild_irritation())

FRUSTRATION_HITS = [
    "wtf is this",
    "this is fucking broken",
    "fuck you",
    "this is a piece of shit",
    "you are completely useless",
    "I give up",
    "STOP GUESSING",
    "stop guessing",
    "> quoted AI proposal text here\n\nSTOP GUESSING",
    "stop making things up",
    "stop making shit up",
    "stop hallucinating",
    "stop being lazy",
    "stop making excuses, figure it out",
    "stop pretending you understand",
    "stop lying to me",
    "just stop it",
    "just stop already, this is getting worse",
]

FRUSTRATION_MISSES = [
    "stop the server",
    "stop at line 10",
    "stop when done",
    "stop processing",
    "STOP THE BUILD",
    "stop after 5 iterations",
    "just stop and think",
    "dont stop, force remove it",
    "GO GO GO",
    "SHIP IT",
    "great, monitor it and fix anything that goes wrong",
    "this sucks",
    "try again with a different approach",
    "no, that's wrong",
    "undo that",
    "please fix the login form",
    "this is great, thanks!",
]

MILD_IMPATIENCE_HITS = [
    "and again! dont modify the repo, just do it in a python call",
    "and again, dont touch the config",
    "yet again, please use X",
    "once again, do this",
    "for the third time, dont touch that file",
    "for the umpteenth time, please run pytest first",
    "AND AGAIN! dont break it",
]

MILD_IMPATIENCE_MISSES = [
    "again, dont modify the repo",
    "dont modify the repo, just do it in a python call",
    "can you try this approach again?",
    "for the first time, this works",
]

RESUME_PHRASES = [
    "continue",
    "Continue",
    "CONTINUE",
    "continue.",
    "Continue!",
    "continue please",
    "please continue",
    "resume",
    "go ahead",
    "keep going",
    "carry on",
    "proceed",
    "ok continue",
    "Continue from where you left off",
    "[context restored] resume",
]

NON_RESUME = [
    "amazing! continue",
    "great work, continue please",
    "continue with the refactor",
    "should I continue?",
    "status?",
    "",
]


def user(text: str, *, minutes: float = 0.0) -> UserEvent:
    event = testkit.parse_event(testkit.user_line(f"u{minutes}", text, session_id="s", timestamp=BASE))
    assert isinstance(event, UserEvent)
    return event


def assistant(text: str, *, model: str = "m") -> AssistantEvent:
    event = testkit.parse_event(testkit.assistant_line("a", text, model=model, session_id="s", timestamp=BASE))
    assert isinstance(event, AssistantEvent)
    return event


def bucket(*texts: str) -> ConversationBucket:
    return ConversationBucket(
        session_id=SessionId("s"),
        bucket_index=BucketIndex(0),
        bucket_start=BASE,
        events=tuple(user(t) for t in texts),
    )


class StubEngine:
    def __init__(self, scores: list[SentimentScore]) -> None:
        self.scores = scores
        self.received: list[ConversationBucket] = []

    async def score(self, buckets, on_progress=lambda _: None):  # noqa: ANN001, ANN201
        self.received = list(buckets)
        for _ in buckets:
            on_progress(1)
        return self.scores[: len(buckets)]

    def peak_memory_gb(self) -> float:
        return 0.42

    async def close(self) -> None:
        return None


@pytest.mark.parametrize("text", FRUSTRATION_HITS)
def test_frustration_short_circuits(text: str) -> None:
    assert short_circuit(FRUSTRATION_SPEC, [[text]]) == [SentimentScore(1)]


@pytest.mark.parametrize("text", FRUSTRATION_MISSES)
def test_frustration_does_not_short_circuit(text: str) -> None:
    assert short_circuit(FRUSTRATION_SPEC, [[text]]) == [None]


@pytest.mark.parametrize("text", RESUME_PHRASES)
def test_resume_clamps_to_three(text: str) -> None:
    assert post_process(RESUME_SPEC, [[text]], [SentimentScore(5)]) == [SentimentScore(3)]


@pytest.mark.parametrize("text", NON_RESUME)
def test_non_resume_unchanged(text: str) -> None:
    assert post_process(RESUME_SPEC, [[text]], [SentimentScore(5)]) == [SentimentScore(5)]


@pytest.mark.parametrize("text", MILD_IMPATIENCE_HITS)
def test_mild_impatience_demotes_when_not_hostile(text: str) -> None:
    assert post_process(DEMOTE_SPEC, [[text]], [SentimentScore(1)]) == [SentimentScore(2)]


@pytest.mark.parametrize("text", MILD_IMPATIENCE_MISSES)
def test_mild_impatience_no_trigger_no_demote(text: str) -> None:
    assert post_process(DEMOTE_SPEC, [[text]], [SentimentScore(1)]) == [SentimentScore(1)]


def test_mild_impatience_kept_when_hostile_lexicon() -> None:
    # 'broken' is a fixed negative-floor override, so the real lexicon marks it hostile.
    assert post_process(DEMOTE_SPEC, [["and again! this is broken"]], [SentimentScore(1)]) == [SentimentScore(1)]


def test_mild_impatience_hostile_via_frustration_regex() -> None:
    # "fuck you" trips the frustration (hostile) regex.
    assert post_process(DEMOTE_SPEC, [["and again! fuck you"]], [SentimentScore(1)]) == [SentimentScore(1)]


def test_positive_clamp_lowers_5_when_short_no_positive() -> None:
    assert post_process(CLAMP_SPEC, [["status?"]], [SentimentScore(5)]) == [SentimentScore(3)]


def test_positive_clamp_keeps_5_when_positive_present() -> None:
    # 'amazing' is above the positive floor in the real lexicon.
    assert post_process(CLAMP_SPEC, [["amazing!"]], [SentimentScore(5)]) == [SentimentScore(5)]


def test_positive_clamp_keeps_5_when_long() -> None:
    assert post_process(CLAMP_SPEC, [["are we doing well today"]], [SentimentScore(5)]) == [SentimentScore(5)]


def test_positive_clamp_ignores_non_5() -> None:
    assert post_process(CLAMP_SPEC, [["status?"]], [SentimentScore(4)]) == [SentimentScore(4)]


def test_engine_forwards_with_empty_spec() -> None:
    stub = StubEngine([SentimentScore(3), SentimentScore(4)])
    engine = FilteredEngine(stub, build_score_spec())
    assert anyio.run(engine.score, [bucket("a"), bucket("b")]) == [SentimentScore(3), SentimentScore(4)]
    assert len(stub.received) == 2


def test_engine_short_circuit_scatters_and_progress() -> None:
    stub = StubEngine([SentimentScore(3)])
    engine = FilteredEngine(stub, FRUSTRATION_SPEC)
    buckets = [bucket("wtf"), bucket("please help"), bucket("fuck you")]
    calls: list[int] = []
    scores = anyio.run(lambda: engine.score(buckets, on_progress=calls.append))
    assert scores == [SentimentScore(1), SentimentScore(3), SentimentScore(1)]
    assert stub.received == [buckets[1]]
    assert calls == [2, 1]


def test_engine_ignores_assistant_frustration() -> None:
    bucket_with_assistant = ConversationBucket(
        session_id=SessionId("s"),
        bucket_index=BucketIndex(0),
        bucket_start=BASE,
        events=(
            assistant("wtf fuck you"),
            user("please continue with the task"),
        ),
    )
    engine = FilteredEngine(StubEngine([SentimentScore(4)]), FRUSTRATION_SPEC)
    assert anyio.run(engine.score, [bucket_with_assistant]) == [SentimentScore(4)]


def test_engine_close_and_peak_delegate() -> None:
    stub = StubEngine([])
    engine = FilteredEngine(stub, build_score_spec())
    assert engine.peak_memory_gb() == 0.42
    anyio.run(engine.close)
