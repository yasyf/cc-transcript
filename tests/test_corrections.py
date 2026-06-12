from __future__ import annotations

import pathlib
from dataclasses import replace
from typing import TYPE_CHECKING, Any

from cc_transcript.corrections import CORRECTIONS_DDL, Correction, CorrectionLog
from cc_transcript.ids import EventUuid, SessionId, ToolDigest

if TYPE_CHECKING:
    import pytest

SESSION = SessionId("11111111-1111-1111-1111-111111111111")
OTHER_SESSION = SessionId("22222222-2222-2222-2222-222222222222")
ANCHOR = EventUuid("anchor-1")
OTHER_ANCHOR = EventUuid("anchor-2")
DIGEST_A = ToolDigest("a" * 64)
DIGEST_B = ToolDigest("b" * 64)
DIGEST_C = ToolDigest("c" * 64)

BASE = Correction(
    ts_ms=1_000,
    session_id=SESSION,
    source="cc-pushback",
    anchor_uuid=ANCHOR,
    incorrect_digest=DIGEST_A,
    incorrect_file="/a.py",
    incorrect_old="alpha = 1",
    incorrect_new="alpha = 2",
    extractor_version=1,
    correction_origin="session",
    correction_file="/a.py",
    correction_old="alpha = 2",
    correction_new="alpha = 3",
    correction_commit=None,
    overlap=0.5,
    detail={"rule": "overlap", "turn": 3},
)

NO_FIX_FIELDS: dict[str, Any] = {
    "correction_origin": None,
    "correction_file": None,
    "correction_old": None,
    "correction_new": None,
    "correction_commit": None,
    "overlap": 0.0,
    "detail": {},
}


def correction(**overrides: Any) -> Correction:
    return replace(BASE, **overrides)


def open_log(tmp_path: pathlib.Path) -> CorrectionLog:
    return CorrectionLog.open(tmp_path / "corrections.db")


def test_append_round_trips_every_field(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    log.append(BASE)
    assert log.for_session(SESSION) == (BASE,)


def test_reappend_of_same_unique_tuple_keeps_one_row(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    log.append(BASE)
    log.append(correction(ts_ms=9_999, overlap=0.9, correction_new="reworked", detail={"rerun": True}))
    assert log.for_session(SESSION) == (BASE,)


def test_for_session_orders_by_ts_and_filters_by_session(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    log.append(correction(ts_ms=3_000, incorrect_digest=DIGEST_C))
    log.append(correction(ts_ms=1_000, incorrect_digest=DIGEST_A))
    log.append(correction(ts_ms=2_000, incorrect_digest=DIGEST_B))
    log.append(correction(ts_ms=1_500, session_id=OTHER_SESSION))
    assert [c.ts_ms for c in log.for_session(SESSION)] == [1_000, 2_000, 3_000]
    assert [c.ts_ms for c in log.for_session(OTHER_SESSION)] == [1_500]


def test_for_anchor_filters_by_anchor(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR))
    log.append(correction(incorrect_digest=DIGEST_B, anchor_uuid=ANCHOR))
    log.append(correction(incorrect_digest=DIGEST_C, anchor_uuid=OTHER_ANCHOR))
    assert {c.incorrect_digest for c in log.for_anchor(SESSION, ANCHOR)} == {DIGEST_A, DIGEST_B}
    assert [c.incorrect_digest for c in log.for_anchor(SESSION, OTHER_ANCHOR)] == [DIGEST_C]


def test_by_digest_is_the_cross_consumer_join(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR))
    log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=OTHER_ANCHOR))
    log.append(correction(incorrect_digest=DIGEST_B, anchor_uuid=ANCHOR))
    assert {c.anchor_uuid for c in log.by_digest(SESSION, incorrect_digest=DIGEST_A)} == {ANCHOR, OTHER_ANCHOR}
    assert log.by_digest(SESSION, incorrect_digest=DIGEST_C) == ()
    assert log.by_digest(OTHER_SESSION, incorrect_digest=DIGEST_A) == ()


def test_no_correction_row_round_trips_nulls(tmp_path: pathlib.Path) -> None:
    log = open_log(tmp_path)
    row = correction(**NO_FIX_FIELDS)
    log.append(row)
    assert log.for_session(SESSION) == (row,)


def test_two_logs_on_one_path_interleave_appends(tmp_path: pathlib.Path) -> None:
    first, second = open_log(tmp_path), open_log(tmp_path)
    assert first.conn.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    first.append(correction(ts_ms=1_000, incorrect_digest=DIGEST_A))
    second.append(correction(ts_ms=2_000, incorrect_digest=DIGEST_B))
    assert {c.incorrect_digest for c in first.for_session(SESSION)} == {DIGEST_A, DIGEST_B}
    assert {c.incorrect_digest for c in second.for_session(SESSION)} == {DIGEST_A, DIGEST_B}


def test_open_defaults_to_home_ledger(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    log = CorrectionLog.open()
    log.append(BASE)
    assert (tmp_path / ".cc-transcript" / "corrections.db").exists()
    assert log.for_session(SESSION) == (BASE,)


def test_ddl_matches_frozen_cross_language_contract() -> None:
    frozen = (pathlib.Path(__file__).parent / "testdata" / "corrections_v1.sql").read_text()
    assert CORRECTIONS_DDL == frozen
