from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from cc_transcript.heartbeats import Heartbeat, HeartbeatLog

if TYPE_CHECKING:
    from pathlib import Path

pytestmark = pytest.mark.anyio


async def test_beat_creates_one_row_with_count_one(tmp_path: Path) -> None:
    log = await HeartbeatLog.open(tmp_path / "decisions.db")
    await log.beat("sess-a", "PreToolUse", 1000)
    assert await log.for_session("sess-a") == (
        Heartbeat(session_id="sess-a", event="PreToolUse", first_ts_ms=1000, last_ts_ms=1000, count=1),
    )


async def test_beat_upserts_count_and_last_ts_keeping_first(tmp_path: Path) -> None:
    log = await HeartbeatLog.open(tmp_path / "decisions.db")
    await log.beat("sess-a", "PreToolUse", 1000)
    await log.beat("sess-a", "PreToolUse", 1500)
    await log.beat("sess-a", "PreToolUse", 2000)
    (beat,) = await log.for_session("sess-a")
    assert beat == Heartbeat(
        session_id="sess-a", event="PreToolUse", first_ts_ms=1000, last_ts_ms=2000, count=3
    )


async def test_distinct_events_are_distinct_rows(tmp_path: Path) -> None:
    log = await HeartbeatLog.open(tmp_path / "decisions.db")
    await log.beat("sess-a", "UserPromptSubmit", 100)
    await log.beat("sess-a", "PreToolUse", 200)
    await log.beat("sess-a", "Stop", 300)
    assert tuple(beat.event for beat in await log.for_session("sess-a")) == ("UserPromptSubmit", "PreToolUse", "Stop")


async def test_sessions_are_isolated(tmp_path: Path) -> None:
    log = await HeartbeatLog.open(tmp_path / "decisions.db")
    await log.beat("sess-a", "PreToolUse", 100)
    await log.beat("sess-b", "Stop", 200)
    assert tuple(beat.event for beat in await log.for_session("sess-a")) == ("PreToolUse",)
    assert tuple(beat.event for beat in await log.for_session("sess-b")) == ("Stop",)
    assert await log.for_session("unknown") == ()


async def test_for_session_orders_by_first_dispatch(tmp_path: Path) -> None:
    log = await HeartbeatLog.open(tmp_path / "decisions.db")
    await log.beat("sess-a", "Stop", 300)
    await log.beat("sess-a", "PreToolUse", 100)
    await log.beat("sess-a", "Stop", 400)  # later, but Stop's first was 300
    await log.beat("sess-a", "UserPromptSubmit", 200)
    assert [b.event for b in await log.for_session("sess-a")] == ["PreToolUse", "UserPromptSubmit", "Stop"]


async def test_reopen_persists_and_keeps_accumulating(tmp_path: Path) -> None:
    db = tmp_path / "decisions.db"
    await (await HeartbeatLog.open(db)).beat("sess-a", "PreToolUse", 100)
    await (await HeartbeatLog.open(db)).beat("sess-a", "PreToolUse", 250)
    (beat,) = await (await HeartbeatLog.open(db)).for_session("sess-a")
    assert beat.count == 2
    assert beat.first_ts_ms == 100
    assert beat.last_ts_ms == 250


async def test_coexists_with_decisions_table_in_same_file(tmp_path: Path) -> None:
    # The heartbeat table shares decisions.db with the decision ledger; opening the ledger
    # must not disturb heartbeat rows and vice versa.
    from cc_transcript.decisions import Decision, DecisionLog

    db = tmp_path / "decisions.db"
    hb = await HeartbeatLog.open(db)
    await hb.beat("sess-a", "PreToolUse", 100)
    dec = await DecisionLog.open(db)
    await dec.append(
        Decision(
            ts_ms=100,
            session_id="sess-a",
            source="captain-hook",
            kind="k",
            source_file="f.py",
            event="PreToolUse",
            action="allow",
        )
    )
    assert tuple(beat.event for beat in await (await HeartbeatLog.open(db)).for_session("sess-a")) == ("PreToolUse",)
    assert len(await (await DecisionLog.open(db)).for_session("sess-a")) == 1