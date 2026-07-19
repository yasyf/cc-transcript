from __future__ import annotations

import pathlib
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import pytest

from cc_transcript.corrections import CORRECTIONS_DDL, CorrectionLog
from cc_transcript.decisions import DECISIONS_DDL, DecisionLog
from cc_transcript.ledger import AsyncLedger, LedgerRecord
from tests.support import (
    BASE_CORRECTION,
    OTHER_SESSION,
    SESSION,
    correction,
    correction_distinct,
    decision,
    decision_distinct,
)

if TYPE_CHECKING:
    from collections.abc import Callable

pytestmark = pytest.mark.anyio


@dataclass(frozen=True, slots=True)
class LedgerCase[R: LedgerRecord]:
    log_cls: type[AsyncLedger[R]] | type[CorrectionLog]
    filename: str
    ddl: str
    sql_fixture: str
    base: R
    reappend: R
    distinct: Callable[..., R]


CORRECTION_CASE = LedgerCase(
    log_cls=CorrectionLog,
    filename="corrections.db",
    ddl=CORRECTIONS_DDL,
    sql_fixture="corrections.sql",
    base=BASE_CORRECTION,
    reappend=correction(ts_ms=9_999, overlap=0.9, correction_new="reworked", detail={"rerun": True}),
    distinct=correction_distinct,
)

DECISION_CASE = LedgerCase(
    log_cls=DecisionLog,
    filename="decisions.db",
    ddl=DECISIONS_DDL,
    sql_fixture="decisions.sql",
    base=decision(1_000),
    reappend=decision(1_000, message="reworded", detail={"rerun": True}),
    distinct=decision_distinct,
)

CASES = [pytest.param(CORRECTION_CASE, id="corrections"), pytest.param(DECISION_CASE, id="decisions")]


async def open_log(case: LedgerCase[Any], tmp_path: pathlib.Path) -> AsyncLedger[Any] | CorrectionLog:
    return await case.log_cls.open(tmp_path / case.filename)


async def journal_mode(log: AsyncLedger[Any] | CorrectionLog) -> str:
    match log:
        case CorrectionLog():
            return (await log.sql("PRAGMA journal_mode"))[0]["journal_mode"]
        case _:
            conn = log._actor.conn
            return (await log._actor.run(lambda: conn.execute("PRAGMA journal_mode").fetchone()))[0]


@pytest.mark.parametrize("case", CASES)
async def test_append_round_trips_every_field(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = await open_log(case, tmp_path)
    await log.append(case.base)
    assert await log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
async def test_reappend_of_same_unique_tuple_keeps_one_row(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = await open_log(case, tmp_path)
    await log.append(case.base)
    await log.append(case.reappend)
    assert await log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
async def test_for_session_orders_by_ts_and_filters_by_session(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = await open_log(case, tmp_path)
    await log.append(case.distinct(3_000, seq=2))
    await log.append(case.distinct(1_000, seq=0))
    await log.append(case.distinct(2_000, seq=1))
    await log.append(case.distinct(1_500, session_id=OTHER_SESSION, seq=0))
    assert [r.ts_ms for r in await log.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [r.ts_ms for r in await log.for_session(OTHER_SESSION)] == [1_500]


@pytest.mark.parametrize("case", CASES)
async def test_two_logs_on_one_path_interleave_appends(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    first, second = await open_log(case, tmp_path), await open_log(case, tmp_path)
    assert await journal_mode(first) == "wal"
    await first.append(case.distinct(1_000, seq=0))
    await second.append(case.distinct(2_000, seq=1))
    await first.append(case.distinct(3_000, seq=2))
    await second.append(case.distinct(3_000, seq=2))
    assert [r.ts_ms for r in await first.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [r.ts_ms for r in await second.for_session(SESSION)] == [1_000, 2_000, 3_000]


@pytest.mark.parametrize("case", CASES)
async def test_open_defaults_to_home_ledger(
    case: LedgerCase[Any], tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    log = await case.log_cls.open()
    await log.append(case.base)
    assert (tmp_path / ".cc-transcript" / case.filename).exists()
    assert await log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
def test_ddl_matches_frozen_contract(case: LedgerCase[Any]) -> None:
    frozen = (pathlib.Path(__file__).parent / "testdata" / case.sql_fixture).read_text()
    assert case.ddl == frozen
