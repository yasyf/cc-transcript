from __future__ import annotations

import json
import re
from functools import partial
from pathlib import Path

import anyio
import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.context import ContextWindow, capture_window
from cc_transcript.ids import EventRef, tool_digest
from cc_transcript.mining import (
    ANSWERED_PREFIX,
    ANSWERED_TRAILER,
    DENIAL_PREFIX,
    HIGH,
    INTERRUPT_REJECTION,
    LOW,
    MEDIUM,
    NOISE_FLOOR,
    NONE,
    PLAN_REVIEW,
    QUESTION_ANSWER,
    TRANSCRIPT_MESSAGE,
    USER_SAID_MARKER,
    USER_SAID_TRAILER,
    CallableReviewFormat,
    CandidateSignal,
    FeedbackCandidate,
    FeedbackStore,
    MiningSpec,
    ReviewComment,
    ReviewSpec,
    Stats,
    StructuredFormat,
    dedup_key,
    extract_structured,
    noise,
    weak,
)
from cc_transcript.mining.confidence import from_payload
from cc_transcript.mining.signals import (
    iter_ask_user_question_signals,
    iter_interrupt_marker_signals,
    iter_plan_reentry_signals,
    iter_plan_rejection_signals,
    iter_review_comment_signals,
    iter_tool_denial_signals,
    iter_user_message_signals,
    last_edit_index,
)
from cc_transcript.mining.spec import ProvenanceSpec, classify_provenance
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
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
from tests.support import (
    BASE,
    MATCHER_ANSWER,
    MATCHER_LABELS,
    MATCHER_QUESTION,
    ROUND1_CONTENT,
    TOMBSTONE_LABELS,
    TOMBSTONE_QUESTION,
)
from tests.support import (
    assistant as _assistant,
)
from tests.support import (
    user as _user,
)

SESSION = SessionId("sess-1")
SPEC = MiningSpec()


def review_spec(
    *formats: CallableReviewFormat,
    surfaces: frozenset[str],
    structured_formats: tuple[StructuredFormat, ...] = (),
) -> MiningSpec:
    return MiningSpec(
        review=ReviewSpec(callable_formats=formats, structured_formats=structured_formats, surfaces=surfaces)
    )


user = partial(_user, session=SESSION)
assistant = partial(_assistant, session=SESSION)


def mode(value: str = "normal") -> ModeEvent:
    return ModeEvent(session_id=SESSION, channel="mode", value=value)


def denial_content(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def anchor(uuid: str) -> EventRef:
    return EventRef(SESSION, EventUuid(uuid))


def question(text: str, header: str, *labels: str, multi_select: bool = False) -> dict[str, object]:
    return {
        "question": text,
        "header": header,
        "multiSelect": multi_select,
        "options": [{"label": label} for label in labels],
    }


def answered(pairs: str) -> str:
    return f"{ANSWERED_PREFIX}{pairs}{ANSWERED_TRAILER}"


def answered_round(
    questions: list[dict[str, object]], content: str, *, is_error: bool = False
) -> list[AssistantEvent | UserEvent]:
    return [
        assistant(
            "a0",
            blocks=(ToolUseBlock(id=ToolUseId("q1"), name="AskUserQuestion", input={"questions": questions}),),
        ),
        user("u0", blocks=(ToolResultBlock(tool_use_id=ToolUseId("q1"), content=content, is_error=is_error),)),
    ]


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
        assistant("a0", blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Bash", input={"command": "x" * 300}),), secs=1),
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
    signals = list(iter_user_message_signals(events, SPEC))
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
    assert [signal.signal for signal in iter_user_message_signals(events, SPEC)] == [expected]


def test_structural_user_message_scores_below_noise_floor() -> None:
    signals = list(iter_user_message_signals([user("u0", "<system-reminder>compacted</system-reminder>")], SPEC))
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
    signals = list(iter_tool_denial_signals(events, SPEC))
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
    assert [signal.signal for signal in iter_tool_denial_signals(events, SPEC)] == [expected]


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
    signals = list(iter_tool_denial_signals(events, SPEC))
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
    assert [signal.signal for signal in iter_plan_rejection_signals(events, SPEC)] == [expected]


def test_iter_interrupt_marker_signals_extracts_correction() -> None:
    events = [
        assistant("a0", "doing work"),
        user(
            "u0",
            blocks=(
                ToolResultBlock(tool_use_id=ToolUseId("t9"), content="[Request interrupted by user]", is_error=True),
            ),
        ),
        user("u1", "actually do it this way instead"),
    ]
    signals = list(iter_interrupt_marker_signals(events, SPEC))
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
                ToolResultBlock(tool_use_id=ToolUseId("t9"), content="[Request interrupted by user]", is_error=True),
            ),
        ),
        user("u1", "<system-reminder>session resumed</system-reminder>"),
    ]
    signals = list(iter_interrupt_marker_signals(events, SPEC))
    assert len(signals) == 1
    assert signals[0].text == "<system-reminder>session resumed</system-reminder>"
    assert signals[0].signal == noise("structural_only")
    assert signals[0].signal.confidence < NOISE_FLOOR


def test_iter_review_comment_signals_with_injected_format() -> None:
    pattern = re.compile(r"^NIT: (.+)$", re.MULTILINE)
    fmt = CallableReviewFormat(
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
    signals = list(iter_review_comment_signals(events, review_spec(fmt, surfaces=frozenset({"typed"}))))
    assert [signal.text for signal in signals] == ["rename this var", "add a test"]
    assert all(signal.detector == "review_comment" for signal in signals)
    assert signals[0].evidence == {
        "format": "tiny",
        "file": None,
        "line_start": None,
        "line_end": None,
        "provenance": "typed",
    }
    assert signals[0].trigger_index == 0
    assert signals[0].signal == CandidateSignal(HIGH, ("format_match", "substantive"))


def workflow_result(content: str, *, sidechain: bool = False) -> tuple[AssistantEvent, UserEvent]:
    use = assistant("a0", blocks=(ToolUseBlock(id=ToolUseId("w1"), name="Bash", input={"command": "verify.sh"}),))
    result = UserEvent(
        meta=EntryMeta(
            uuid=EventUuid("u0"),
            parent_uuid=None,
            session_id=SESSION,
            timestamp=BASE,
            cwd="/repo",
            git_branch="main",
            cc_version=CcVersion("1.2.3"),
            is_sidechain=sidechain,
            is_meta=False,
            entrypoint="cli",
            is_compact_summary=False,
            is_visible_in_transcript_only=False,
        ),
        text="",
        blocks=(ToolResultBlock(tool_use_id=ToolUseId("w1"), content=content, is_error=False),),
        interrupted=False,
    )
    return use, result


def test_iter_review_comment_signals_surfaced_structured_tool_result() -> None:
    fmt = StructuredFormat(name="workflow", finding_keys=("findings",))
    payload = json.dumps({"findings": [{"file": "a.py", "line": "24-51", "comment": "guard against None"}]})
    use, result = workflow_result(payload)
    signals = list(
        iter_review_comment_signals(
            [use, result], review_spec(surfaces=frozenset({"typed", "surfaced"}), structured_formats=(fmt,))
        )
    )
    assert [signal.text for signal in signals] == ["guard against None"]
    signal = signals[0]
    assert signal.detector == "review_comment"
    assert signal.evidence == {
        "format": "workflow",
        "file": "a.py",
        "line_start": 24,
        "line_end": 51,
        "provenance": "surfaced",
    }
    assert signal.trigger_index is None


def test_iter_review_comment_signals_subagent_result_is_claude_and_excluded() -> None:
    fmt = StructuredFormat(name="workflow")
    use = assistant("a0", blocks=(ToolUseBlock(id=ToolUseId("t1"), name="Agent", input={"prompt": "review"}),))
    result = user(
        "u0",
        blocks=(
            ToolResultBlock(
                tool_use_id=ToolUseId("t1"),
                content=json.dumps([{"file": "b.py", "line": 9, "comment": "rename"}]),
                is_error=False,
            ),
        ),
    )
    surfaced = iter_review_comment_signals(
        [use, result], review_spec(surfaces=frozenset({"surfaced"}), structured_formats=(fmt,))
    )
    assert list(surfaced) == []
    signals = list(
        iter_review_comment_signals(
            [use, result], review_spec(surfaces=frozenset({"claude"}), structured_formats=(fmt,))
        )
    )
    assert [signal.evidence["provenance"] for signal in signals] == ["claude"]


def test_iter_review_comment_signals_default_call_tags_typed_provenance() -> None:
    pattern = re.compile(r"^NIT: (.+)$", re.MULTILINE)
    fmt = CallableReviewFormat(
        name="tiny",
        pattern=pattern,
        extract=lambda text: tuple(
            ReviewComment(file=None, line_start=None, line_end=None, comment=match.group(1))
            for match in pattern.finditer(text)
        ),
    )
    events = [assistant("a0", "code"), user("u0", "NIT: rename this var")]
    signals = list(iter_review_comment_signals(events, review_spec(fmt, surfaces=frozenset({"typed"}))))
    assert len(signals) == 1
    assert signals[0].evidence == {
        "format": "tiny",
        "file": None,
        "line_start": None,
        "line_end": None,
        "provenance": "typed",
    }
    assert signals[0].trigger_index == 0


def test_structured_format_fix_keys_append_suggested_fix() -> None:
    fmt = StructuredFormat(name="x", comment_keys=("description",), fix_keys=("suggested_fix",))
    payload = json.dumps(
        {"findings": [{"path": "c.py", "line": 9, "description": "leaks a fd", "suggested_fix": "use with"}]}
    )
    comments = [comment for _, comment in extract_structured(payload, (fmt,))]
    assert comments == [ReviewComment(file="c.py", line_start=9, line_end=9, comment="leaks a fd use with")]
    bare = json.dumps({"findings": [{"path": "c.py", "line": 9, "description": "leaks a fd"}]})
    assert [c.comment for _, c in extract_structured(bare, (fmt,))] == ["leaks a fd"]


def test_extract_structured_tolerates_non_json_and_shapes() -> None:
    fmt = StructuredFormat(name="x")
    assert list(extract_structured("not json at all", (fmt,))) == []
    nested = json.dumps({"result": {"confirmed_bugs": [{"path": "c.py", "line": 96, "message": "fix"}]}})
    comments = [comment for _, comment in extract_structured(nested, (fmt,))]
    assert comments == [ReviewComment(file="c.py", line_start=96, line_end=96, comment="fix")]


def test_classify_provenance() -> None:
    spec = ProvenanceSpec()
    assert classify_provenance(spec, None, is_sidechain=False) == "typed"
    assert classify_provenance(spec, "Bash", is_sidechain=False) == "surfaced"
    assert classify_provenance(spec, "Bash", is_sidechain=True) == "claude"
    assert classify_provenance(spec, "Agent", is_sidechain=False) == "claude"
    assert classify_provenance(spec, "Task", is_sidechain=False) == "claude"
    assert classify_provenance(spec, "mcp__conductor__Task", is_sidechain=False) == "claude"


def test_iter_plan_reentry_signals_smoke() -> None:
    events = [
        assistant("a0", "", blocks=(ToolUseBlock(id=ToolUseId("e1"), name="Edit", input={"file_path": "/x"}),)),
        ModeEvent(session_id=SESSION, channel="mode", value="plan"),
        user("u0", "reconsider the plan, this is wrong"),
    ]
    assert list(iter_review_comment_signals(events, review_spec(surfaces=frozenset({"typed"})))) == []
    reentries = list(iter_plan_reentry_signals(events, SPEC))
    assert len(reentries) == 1
    assert reentries[0].kind == PLAN_REVIEW
    assert reentries[0].detector == "plan_reentry"
    assert reentries[0].text == "reconsider the plan, this is wrong"
    assert reentries[0].lower_bound == 0
    assert reentries[0].signal == CandidateSignal(HIGH, ("reentry_after_edit", "substantive"))


def test_last_edit_index_matches_ccx_mcp_edit_suffix() -> None:
    events = [
        assistant(
            "a0",
            "",
            blocks=(
                ToolUseBlock(
                    id=ToolUseId("e1"),
                    name="mcp__cc-context__ccx_code_edit",
                    input={"file_path": "/x"},
                ),
            ),
        ),
        ModeEvent(session_id=SESSION, channel="mode", value="plan"),
        user("u0", "reconsider the plan, this is wrong"),
    ]
    assert last_edit_index(events, 2, SPEC) == 0


def test_iter_ask_user_question_signals_single_option_pick() -> None:
    events = answered_round(
        [question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
        answered('"Which adapter?"="Storage (Recommended)"'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.kind == QUESTION_ANSWER
    assert signal.detector == "ask_user_question"
    assert signal.text == "Storage (Recommended)"
    assert signal.trigger_index == 0
    assert signal.signal == weak("option_pick")
    assert signal.evidence == {
        "question": "Which adapter?",
        "header": "Adapter",
        "multi_select": False,
        "option_pick": True,
        "picked_labels": ["Storage (Recommended)"],
        "recommended_pick": True,
    }


def test_iter_ask_user_question_signals_freeform_nested_quotes_and_commas() -> None:
    events = answered_round(
        [
            question(TOMBSTONE_QUESTION, "Enforcement", *TOMBSTONE_LABELS),
            question(MATCHER_QUESTION, "Matcher", *MATCHER_LABELS),
        ],
        ROUND1_CONTENT,
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert [signal.text for signal in signals] == ["Advisory warn (Recommended)", MATCHER_ANSWER]
    pick, freeform = signals
    assert pick.signal == weak("option_pick")
    assert pick.evidence == {
        "question": TOMBSTONE_QUESTION,
        "header": "Enforcement",
        "multi_select": False,
        "option_pick": True,
        "picked_labels": ["Advisory warn (Recommended)"],
        "recommended_pick": True,
    }
    assert freeform.signal == CandidateSignal(HIGH, ("freeform_answer", "substantive"))
    assert freeform.evidence == {
        "question": MATCHER_QUESTION,
        "header": "Matcher",
        "multi_select": False,
        "option_pick": False,
        "picked_labels": ["NLP-only (as originally floated)"],
        "recommended_pick": False,
    }


def test_iter_ask_user_question_signals_ordinal_shorthand_is_freeform() -> None:
    events = answered_round(
        [
            question(
                "What should the two built-in edit-text contexts be named?",
                "Names",
                "BeforeEdit / AfterEdit (Recommended)",
                "EditOld / EditNew",
                "Preimage / Postimage",
            )
        ],
        answered(
            '"What should the two built-in edit-text contexts be named?"='
            '"1, but shouldnt those be default contexts? were they not before?"'
        ),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.text == "1, but shouldnt those be default contexts? were they not before?"
    assert signal.evidence["picked_labels"] == ["BeforeEdit / AfterEdit (Recommended)"]
    assert signal.evidence["option_pick"] is False
    assert signal.evidence["recommended_pick"] is True
    assert signal.signal == CandidateSignal(HIGH, ("freeform_answer", "substantive"))


def test_iter_ask_user_question_signals_multiselect_subset_join_is_option_pick() -> None:
    events = answered_round(
        [
            question(
                "Which docs pages?",
                "Docs",
                "Getting started",
                "How it works, end to end",
                "CLI reference",
                multi_select=True,
            )
        ],
        answered('"Which docs pages?"="Getting started, How it works, end to end"'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.text == "Getting started, How it works, end to end"
    assert signal.signal == weak("option_pick")
    assert signal.evidence["multi_select"] is True
    assert signal.evidence["option_pick"] is True
    assert signal.evidence["picked_labels"] == ["Getting started", "How it works, end to end"]
    assert signal.evidence["recommended_pick"] is False


def test_iter_ask_user_question_signals_preview_split_into_evidence() -> None:
    events = answered_round(
        [question("How far should enable go?", "Install", "Full turnkey (Recommended)", "Install only")],
        answered('"How far should enable go?"="Full turnkey (Recommended)" selected preview:\n$ tool enable\n==> done'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.text == "Full turnkey (Recommended)"
    assert signal.signal == weak("option_pick")
    assert signal.evidence["preview"] == "$ tool enable\n==> done"
    assert signal.evidence["picked_labels"] == ["Full turnkey (Recommended)"]
    assert "notes" not in signal.evidence


def test_iter_ask_user_question_signals_option_pick_with_notes_scores_on_notes() -> None:
    events = answered_round(
        [question("Which adapter?", "Adapter", "Storage (Recommended)", "Memory")],
        answered('"Which adapter?"="Storage (Recommended)" notes: never store secrets to the memory file'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.text == "never store secrets to the memory file"
    assert signal.evidence["option_pick"] is True
    assert signal.evidence["picked_labels"] == ["Storage (Recommended)"]
    assert signal.evidence["notes"] == "never store secrets to the memory file"
    assert signal.signal == CandidateSignal(HIGH, ("freeform_answer", "substantive"))


def test_iter_ask_user_question_signals_no_option_selected_notes() -> None:
    events = answered_round(
        [question("Add CI coverage?", "CI", "Add the guard", "Skip CI guard")],
        answered('"Add CI coverage?"=(no option selected) notes: fix it in capt-hook so it skips invalid files'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    signal = signals[0]
    assert signal.text == "fix it in capt-hook so it skips invalid files"
    assert signal.signal == CandidateSignal(HIGH, ("freeform_answer", "substantive"))
    assert signal.evidence["option_pick"] is False
    assert signal.evidence["picked_labels"] == []
    assert signal.evidence["recommended_pick"] is False
    assert signal.evidence["notes"] == "fix it in capt-hook so it skips invalid files"
    assert "preview" not in signal.evidence


def test_iter_ask_user_question_signals_omitted_pair_skipped() -> None:
    events = answered_round(
        [question("First unanswered?", "One", "A", "B"), question("Second answered?", "Two", "C", "D")],
        answered('"Second answered?"="C"'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert [signal.evidence["question"] for signal in signals] == ["Second answered?"]
    assert signals[0].evidence["picked_labels"] == ["C"]
    assert signals[0].signal == weak("option_pick")


def test_iter_ask_user_question_signals_malformed_question_skipped() -> None:
    events = answered_round(
        [
            {"header": "Broken", "multiSelect": False, "options": [{"label": "A"}]},
            question("Second answered?", "Two", "C", "D"),
        ],
        answered('"Second answered?"="C"'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert [signal.evidence["question"] for signal in signals] == ["Second answered?"]
    assert signals[0].evidence["picked_labels"] == ["C"]


def test_iter_ask_user_question_signals_errored_result_yields_nothing() -> None:
    events = answered_round(
        [question("Which adapter?", "Adapter", "Storage", "Memory")],
        answered('"Which adapter?"="Storage"'),
        is_error=True,
    )
    assert list(iter_ask_user_question_signals(events, SPEC)) == []


def test_iter_ask_user_question_signals_unpaired_result_skipped() -> None:
    events = [
        assistant(
            "a0",
            blocks=(
                ToolUseBlock(
                    id=ToolUseId("q1"),
                    name="AskUserQuestion",
                    input={"questions": [question("Which adapter?", "Adapter", "Storage", "Memory")]},
                ),
            ),
        ),
        user(
            "u0",
            blocks=(
                ToolResultBlock(
                    tool_use_id=ToolUseId("q9"), content=answered('"Which adapter?"="Storage"'), is_error=False
                ),
            ),
        ),
    ]
    assert list(iter_ask_user_question_signals(events, SPEC)) == []


def test_iter_ask_user_question_signals_answer_ending_in_quote_not_overstripped() -> None:
    events = answered_round(
        [question("Which name?", "Name", "Alpha", "Beta")],
        answered('"Which name?"="call it "beta""'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert len(signals) == 1
    assert signals[0].text == 'call it "beta"'
    assert signals[0].evidence["picked_labels"] == []
    assert signals[0].evidence["option_pick"] is False


def test_iter_ask_user_question_signals_answer_embedding_later_anchor() -> None:
    events = answered_round(
        [question("First?", "One", "A", "B"), question("Second?", "Two", "C", "D")],
        answered('"First?"="I think "Second?"=maybe", "Second?"="C"'),
    )
    signals = list(iter_ask_user_question_signals(events, SPEC))
    assert [signal.evidence["question"] for signal in signals] == ["First?", "Second?"]
    assert signals[0].text == 'I think "Second?"=maybe'
    assert signals[1].evidence["picked_labels"] == ["C"]


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
