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