from __future__ import annotations

import asyncio
import sqlite3
from typing import TYPE_CHECKING, Any

import pytest

from cc_transcript.decisions import Decision, DecisionLog
from tests.support import DIGEST_A, DIGEST_B, OTHER_SESSION, SESSION, decision

if TYPE_CHECKING:
    from pathlib import Path

pytestmark = pytest.mark.anyio

STOP_FIELDS: dict[str, Any] = {
    "event": "Stop",
    "kind": "task-tracking",
    "tool_name": None,
    "tool_digest": None,
    "event_uuid": None,
    "message": None,
    "detail": {},
}


def stop_decision(ts_ms: int, **overrides: Any) -> Decision:
    return decision(ts_ms, **(STOP_FIELDS | overrides))


async def open_log(tmp_path: Path) -> DecisionLog:
    return await DecisionLog.open(tmp_path / "decisions.db")


async def test_attribute_tool_picks_nearest_preceding_digest_match(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(decision(1_000))
    await log.append(decision(4_500))
    await log.append(decision(4_800, tool_digest=DIGEST_B))
    await log.append(decision(6_000))
    found = await log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=5_000)
    assert found is not None and found.ts_ms == 4_500


async def test_attribute_tool_ignores_rows_outside_window(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(decision(1_000))
    assert await log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=302_000) is None
    assert await log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=2_000, window_ms=500) is None
    found = await log.attribute_tool(SESSION, tool_digest=DIGEST_A, near_ts_ms=2_000, window_ms=1_000)
    assert found is not None and found.ts_ms == 1_000


async def test_attribute_tool_requires_digest_equality(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(decision(1_000))
    assert await log.attribute_tool(SESSION, tool_digest=DIGEST_B, near_ts_ms=1_500) is None
    assert await log.attribute_tool(OTHER_SESSION, tool_digest=DIGEST_A, near_ts_ms=1_500) is None


async def test_stop_shaped_decision_round_trips_nones(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(stop_decision(9_000))
    assert await log.for_session(SESSION) == (stop_decision(9_000),)


async def test_attribute_nearest_filters_by_event_and_picks_nearest(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(stop_decision(1_000, kind="task-tracking"))
    await log.append(stop_decision(2_000, kind="stop-summary"))
    await log.append(decision(1_790))
    found = await log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800)
    assert found is not None and found.ts_ms == 2_000
    assert await log.attribute_nearest(SESSION, event="Notification", near_ts_ms=1_800) is None


async def test_attribute_nearest_honors_kind_and_window(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(stop_decision(1_000, kind="task-tracking"))
    await log.append(stop_decision(2_000, kind="stop-summary"))
    found = await log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800, kind="task-tracking")
    assert found is not None and found.ts_ms == 1_000
    assert await log.attribute_nearest(SESSION, event="Stop", near_ts_ms=1_800, kind="missing") is None
    assert await log.attribute_nearest(SESSION, event="Stop", near_ts_ms=400_000, window_ms=100_000) is None


async def test_operation_after_close_raises_programming_error(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    await log.append(decision(1_000))
    await log.close()
    await log.close()  # idempotent: a second close is a no-op
    with pytest.raises(sqlite3.ProgrammingError):
        await log.append(decision(2_000))


async def test_actor_runs_concurrent_tasks_in_submission_order(tmp_path: Path) -> None:
    log = await open_log(tmp_path)
    order: list[int] = []
    await asyncio.gather(*(log._actor.run(lambda i=i: order.append(i)) for i in range(25)))
    assert order == list(range(25))
    await log.close()

@pytest.mark.parametrize(
    ("start_ts_ms", "end_ts_ms", "expected_kinds"),
    [
        (1_000, 3_000, ("lower", "z-first", "a-second", "upper")),
        (2_000, 2_000, ("z-first", "a-second")),
        (3_002, 4_000, ()),
        (3_000, 1_000, ()),
    ],
)
async def test_session_between_filters_before_hydration_and_preserves_order(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    start_ts_ms: int,
    end_ts_ms: int,
    expected_kinds: tuple[str, ...],
) -> None:
    async with await open_log(tmp_path) as log:
        for record in (
            decision(3_000, kind="upper"),
            decision(999, kind="before"),
            decision(2_000, kind="z-first"),
            decision(2_000, kind="a-second"),
            decision(1_000, kind="lower"),
            decision(3_001, kind="after"),
            decision(2_000, kind="other-session", session_id=OTHER_SESSION),
        ):
            await log.append(record)
        hydrated: list[str] = []
        row_to_record = log.row_to_record

        def record_hydration(row: sqlite3.Row) -> Decision:
            hydrated.append(row["kind"])
            return row_to_record(row)

        monkeypatch.setattr(log, "row_to_record", record_hydration)
        rows = await log.for_session_between(SESSION, start_ts_ms, end_ts_ms)
        assert tuple(row.kind for row in rows) == expected_kinds
        assert tuple(hydrated) == expected_kinds
