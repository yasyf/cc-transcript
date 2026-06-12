from __future__ import annotations

import json
import re
from datetime import UTC, datetime, timedelta
from pathlib import Path

import anyio
import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.context import ContextWindow, capture_window
from cc_transcript.ids import EventRef, tool_digest
from cc_transcript.mining import (
    DENIAL_PREFIX,
    HIGH,
    INTERRUPT_REJECTION,
    LOW,
    MEDIUM,
    NOISE_FLOOR,
    NONE,
    PLAN_REVIEW,
    TRANSCRIPT_MESSAGE,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
    CandidateSignal,
    FeedbackCandidate,
    FeedbackStore,
    ReviewComment,
    ReviewFormat,
    Stats,
    dedup_key,
    iter_interrupt_marker_signals,
    iter_plan_reentry_signals,
    iter_plan_rejection_signals,
    iter_review_comment_signals,
    iter_tool_denial_signals,
    iter_user_message_signals,
    noise,
    weak,
)
from cc_transcript.mining.confidence import from_payload
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    EventUuid,
    ModeEvent,
    SessionId,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    UserEvent,
)

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("sess-1")


def meta(uuid: str, *, secs: int = 0) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
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


def mode(value: str = "normal") -> ModeEvent:
    return ModeEvent(session_id=SESSION, channel="mode", value=value)


def denial_content(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def anchor(uuid: str) -> EventRef:
    return EventRef(SESSION, EventUuid(uuid))


def test_dedup_key_stable_and_content_derived() -> None:
    assert dedup_key("sess-1", "transcript_message", "hi") == dedup_key("sess-1", "transcript_message", "hi")
    assert dedup_key("sess-1", "transcript_message", "hi") != dedup_key("sess-1", "transcript_message", "bye")
    assert len(dedup_key("a", "b")) == 64


def test_capture_window_wraps_turns_around_the_feedback_anchor() -> None:
    events = [
        user("u0", "first ask"),
        assistant("a0", "working on it", blocks=(TextBlock("working on it"),), secs=1),
        user("u1", "the feedback", secs=2),
        assistant("a1", "fixed", blocks=(TextBlock("fixed"),), secs=3),
    ]
    window = capture_window(SessionActivity.from_events(SESSION, events), anchor("u1"))
    assert isinstance(window, ContextWindow)
    assert window.anchor == anchor("u1")
    assert window.fidelity == "full"
    assert [ref.preview for ref in window.before] == ["user: first ask\nassistant: working on it"]
    assert window.trigger is not None
    assert window.trigger.preview == "user: the feedback\nassistant: fixed"
    assert window.after == ()


def test_capture_window_previews_carry_tool_actions_and_digests() -> None:
    bash_input = {"command": "rm -rf build"}
    edit_input = {"file_path": "/a.py", "old_string": "x", "new_string": "y"}
    events = [
        user("u0", "go ahead"),
        assistant(
            "a0",
            blocks=(
                ToolUseBlock(id=ToolUseId("t1"), name="Bash", input=bash_input),
                ToolUseBlock(id=ToolUseId("t2"), name="Edit", input=edit_input),
            ),
            secs=1,
        ),
        user("u1", "no, stop", secs=2),
    ]
    window = capture_window(SessionActivity.from_events(SESSION, events), anchor("u1"))
    assert window.before[-1].preview == "user: go ahead\nrm -rf build\nEdit /a.py\n- x\n+ y"
    assert window.before[-1].tool_digests == (tool_digest("Bash", bash_input), tool_digest("Edit", edit_input))


def test_capture_window_clips_previews_to_the_persisted_budget() -> None:
    events = [
        user("u0", "go"),
        assistant(
            "a0", blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Bash", input={"command": "x" * 300}),), secs=1
        ),
        user("u1", "feedback", secs=2),
    ]
    window = capture_window(SessionActivity.from_events(SESSION, events), anchor("u1"), preview_chars=50)
    assert window.preview_chars == 50
    assert f"{'x' * 50}…(+250ch)" in window.before[-1].preview


def test_iter_user_message_signals_filters_and_sets_trigger() -> None:
    events = [
        user("u0", "   "),
        user("u1", "[Request interrupted by user]"),
        user("u2", "real feedback here"),
        assistant("a0", "ok"),
        user("u3", "more feedback"),
    ]
    signals = list(iter_user_message_signals(events))
    assert [signal.event_uuid for signal in signals] == [EventUuid("u2"), EventUuid("u3")]
    assert signals[0].trigger_index is None
    assert signals[1].trigger_index == 3
    assert signals[0].kind == TRANSCRIPT_MESSAGE
    assert all(signal.detector == "transcript_message" for signal in signals)
    assert signals[0].signal == CandidateSignal(MEDIUM, ("user_message",))
    assert signals[1].signal == CandidateSignal(MEDIUM, ("user_message", "short_followup", "trigger_proximate"))


@pytest.mark.parametrize(
    ("events", "expected"),
    [
        pytest.param(
            [assistant("a0", "made the change"), user("u0", "no, revert to the old parser")],
            CandidateSignal(HIGH, ("user_message", "trigger_proximate")),
            id="strong_when_tightly_following_trigger",
        ),
        pytest.param(
            [assistant("a0", "made the change"), mode(), mode(), user("u0", "please rework the parser entirely")],
            CandidateSignal(MEDIUM, ("user_message",)),
            id="firm_when_distant_from_trigger",
        ),
        pytest.param(
            [user("u0", "ok then")],
            CandidateSignal(LOW, ("user_message", "short_followup")),
            id="weak_short_followup",
        ),
        pytest.param(
            [assistant("a0", "made the change"), user("u0", "no stop")],
            CandidateSignal(MEDIUM, ("user_message", "short_followup", "trigger_proximate")),
            id="short_followup_demotion_offsets_proximity_bump",
        ),
        pytest.param(
            [assistant("a0", "made the change"), user("u0", "<system-reminder>compact summary</system-reminder>")],
            CandidateSignal(NONE, ("structural_only",)),
            id="noise_structural_only_despite_proximity",
        ),
    ],
)
def test_user_message_confidence_calibration(events: list[object], expected: CandidateSignal) -> None:
    assert [signal.signal for signal in iter_user_message_signals(events)] == [expected]


def test_structural_user_message_scores_below_noise_floor() -> None:
    signals = list(iter_user_message_signals([user("u0", "<system-reminder>compacted</system-reminder>")]))
    assert signals[0].signal.confidence < NOISE_FLOOR


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
    assert signal.signal == CandidateSignal(HIGH, ("embedded_text", "substantive"))


@pytest.mark.parametrize(
    ("said", "expected"),
    [
        pytest.param(
            "use the storage adapter instead",
            CandidateSignal(HIGH, ("embedded_text", "substantive")),
            id="substantive_promotes_to_strong",
        ),
        pytest.param("no", CandidateSignal(MEDIUM, ("embedded_text",)), id="terse_stays_firm"),
        pytest.param(
            "maybe try the storage adapter instead",
            CandidateSignal(MEDIUM, ("embedded_text", "substantive", "hedged")),
            id="hedged_demotes_back_to_firm",
        ),
    ],
)
def test_tool_denial_embedded_text_calibration(said: str, expected: CandidateSignal) -> None:
    events = [
        assistant("a0", blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Bash", input={"command": "rm -rf /"}),)),
        user(
            "u0",
            blocks=(ToolResultBlock(tool_use_id=ToolUseId("t1"), content=denial_content(said), is_error=True),),
        ),
    ]
    assert [signal.signal for signal in iter_tool_denial_signals(events)] == [expected]


@pytest.mark.parametrize(
    ("followup", "expected"),
    [
        pytest.param(
            "actually run it in the sandbox first",
            CandidateSignal(LOW, ("bare_marker",)),
            id="substantive_followup_stays_weak",
        ),
        pytest.param(
            "<system-reminder>hook output</system-reminder>",
            CandidateSignal(NONE, ("structural_only",)),
            id="structural_only_followup_is_noise",
        ),
    ],
)
def test_tool_denial_bare_marker_followup_calibration(followup: str, expected: CandidateSignal) -> None:
    events = [
        assistant("a0", blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Bash", input={"command": "ls"}),)),
        user(
            "u0",
            blocks=(ToolResultBlock(tool_use_id=ToolUseId("t1"), content=f"{DENIAL_PREFIX}.", is_error=True),),
        ),
        user("u1", followup),
    ]
    signals = list(iter_tool_denial_signals(events))
    assert [signal.text for signal in signals] == [followup]
    assert [signal.signal for signal in signals] == [expected]


@pytest.mark.parametrize(
    ("said", "expected"),
    [
        pytest.param(
            "the plan skips the data migration step",
            CandidateSignal(HIGH, ("embedded_text", "substantive")),
            id="substantive_rejection_promotes_to_strong",
        ),
        pytest.param("no", CandidateSignal(MEDIUM, ("embedded_text",)), id="terse_rejection_stays_firm"),
    ],
)
def test_plan_rejection_signal_calibration(said: str, expected: CandidateSignal) -> None:
    events = [
        assistant(
            "a0",
            blocks=(ToolUseBlock(id=ToolUseId("t1"), name="ExitPlanMode", input={"plan": "do it"}),),
        ),
        user(
            "u0",
            blocks=(ToolResultBlock(tool_use_id=ToolUseId("t1"), content=denial_content(said), is_error=True),),
        ),
    ]
    assert [signal.signal for signal in iter_plan_rejection_signals(events)] == [expected]


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


def test_iter_interrupt_marker_signals_structural_only_correction_is_noise() -> None:
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
        user("u1", "<system-reminder>session resumed</system-reminder>"),
    ]
    signals = list(iter_interrupt_marker_signals(events))
    assert len(signals) == 1
    assert signals[0].text == "<system-reminder>session resumed</system-reminder>"
    assert signals[0].signal == noise("structural_only")
    assert signals[0].signal.confidence < NOISE_FLOOR


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
    assert signals[0].signal == CandidateSignal(HIGH, ("format_match", "substantive"))


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
    assert reentries[0].signal == CandidateSignal(HIGH, ("reentry_after_edit", "substantive"))


def test_feedback_store_round_trip_preserves_signal_window_and_ref(tmp_path: Path) -> None:
    events = [
        user("u0", "first ask"),
        assistant("a0", "working on it", blocks=(TextBlock("working on it"),), secs=1),
        user("u1", "store me", secs=2),
    ]
    window = capture_window(SessionActivity.from_events(SESSION, events), anchor("u1"))
    candidate = FeedbackCandidate(
        dedup_key=dedup_key("sess-1", "transcript_message", "store me"),
        source_kind=TRANSCRIPT_MESSAGE,
        occurred_at=BASE,
        text="store me",
        window=window,
        ref=anchor("u1"),
        session_id=SESSION,
        cc_version="1.2.3",
        signal=weak("noisy", durable=False),
        payload={"detector": "transcript_message"},
    )

    async def go() -> tuple[int, int, Stats, list[dict[str, object]], list[dict[str, object]]]:
        async with await FeedbackStore.open(tmp_path / "feedback.db") as store:
            inserted = await store.record_file_scan("/t.jsonl", 1.0, [candidate])
            rescanned = await store.record_file_scan("/t.jsonl", 2.0, [candidate])
            return inserted, rescanned, await store.stats(), await store.recent(), await store.events()

    inserted, rescanned, stats, recent, rows = anyio.run(go)
    assert inserted == 1
    assert rescanned == 0
    assert stats.total == 1
    assert stats.files == 1
    assert stats.by_source == {"transcript_message": 1}
    assert [row["text"] for row in recent] == ["store me"]
    assert (rows[0]["session_id"], rows[0]["event_uuid"]) == ("sess-1", "u1")
    context_json = rows[0]["context_json"]
    assert isinstance(context_json, str)
    assert ContextWindow.from_json(context_json) == window
    payload_json = rows[0]["payload_json"]
    assert isinstance(payload_json, str)
    payload = json.loads(payload_json)
    assert payload["detector"] == "transcript_message"
    assert from_payload(payload["signal"]) == weak("noisy", durable=False)
