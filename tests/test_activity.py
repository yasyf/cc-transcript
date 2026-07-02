from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from functools import partial
from typing import TYPE_CHECKING, Any

import anyio
import pytest

from cc_transcript.activity import Edit, SessionActivity, ToolUse, Turn, hunk_overlap, native_user_classifier
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    ModeEvent,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.tools import BashCall, EditCall, Hunk, OtherCall

if TYPE_CHECKING:
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("11111111-1111-1111-1111-111111111111")


def meta(uuid: str, *, secs: int = 0, is_meta: bool = False, is_sidechain: bool = False) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SESSION,
        timestamp=BASE + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=is_sidechain,
        is_meta=is_meta,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(
    uuid: str,
    text: str = "",
    *,
    blocks: tuple[ContentBlock, ...] = (),
    secs: int = 0,
    interrupted: bool = False,
    is_meta: bool = False,
    is_sidechain: bool = False,
) -> UserEvent:
    return UserEvent(
        meta=meta(uuid, secs=secs, is_meta=is_meta, is_sidechain=is_sidechain),
        text=text,
        blocks=blocks,
        interrupted=interrupted,
    )


def assistant(uuid: str, text: str = "", *, blocks: tuple[ContentBlock, ...] = (), secs: int = 0) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-7", text=text, blocks=blocks, stop_reason=None, usage=None
    )


def bash(id: str, command: str) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name="Bash", input={"command": command})


def edit(id: str, path: str, old: str, new: str) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name="Edit", input={"file_path": path, "old_string": old, "new_string": new})


def write(id: str, path: str, content: str) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name="Write", input={"file_path": path, "content": content})


def result(id: str, content: str = "ok") -> ToolResultBlock:
    return ToolResultBlock(tool_use_id=ToolUseId(id), content=content, is_error=False)


def ref(uuid: str, tool_use_id: str | None = None) -> EventRef:
    return EventRef(SESSION, EventUuid(uuid), ToolUseId(tool_use_id) if tool_use_id else None)


def activity(*events: TranscriptEvent) -> SessionActivity:
    return SessionActivity.from_events(SESSION, events)


@pytest.mark.parametrize(
    ("event", "expected"),
    [
        pytest.param(user("u0", "fix the bug"), True, id="real_prompt"),
        pytest.param(user("u0", "reminder", is_meta=True), False, id="meta"),
        pytest.param(user("u0", "subagent ask", is_sidechain=True), False, id="sidechain"),
        pytest.param(user("u0", "[Request interrupted by user]", interrupted=True), False, id="interruption"),
        pytest.param(user("u0", "", blocks=(result("t1"),)), False, id="tool_result_only"),
        pytest.param(user("u0", "   "), False, id="whitespace_only"),
    ],
)
def test_native_user_classifier(event: UserEvent, expected: bool) -> None:
    assert native_user_classifier(event) is expected


def test_from_events_opens_a_turn_per_qualifying_prompt() -> None:
    act = activity(
        user("u0", "first ask"),
        assistant("a0", "on it", secs=1),
        user("u1", "", blocks=(result("t1"),), secs=2),
        assistant("a1", "done", secs=3),
        user("u2", "second ask", secs=4),
        assistant("a2", "ack", secs=5),
    )
    assert [turn.prompt for turn in act.turns] == ["first ask", "second ask"]
    assert [turn.index for turn in act.turns] == [0, 1]
    assert [len(turn.events) for turn in act.turns] == [4, 2]


def test_events_before_first_prompt_form_turn_zero_with_empty_prompt() -> None:
    act = activity(
        user("u0", "injected", is_meta=True),
        assistant("a0", "preamble", secs=1),
        user("u1", "real ask", secs=2),
    )
    assert [turn.prompt for turn in act.turns] == ["", "real ask"]
    assert len(act.turns[0].events) == 2


def test_injected_classifier_controls_segmentation() -> None:
    events = [user("u0", "chatter"), user("u1", "go: build it", secs=1), user("u2", "more chatter", secs=2)]
    act = SessionActivity.from_events(SESSION, events, user_classifier=lambda e: e.text.startswith("go:"))
    assert [turn.prompt for turn in act.turns] == ["", "go: build it"]
    assert len(act.turns[1].events) == 2


def test_turn_timestamps_span_meta_bearing_events() -> None:
    act = activity(
        ModeEvent(session_id=SESSION, channel="mode", value="plan"),
        user("u0", "ask", secs=10),
        assistant("a0", "done", secs=25),
    )
    preamble, turn = act.turns
    assert (preamble.started_at, preamble.ended_at) == (None, None)
    assert turn.started_at == BASE + timedelta(seconds=10)
    assert turn.ended_at == BASE + timedelta(seconds=25)


def test_tool_uses_lift_every_block_with_matched_results() -> None:
    act = activity(
        user("u0", "run and edit"),
        assistant("a0", "", blocks=(bash("t1", "uv run pytest"), edit("t2", "/a.py", "x = 1", "x = 2")), secs=3),
        user("u1", "", blocks=(result("t1", "1 passed"),), secs=4),
    )
    uses = act.turns[0].tool_uses
    assert [type(use) for use in uses] == [ToolUse, ToolUse]
    first, second = uses
    assert first.ref == ref("a0", "t1")
    assert isinstance(first.call, BashCall)
    assert first.call.command == "uv run pytest"
    assert first.result == result("t1", "1 passed")
    assert first.turn_index == 0
    assert first.ts == BASE + timedelta(seconds=3)
    assert first.result_ts == BASE + timedelta(seconds=4)
    assert first.duration_ms == 1000
    assert isinstance(second.call, EditCall)
    assert second.result is None
    assert second.result_ts is None
    assert second.duration_ms is None
    assert first.call is uses[0].call


def test_malformed_tool_input_lifts_to_other_call() -> None:
    bad = ToolUseBlock(id=ToolUseId("t1"), name="Grep", input={"query": "^from", "path": "pkg"})
    act = activity(
        user("u0", "search"),
        assistant("a0", "", blocks=(bad, bash("t2", "ls")), secs=3),
    )
    first, second = act.turns[0].tool_uses
    assert isinstance(first.call, OtherCall)
    assert first.call.name == "Grep"
    assert first.call.raw == {"query": "^from", "path": "pkg"}
    assert isinstance(second.call, BashCall)


def test_turn_edits_derive_only_from_hunked_calls_with_a_file_path() -> None:
    act = activity(
        user("u0", "edit things"),
        assistant(
            "a0",
            "",
            blocks=(
                edit("t1", "/a.py", "x = 1", "x = 2"),
                write("t2", "/b.py", "print(1)"),
                bash("t3", "ls"),
                ToolUseBlock(id=ToolUseId("t4"), name="Read", input={"file_path": "/a.py"}),
            ),
            secs=1,
        ),
    )
    edits = act.turns[0].edits
    assert [(e.file_path, e.tool, e.hunks) for e in edits] == [
        ("/a.py", "Edit", (Hunk("x = 1", "x = 2"),)),
        ("/b.py", "Write", (Hunk("", "print(1)"),)),
    ]
    assert all(isinstance(e, Edit) and e.turn_index == 0 for e in edits)
    assert edits[0].ref == ref("a0", "t1")


def test_multiedit_lowers_to_one_edit_with_ordered_hunks() -> None:
    block = ToolUseBlock(
        id=ToolUseId("t1"),
        name="MultiEdit",
        input={
            "file_path": "/a.py",
            "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}],
        },
    )
    act = activity(user("u0", "go"), assistant("a0", "", blocks=(block,), secs=1))
    assert act.turns[0].edits == (
        Edit(
            file_path="/a.py",
            hunks=(Hunk("a", "b"), Hunk("c", "d")),
            tool="MultiEdit",
            ref=ref("a0", "t1"),
            turn_index=0,
            ts=BASE + timedelta(seconds=1),
        ),
    )


def test_session_edits_concatenate_turns_chronologically() -> None:
    act = activity(
        user("u0", "one"),
        assistant("a0", "", blocks=(edit("t1", "/a.py", "1", "2"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", "", blocks=(edit("t2", "/b.py", "3", "4"),), secs=3),
    )
    assert [(e.turn_index, e.file_path) for e in act.edits] == [(0, "/a.py"), (1, "/b.py")]


def test_turn_of_resolves_event_and_tool_refs_and_misses_to_none() -> None:
    act = activity(
        user("u0", "one"),
        assistant("a0", "", blocks=(edit("t1", "/a.py", "1", "2"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", "done", secs=3),
    )
    assert (found := act.turn_of(ref("a1"))) is not None and found.index == 1
    assert (found := act.turn_of(ref("a0", "t1"))) is not None and found.index == 0
    assert act.turn_of(ref("compacted-away")) is None
    assert isinstance(act.turns[0], Turn)


def edit_ladder() -> SessionActivity:
    return activity(
        user("u0", "one"),
        assistant("a0", "", blocks=(edit("t1", "/a.py", "old1", "new1"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", "", blocks=(edit("t2", "/b.py", "old2", "new2"),), secs=3),
        user("u2", "three", secs=4),
        assistant("a2", "", blocks=(edit("t3", "/a.py", "old3", "new3"),), secs=5),
        assistant("a3", "anchor here", secs=6),
        assistant("a4", "", blocks=(edit("t4", "/a.py", "old4", "new4"),), secs=7),
    )


def test_edits_before_windows_and_orders_newest_first() -> None:
    act = edit_ladder()
    anchor = ref("a3")
    assert [e.ref.tool_use_id for e in act.edits_before(anchor, lookback_turns=5)] == ["t3", "t2", "t1"]
    assert [e.ref.tool_use_id for e in act.edits_before(anchor, lookback_turns=1)] == ["t3", "t2"]
    assert [e.ref.tool_use_id for e in act.edits_before(anchor, lookback_turns=0)] == ["t3"]


def test_edits_before_excludes_anchor_edit_and_compacted_anchor() -> None:
    act = edit_ladder()
    assert [e.ref.tool_use_id for e in act.edits_before(ref("a2", "t3"), lookback_turns=5)] == ["t2", "t1"]
    assert act.edits_before(ref("gone"), lookback_turns=5) == ()


def test_edits_before_orders_blocks_within_one_event() -> None:
    act = activity(
        user("u0", "go"),
        assistant("a0", "", blocks=(edit("t1", "/a.py", "1", "2"), edit("t2", "/a.py", "3", "4")), secs=1),
    )
    assert [e.ref.tool_use_id for e in act.edits_before(ref("a0", "t2"), lookback_turns=0)] == ["t1"]
    assert act.edits_before(ref("a0", "t1"), lookback_turns=0) == ()


def test_edits_after_filters_file_and_orders_oldest_first() -> None:
    act = edit_ladder()
    anchor = ref("a0", "t1")
    assert [e.ref.tool_use_id for e in act.edits_after(anchor, file_path="/a.py", lookahead_turns=5)] == ["t3", "t4"]
    assert [e.ref.tool_use_id for e in act.edits_after(anchor, file_path="/b.py", lookahead_turns=5)] == ["t2"]
    assert act.edits_after(anchor, file_path="/a.py", lookahead_turns=0) == ()
    assert act.edits_after(ref("gone"), file_path="/a.py", lookahead_turns=5) == ()


def test_edits_after_includes_rest_of_anchor_turn_only_after_anchor() -> None:
    act = activity(
        user("u0", "go"),
        assistant("a0", "", blocks=(edit("t1", "/a.py", "1", "2"),), secs=1),
        assistant("a1", "anchor", secs=2),
        assistant("a2", "", blocks=(edit("t2", "/a.py", "3", "4"),), secs=3),
    )
    assert [e.ref.tool_use_id for e in act.edits_after(ref("a1"), file_path="/a.py", lookahead_turns=0)] == ["t2"]


@pytest.mark.parametrize(
    ("a", "b", "expected"),
    [
        pytest.param(Hunk("", "x = 1\ny = 2"), Hunk("x = 1\ny = 2", ""), 1.0, id="all_lines_present"),
        pytest.param(Hunk("", "x = 1\ny = 2"), Hunk("x = 1\nz = 3", ""), 0.5, id="half_present"),
        pytest.param(Hunk("", "  x  =  1  "), Hunk("x = 1", ""), 1.0, id="whitespace_normalized"),
        pytest.param(Hunk("", "x = 1\n\n   \n"), Hunk("x = 1", ""), 1.0, id="blank_lines_ignored"),
        pytest.param(Hunk("", ""), Hunk("x = 1", ""), 0.0, id="empty_new_side"),
        pytest.param(Hunk("", "a = 1"), Hunk("b = 2", ""), 0.0, id="disjoint"),
    ],
)
def test_hunk_overlap(a: Hunk, b: Hunk, expected: float) -> None:
    assert hunk_overlap(a, b) == expected


def transcript_line(uuid: str, secs: int, **overrides: Any) -> str:
    return json.dumps(
        {
            "uuid": uuid,
            "parentUuid": None,
            "sessionId": str(SESSION),
            "timestamp": (BASE + timedelta(seconds=secs)).isoformat(),
            "cwd": "/repo",
            "gitBranch": "main",
            "version": "1.2.3",
            "isSidechain": False,
        }
        | overrides
    )


def write_transcript(root: Path) -> Path:
    path = root / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    lines = [
        transcript_line("u0", 0, type="user", message={"role": "user", "content": "fix the bug"}),
        transcript_line(
            "a0",
            1,
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-7",
                "content": [
                    {"type": "text", "text": "editing"},
                    {
                        "type": "tool_use",
                        "id": "t1",
                        "name": "Edit",
                        "input": {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"},
                    },
                ],
            },
        ),
        transcript_line(
            "u1",
            2,
            type="user",
            message={
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "applied", "is_error": False}],
            },
        ),
    ]
    path.write_text("\n".join(lines) + "\n")
    return path


def test_from_session_discovers_parses_and_lifts(tmp_path: Path) -> None:
    write_transcript(tmp_path)
    act = anyio.run(partial(SessionActivity.from_session, SESSION, root=tmp_path))
    assert act.session_id == SESSION
    assert [turn.prompt for turn in act.turns] == ["fix the bug"]
    (use,) = act.turns[0].tool_uses
    assert isinstance(use.call, EditCall)
    assert use.result is not None and use.result.content == "applied"
    assert use.result_ts == BASE + timedelta(seconds=2)
    assert use.duration_ms == 1000
    assert act.edits[0].hunks == (Hunk("x = 1", "x = 2"),)


def test_from_session_raises_expired_when_transcript_gone(tmp_path: Path) -> None:
    with pytest.raises(TranscriptExpiredError) as excinfo:
        anyio.run(partial(SessionActivity.from_session, SESSION, root=tmp_path))
    assert excinfo.value.session_id == SESSION
