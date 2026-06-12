from __future__ import annotations

from dataclasses import replace
from typing import TYPE_CHECKING, Any

import pytest

from cc_transcript.decisions import Decision, DecisionLog
from cc_transcript.ids import EventUuid, SessionId, ToolDigest

if TYPE_CHECKING:
    from pathlib import Path

SESSION = SessionId("11111111-1111-1111-1111-111111111111")
OTHER_SESSION = SessionId("22222222-2222-2222-2222-222222222222")
DIGEST_A = ToolDigest("a" * 64)
DIGEST_B = ToolDigest("b" * 64)

BASE = Decision(
    ts_ms=0,
    session_id=SESSION,
    source="captain-hook",
    kind="no-defensive-code",
    source_file="primitives/nudge.py",
    event="PreToolUse",
    action="nudge",
    tool_name="Edit",
    tool_digest=DIGEST_A,
    event_uuid=EventUuid("e-1"),
    message="prefer failing fast",
    detail={"rule": "defensive", "turn": 3},
)

STOP_FIELDS: dict[str, Any] = {
    "event": "Stop",
    "kind": "task-tracking",
    "tool_name": None,
    "tool_digest": None,
    "event_uuid": None,
    "message": None,
    "detail": {},
}


def decision(ts_ms: int, **overrides: Any) -> Decision:
    return replace(BASE, ts_ms=ts_ms, **overrides)


def stop_decision(ts_ms: int, **overrides: Any) -> Decision:
    return decision(ts_ms, **(STOP_FIELDS | overrides))


def open_log(tmp_path: Path) -> DecisionLog:
    return DecisionLog.open(tmp_path / "decisions.db")


def test_append_round_trips_every_field(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(1_000))
    assert log.for_session(SESSION) == (decision(1_000),)


def test_reappend_of_same_unique_tuple_keeps_one_row(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(1_000))
    log.append(decision(1_000, message="reworded", detail={"rerun": True}))
    assert log.for_session(SESSION) == (decision(1_000),)


def test_for_session_orders_by_ts_and_filters_by_session(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(3_000))
    log.append(decision(1_000))
    log.append(decision(2_000))
    log.append(decision(1_500, session_id=OTHER_SESSION))
    assert [d.ts_ms for d in log.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [d.ts_ms for d in log.for_session(OTHER_SESSION)] == [1_500]


def test_attribute_tool_picks_nearest_preceding_digest_match(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(1_000))
    log.append(decision(4_500))
    log.append(decision(4_800, tool_digest=DIGEST_B))
    log.append(decision(6_000))
    found = log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=5_000)
    assert found is not None and found.ts_ms == 4_500


def test_attribute_tool_ignores_rows_outside_window(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(1_000))
    assert log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=302_000) is None
    assert log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=2_000, window_ms=500) is None
    found = log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=2_000, window_ms=1_000)
    assert found is not None and found.ts_ms == 1_000


def test_attribute_tool_requires_digest_equality(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(decision(1_000))
    assert log.attribute_tool(SESSION, tool_digest=DIGEST_B, near_ts_ms=1_500) is None
    assert log.attribute_tool(OTHER_SESSION, tool_digest=DIGEST_A, near_ts_ms=1_500) is None


def test_stop_shaped_decision_round_trips_nones(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(stop_decision(9_000))
    assert log.for_session(SESSION) == (stop_decision(9_000),)


def test_attribute_nearest_filters_by_event_and_picks_nearest(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(stop_decision(1_000, kind="task-tracking"))
    log.append(stop_decision(2_000, kind="stop-summary"))
    log.append(decision(1_790))
    found = log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800)
    assert found is not None and found.ts_ms == 2_000
    assert log.attribute_nearest(SESSION, event="Notification", near_ts_ms=1_800) is None


def test_attribute_nearest_honors_kind_and_window(tmp_path: Path) -> None:
    log = open_log(tmp_path)
    log.append(stop_decision(1_000, kind="task-tracking"))
    log.append(stop_decision(2_000, kind="stop-summary"))
    found = log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800, kind="task-tracking")
    assert found is not None and found.ts_ms == 1_000
    assert log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800, kind="missing") is None
    assert log.attribute_nearest(SESSION, event="Stop", near_ts_ms=400_000, window_ms=100_000) is None


def test_two_logs_on_one_path_interleave_appends(tmp_path: Path) -> None:
    first, second = open_log(tmp_path), open_log(tmp_path)
    assert first.conn.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    first.append(decision(1_000))
    second.append(decision(2_000))
    first.append(decision(3_000))
    second.append(decision(3_000))
    assert [d.ts_ms for d in first.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [d.ts_ms for d in second.for_session(SESSION)] == [1_000, 2_000, 3_000]


def test_open_defaults_to_home_ledger(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    log = DecisionLog.open()
    log.append(decision(1_000))
    assert (tmp_path / ".cc-transcript" / "decisions.db").exists()
    assert log.for_session(SESSION) == (decision(1_000),)
