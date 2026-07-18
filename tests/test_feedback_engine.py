"""Native ``RustFeedbackStore`` wrapper behavior pinned against CPython ``sqlite3``.

Pins the open-time v8/v9 verdict-schema guard on readonly opens and the parameter-binding
exception fidelity (OverflowError for out-of-range ints, ProgrammingError for unsupported
types) that the stdlib ``sqlite3`` module raises for the same inputs.
"""

from __future__ import annotations

import sqlite3

import pytest

from cc_transcript import _native
from cc_transcript.judge.verdicts import VerdictSchemaError

V8_VERDICTS_DDL = (
    "CREATE TABLE verdicts (dedup_key TEXT, role TEXT, prompt_version INTEGER, "
    "model TEXT, category TEXT, accepted INTEGER, summary TEXT, confidence REAL, "
    "rationale TEXT, fidelity TEXT, judged_at TEXT, "
    "UNIQUE(dedup_key, role, prompt_version, model));"
)


def open_store(path: object, **overrides: object) -> _native.RustFeedbackStore:
    kwargs = {
        "extra_ddl": [],
        "event_columns": [],
        "migrations": [],
        "verdict_table": "verdicts",
        "accepted_column": "accepted",
        "summary_column": "summary",
        "event_filter": None,
    } | overrides
    return _native.RustFeedbackStore(str(path), **kwargs)


def test_readonly_open_rejects_v8_verdict_schema(tmp_path) -> None:
    path = tmp_path / "feedback.db"
    conn = sqlite3.connect(path)
    conn.executescript(V8_VERDICTS_DDL)
    conn.close()
    with pytest.raises(VerdictSchemaError, match="predates the v9 schema"):
        open_store(path, readonly=True)


def test_oversized_int_raises_overflow_error_like_sqlite3(tmp_path) -> None:
    store = open_store(tmp_path / "feedback.db")
    for value in (2**63, -(2**63) - 1):
        with pytest.raises(
            OverflowError, match="Python int too large to convert to SQLite INTEGER"
        ):
            store.sql("SELECT ? AS v", [value])
    assert store.sql("SELECT ? AS v", [2**63 - 1])[0]["v"] == 2**63 - 1


def test_unsupported_param_type_raises_programming_error_like_sqlite3(tmp_path) -> None:
    store = open_store(tmp_path / "feedback.db")
    with pytest.raises(
        sqlite3.ProgrammingError,
        match="Error binding parameter 1: type 'object' is not supported",
    ):
        store.sql("SELECT ?", [object()])
