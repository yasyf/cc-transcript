from __future__ import annotations

import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.ids import EventRef, EventUuid, ToolUseId
from tests import testkit
from tests.support import SESSION, assistant, user


@pytest.fixture
def activities() -> tuple[SessionActivity, SessionActivity, EventRef]:
    full = SessionActivity.from_events(
        SESSION,
        tuple(
            event
            for index in range(9)
            for event in (
                user(f"u{index}", f"turn {index}"),
                assistant(
                    f"a{index}",
                    "",
                    blocks=tuple(
                        testkit.tool_use(
                            tool_id,
                            "Edit",
                            {"file_path": path, "old_string": "old", "new_string": "new"},
                        )
                        for tool_id, path in (
                            (("before6", "/a.py"), ("anchor6", "/a.py"), ("after6", "/a.py"), ("other6", "/b.py"))
                            if index == 6
                            else ((f"t{index}", "/a.py"),)
                        )
                    ),
                ),
            )
        ),
    )
    return (
        full,
        SessionActivity(SESSION, full.turns[4:]),
        EventRef(SESSION, EventUuid("a6"), ToolUseId("anchor6")),
    )


@pytest.mark.parametrize(
    ("lookback", "expected"),
    [(0, ["before6"]), (1, ["before6", "t5"]), (2, ["before6", "t5", "t4"]), (20, ["before6", "t5", "t4"])],
)
def test_edits_before_uses_original_turn_indexes(
    activities: tuple[SessionActivity, SessionActivity, EventRef], lookback: int, expected: list[str]
) -> None:
    full, window, anchor = activities
    edits = window.edits_before(anchor, lookback_turns=lookback)
    assert [turn.index for turn in window.turns] == [4, 5, 6, 7, 8]
    assert [edit.ref.tool_use_id for edit in edits] == expected
    assert edits == tuple(edit for edit in full.edits_before(anchor, lookback_turns=lookback) if edit.turn_index >= 4)


@pytest.mark.parametrize(
    ("lookahead", "expected"),
    [(0, ["after6"]), (1, ["after6", "t7"]), (2, ["after6", "t7", "t8"]), (20, ["after6", "t7", "t8"])],
)
def test_edits_after_uses_original_turn_indexes(
    activities: tuple[SessionActivity, SessionActivity, EventRef], lookahead: int, expected: list[str]
) -> None:
    full, window, anchor = activities
    edits = window.edits_after(anchor, file_path="/a.py", lookahead_turns=lookahead)
    assert [edit.ref.tool_use_id for edit in edits] == expected
    assert edits == full.edits_after(anchor, file_path="/a.py", lookahead_turns=lookahead)
    assert [edit.ref.tool_use_id for edit in window.edits_after(anchor, file_path="/b.py", lookahead_turns=lookahead)] == ["other6"]


def test_scoped_activity_cannot_resolve_an_anchor_outside_its_window(
    activities: tuple[SessionActivity, SessionActivity, EventRef],
) -> None:
    _, window, _ = activities
    anchor = EventRef(SESSION, EventUuid("a3"), ToolUseId("t3"))
    assert window.edits_before(anchor, lookback_turns=20) == ()
    assert window.edits_after(anchor, file_path="/a.py", lookahead_turns=20) == ()
