from __future__ import annotations

import pathlib
from dataclasses import dataclass, replace
from typing import TYPE_CHECKING, Any

import pytest

from cc_transcript.corrections import CORRECTIONS_DDL, Correction, CorrectionLog
from cc_transcript.decisions import DECISIONS_DDL, Decision, DecisionLog
from cc_transcript.ids import EventUuid, SessionId, ToolDigest
from cc_transcript.ledger import LedgerRecord, SyncLedger

if TYPE_CHECKING:
    from collections.abc import Callable

SESSION = SessionId("11111111-1111-1111-1111-111111111111")
OTHER_SESSION = SessionId("22222222-2222-2222-2222-222222222222")
ANCHOR = EventUuid("anchor-1")
DIGEST_A = ToolDigest("a" * 64)
DIGEST_B = ToolDigest("b" * 64)
DIGEST_C = ToolDigest("c" * 64)
DIGESTS = (DIGEST_A, DIGEST_B, DIGEST_C)

BASE_CORRECTION = Correction(
    ts_ms=1_000,
    session_id=SESSION,
    source="cc-pushback",
    anchor_uuid=ANCHOR,
    incorrect_digest=DIGEST_A,
    incorrect_file="/a.py",
    incorrect_old="alpha = 1",
    incorrect_new="alpha = 2",
    correction_origin="session",
    correction_file="/a.py",
    correction_old="alpha = 2",
    correction_new="alpha = 3",
    correction_commit=None,
    overlap=0.5,
    detail={"rule": "overlap", "turn": 3},
)

BASE_DECISION = Decision(
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


def correction(**overrides: Any) -> Correction:
    return replace(BASE_CORRECTION, **overrides)


def decision(ts_ms: int, **overrides: Any) -> Decision:
    return replace(BASE_DECISION, ts_ms=ts_ms, **overrides)


def correction_distinct(ts_ms: int, *, session_id: SessionId = SESSION, seq: int = 0) -> Correction:
    return correction(ts_ms=ts_ms, session_id=session_id, incorrect_digest=DIGESTS[seq])


def decision_distinct(ts_ms: int, *, session_id: SessionId = SESSION, seq: int = 0) -> Decision:
    return decision(ts_ms, session_id=session_id)


@dataclass(frozen=True, slots=True)
class LedgerCase[R: LedgerRecord]:
    log_cls: type[SyncLedger[R]]
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


def open_log(case: LedgerCase[Any], tmp_path: pathlib.Path) -> SyncLedger[Any]:
    return case.log_cls.open(tmp_path / case.filename)


@pytest.mark.parametrize("case", CASES)
def test_append_round_trips_every_field(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = open_log(case, tmp_path)
    log.append(case.base)
    assert log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
def test_reappend_of_same_unique_tuple_keeps_one_row(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = open_log(case, tmp_path)
    log.append(case.base)
    log.append(case.reappend)
    assert log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
def test_for_session_orders_by_ts_and_filters_by_session(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    log = open_log(case, tmp_path)
    log.append(case.distinct(3_000, seq=2))
    log.append(case.distinct(1_000, seq=0))
    log.append(case.distinct(2_000, seq=1))
    log.append(case.distinct(1_500, session_id=OTHER_SESSION, seq=0))
    assert [r.ts_ms for r in log.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [r.ts_ms for r in log.for_session(OTHER_SESSION)] == [1_500]


@pytest.mark.parametrize("case", CASES)
def test_two_logs_on_one_path_interleave_appends(case: LedgerCase[Any], tmp_path: pathlib.Path) -> None:
    first, second = open_log(case, tmp_path), open_log(case, tmp_path)
    assert first.conn.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    first.append(case.distinct(1_000, seq=0))
    second.append(case.distinct(2_000, seq=1))
    first.append(case.distinct(3_000, seq=2))
    second.append(case.distinct(3_000, seq=2))
    assert [r.ts_ms for r in first.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [r.ts_ms for r in second.for_session(SESSION)] == [1_000, 2_000, 3_000]


@pytest.mark.parametrize("case", CASES)
def test_open_defaults_to_home_ledger(
    case: LedgerCase[Any], tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    log = case.log_cls.open()
    log.append(case.base)
    assert (tmp_path / ".cc-transcript" / case.filename).exists()
    assert log.for_session(SESSION) == (case.base,)


@pytest.mark.parametrize("case", CASES)
def test_ddl_matches_frozen_contract(case: LedgerCase[Any]) -> None:
    frozen = (pathlib.Path(__file__).parent / "testdata" / case.sql_fixture).read_text()
    assert case.ddl == frozen
