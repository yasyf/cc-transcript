from __future__ import annotations

import json
import re
from datetime import UTC, datetime, timedelta
from pathlib import Path

import anyio

from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    EntryUuid,
    ModeEvent,
    SessionId,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    UserEvent,
)
from cc_transcript.domains.mining import (
    DENIAL_PREFIX,
    INTERRUPT_REJECTION,
    PLAN_REVIEW,
    TRANSCRIPT_MESSAGE,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
    ContextSnapshot,
    FeedbackCandidate,
    FeedbackStore,
    ReviewComment,
    ReviewFormat,
    Stats,
    build_snapshot,
    dedup_key,
    firm,
    iter_interrupt_marker_signals,
    iter_plan_reentry_signals,
    iter_review_comment_signals,
    iter_tool_denial_signals,
    iter_user_message_signals,
    weak,
)
from cc_transcript.domains.mining.confidence import from_payload

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("sess-1")


def meta(uuid: str, *, secs: int = 0) -> EntryMeta:
    return EntryMeta(
        uuid=EntryUuid(uuid),
        parent_uuid=None,
        session_id=SESSION,
        timestamp=BASE + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=False,
        is_meta=False,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(uuid: str, text: str = "", *, blocks: tuple[ContentBlock, ...] = (), secs: int = 0) -> UserEvent:
    return UserEvent(meta=meta(uuid, secs=secs), text=text, blocks=blocks, interrupted=False)


def assistant(uuid: str, text: str = "", *, blocks: tuple[ContentBlock, ...] = (), secs: int = 0) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-7", text=text, blocks=blocks, stop_reason=None
    )


def denial_content(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def test_dedup_key_stable_and_content_derived() -> None:
    assert dedup_key("sess-1", "transcript_message", "hi") == dedup_key("sess-1", "transcript_message", "hi")
    assert dedup_key("sess-1", "transcript_message", "hi") != dedup_key("sess-1", "transcript_message", "bye")
    assert len(dedup_key("a", "b")) == 64


def test_build_snapshot_before_trigger_after() -> None:
    events = [
        user("u0", "first ask"),
        assistant("a0", "working on it"),
        user("u1", "the feedback"),
        assistant("a1", "fixed"),
    ]
    snap = build_snapshot(events, 2)
    assert isinstance(snap, ContextSnapshot)
    assert snap.trigger is not None
    assert snap.trigger.role == "assistant"
    assert snap.trigger.text == "working on it"
    assert [turn.text for turn in snap.before] == ["first ask", "working on it"]
    assert [turn.text for turn in snap.after] == ["fixed"]


def test_iter_user_message_signals_filters_and_sets_trigger() -> None:
    events = [
        user("u0", "   "),
        user("u1", "[Request interrupted by user]"),
        user("u2", "real feedback here"),
        assistant("a0", "ok"),
        user("u3", "more feedback"),
    ]
    signals = list(iter_user_message_signals(events))
    assert [signal.event_uuid for signal in signals] == [EntryUuid("u2"), EntryUuid("u3")]
    assert signals[0].trigger_index is None
    assert signals[1].trigger_index == 3
    assert signals[0].kind == TRANSCRIPT_MESSAGE
    assert all(signal.detector == "transcript_message" for signal in signals)
    assert signals[0].signal == firm("transcript_message")


def test_iter_tool_denial_signals_extracts_embedded_text() -> None:
    events = [
        assistant(
            "a0",
            blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Bash", input={"command": "rm -rf /"}),),
        ),
        user(
            "u0",
            blocks=(
                ToolResultBlock(
                    tool_use_id=ToolUseId("t1"), content=denial_content("use a different approach"), is_error=True
                ),
            ),
        ),
    ]
    signals = list(iter_tool_denial_signals(events))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.kind == INTERRUPT_REJECTION
    assert signal.detector == "denial"
    assert signal.text == "use a different approach"
    assert signal.evidence == {"tool": "Bash", "file_path": None}
    assert signal.trigger_index == 0
    assert signal.signal == firm("denial")


def test_iter_interrupt_marker_signals_extracts_correction() -> None:
    events = [
        assistant("a0", "doing work"),
        user(
            "u0",
            blocks=(
                ToolResultBlock(
                    tool_use_id=ToolUseId("t9"), content="[Request interrupted by user]", is_error=True
                ),
            ),
        ),
        user("u1", "actually do it this way instead"),
    ]
    signals = list(iter_interrupt_marker_signals(events))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.kind == INTERRUPT_REJECTION
    assert signal.detector == "interrupt"
    assert signal.text == "actually do it this way instead"
    assert signal.trigger_index == 0
    assert signal.signal == weak("bare_marker")


def test_iter_review_comment_signals_with_injected_format() -> None:
    pattern = re.compile(r"^NIT: (.+)$", re.MULTILINE)
    fmt = ReviewFormat(
        name="tiny",
        pattern=pattern,
        extract=lambda text: tuple(
            ReviewComment(file=None, line_start=None, line_end=None, comment=match.group(1))
            for match in pattern.finditer(text)
        ),
    )
    events = [
        assistant("a0", "here is the code"),
        user("u0", "NIT: rename this var\nNIT: add a test"),
    ]
    signals = list(iter_review_comment_signals(events, (fmt,)))
    assert [signal.text for signal in signals] == ["rename this var", "add a test"]
    assert all(signal.detector == "review_comment" for signal in signals)
    assert signals[0].evidence == {"format": "tiny", "file": None, "line_start": None, "line_end": None}
    assert signals[0].trigger_index == 0


def test_iter_plan_reentry_signals_smoke() -> None:
    events = [
        assistant("a0", "", blocks=(ToolUseBlock(id=ToolUseId("e1"), name="Edit", input={"file_path": "/x"}),)),
        ModeEvent(session_id=SESSION, channel="mode", value="plan"),
        user("u0", "reconsider the plan, this is wrong"),
    ]
    assert list(iter_review_comment_signals(events, ())) == []
    reentries = list(iter_plan_reentry_signals(events))
    assert len(reentries) == 1
    assert reentries[0].kind == PLAN_REVIEW
    assert reentries[0].detector == "plan_reentry"
    assert reentries[0].text == "reconsider the plan, this is wrong"
    assert reentries[0].lower_bound == 0


def test_feedback_store_round_trip_preserves_signal(tmp_path: Path) -> None:
    candidate = FeedbackCandidate(
        dedup_key=dedup_key("sess-1", "transcript_message", "store me"),
        source_kind=TRANSCRIPT_MESSAGE,
        occurred_at=BASE,
        text="store me",
        context=ContextSnapshot(before=(), trigger=None, after=()),
        session_id=SESSION,
        origin_path=None,
        origin_uuid="u0",
        cc_version="1.2.3",
        payload={"detector": "transcript_message"},
        signal=weak("noisy", durable=False),
    )

    async def go() -> tuple[int, Stats, list[dict[str, object]], list[dict[str, object]]]:
        async with await FeedbackStore.open(tmp_path / "feedback.db") as store:
            inserted = await store.record_file_scan("/t.jsonl", 1.0, [candidate])
            return inserted, await store.stats(), await store.recent(), await store.events()

    inserted, stats, recent, events = anyio.run(go)
    assert inserted == 1
    assert stats.total == 1
    assert stats.files == 1
    assert stats.by_source == {"transcript_message": 1}
    assert [row["text"] for row in recent] == ["store me"]
    payload_json = events[0]["payload_json"]
    assert isinstance(payload_json, str)
    payload = json.loads(payload_json)
    assert payload["detector"] == "transcript_message"
    assert from_payload(payload["signal"]) == weak("noisy", durable=False)
