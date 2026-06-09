from __future__ import annotations

import sqlite3
from pathlib import Path

import anyio
import pytest

from cc_transcript.store import FileStateStore

EXTRA_SCHEMA = """
CREATE TABLE IF NOT EXISTS notes (
  path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
  body TEXT NOT NULL
);
"""


def test_record_and_read_mtimes(tmp_path: Path) -> None:
    async def go() -> dict[str, float]:
        async with await FileStateStore.open(tmp_path / "state.db") as store:
            await store.record_file("/a.jsonl", 1.0)
            await store.record_file("/b.jsonl", 2.0)
            await store.record_file("/a.jsonl", 3.0)
            return await store.file_mtimes()

    assert anyio.run(go) == {"/a.jsonl": 3.0, "/b.jsonl": 2.0}


def test_extra_schema_table_created(tmp_path: Path) -> None:
    async def go() -> list[tuple[object, ...]]:
        async with await FileStateStore.open(tmp_path / "state.db", extra_schema=EXTRA_SCHEMA) as store:
            await store.record_file("/a.jsonl", 1.0)
            async with store.transaction() as conn:
                await conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/a.jsonl", "hi"))
            cur = await store.conn.execute("SELECT path, body FROM notes")
            return [tuple(r) for r in await cur.fetchall()]

    assert anyio.run(go) == [("/a.jsonl", "hi")]


def test_transaction_is_atomic_across_record_and_extra(tmp_path: Path) -> None:
    db = tmp_path / "state.db"

    async def go() -> dict[str, float]:
        async with await FileStateStore.open(db, extra_schema=EXTRA_SCHEMA) as store:
            with pytest.raises(sqlite3.IntegrityError):
                async with store.transaction() as conn:
                    await store.record_file("/c.jsonl", 9.0)
                    await conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/missing.jsonl", "x"))
            return await store.file_mtimes()

    assert anyio.run(go) == {}


def test_transaction_commits_record_and_extra_together(tmp_path: Path) -> None:
    async def go() -> tuple[dict[str, float], int]:
        async with await FileStateStore.open(tmp_path / "state.db", extra_schema=EXTRA_SCHEMA) as store:
            async with store.transaction() as conn:
                await store.record_file("/d.jsonl", 4.0)
                await conn.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/d.jsonl", "note"))
            cur = await store.conn.execute("SELECT COUNT(*) FROM notes")
            return await store.file_mtimes(), (await cur.fetchone())[0]

    mtimes, notes = anyio.run(go)
    assert mtimes == {"/d.jsonl": 4.0}
    assert notes == 1


def test_context_manager_closes_connection(tmp_path: Path) -> None:
    async def go() -> None:
        store = await FileStateStore.open(tmp_path / "state.db")
        async with store:
            await store.record_file("/a.jsonl", 1.0)
        with pytest.raises(ValueError):
            await store.conn.execute("SELECT 1")

    anyio.run(go)
