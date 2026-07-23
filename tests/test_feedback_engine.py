"""Native ``RustFeedbackStore`` wrapper behavior pinned against CPython ``sqlite3``."""

from __future__ import annotations

import sqlite3

import pytest

from cc_transcript import _native
from cc_transcript.mining.store import DEFAULT_SCHEMA_DDL

pytestmark = pytest.mark.anyio

async def open_store(path: object, **overrides: object) -> _native.RustFeedbackStore:
    kwargs = {
        "schema_identity": "cc-transcript-feedback-test",
        "schema_ddl": DEFAULT_SCHEMA_DDL,
        "event_columns": [],
        "extension_paths": [],
        "verdict_table": "verdicts",
        "accepted_column": "accepted",
        "summary_column": "summary",
        "event_filter": None,
    } | overrides
    store = _native.RustFeedbackStore(str(path), **kwargs)
    await store.open()
    return store


async def test_readonly_open_rejects_foreign_schema(tmp_path) -> None:
    path = tmp_path / "feedback.db"
    conn = sqlite3.connect(path)
    conn.execute("CREATE TABLE foreign_table(id INTEGER)")
    conn.close()
    with pytest.raises(sqlite3.DatabaseError, match="schema"):
        await open_store(path, readonly=True)


async def test_oversized_int_raises_overflow_error_like_sqlite3(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    for value in (2**63, -(2**63) - 1):
        with pytest.raises(
            OverflowError, match="Python int too large to convert to SQLite INTEGER"
        ):
            store.sql("SELECT ? AS v", [value])
    assert (await store.sql("SELECT ? AS v", [2**63 - 1]))[0]["v"] == 2**63 - 1


async def test_unsupported_param_type_raises_programming_error_like_sqlite3(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    with pytest.raises(
        sqlite3.ProgrammingError,
        match="Error binding parameter 1: type 'object' is not supported",
    ):
        store.sql("SELECT ?", [object()])
