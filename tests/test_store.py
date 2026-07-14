from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from cc_transcript.store import FileStateStore

EXTRA_SCHEMA = """
CREATE TABLE IF NOT EXISTS notes (
  path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
  body TEXT NOT NULL
);
"""


def test_record_and_read_mtimes(tmp_path: Path) -> None:
    with FileStateStore.open(tmp_path / "state.db") as store:
        store.record_file("/a.jsonl", 1.0)
        store.record_file("/b.jsonl", 2.0)
        store.record_file("/a.jsonl", 3.0)
        assert store.file_mtimes() == {"/a.jsonl": 3.0, "/b.jsonl": 2.0}


def test_extra_schema_table_created(tmp_path: Path) -> None:
    with FileStateStore.open(tmp_path / "state.db", extra_schema=EXTRA_SCHEMA) as store:
        store.record_file("/a.jsonl", 1.0)
        with store.transaction() as conn:
            conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/a.jsonl", "hi"))
        rows = [tuple(r) for r in store.conn.execute("SELECT path, body FROM notes").fetchall()]
    assert rows == [("/a.jsonl", "hi")]


def test_transaction_is_atomic_across_record_and_extra(tmp_path: Path) -> None:
    with FileStateStore.open(tmp_path / "state.db", extra_schema=EXTRA_SCHEMA) as store:
        with pytest.raises(sqlite3.IntegrityError):
            with store.transaction() as conn:
                store.record_file("/c.jsonl", 9.0)
                conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/missing.jsonl", "x"))
        assert store.file_mtimes() == {}


def test_transaction_commits_record_and_extra_together(tmp_path: Path) -> None:
    with FileStateStore.open(tmp_path / "state.db", extra_schema=EXTRA_SCHEMA) as store:
        with store.transaction() as conn:
            store.record_file("/d.jsonl", 4.0)
            conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/d.jsonl", "note"))
        notes = store.conn.execute("SELECT COUNT(*) FROM notes").fetchone()[0]
        assert store.file_mtimes() == {"/d.jsonl": 4.0}
        assert notes == 1


def test_context_manager_closes_connection(tmp_path: Path) -> None:
    store = FileStateStore.open(tmp_path / "state.db")
    with store:
        store.record_file("/a.jsonl", 1.0)
    with pytest.raises(sqlite3.ProgrammingError):
        store.conn.execute("SELECT 1")
