"""The file-state ledger and transaction discipline, now on the ``FeedbackStore`` facade.

``FileStateStore`` folded into the facade at the native-store flip; these pin the
scanned-file mtime table, the ``extra_ddl`` composition with a foreign-keyed consumer
table, and the ``_txn_owner`` transaction-conflict guard the facade carries in Python.
"""

from __future__ import annotations

import asyncio
import re
import sqlite3
import threading
from pathlib import Path

import pytest

from cc_transcript.mining import store as store_module
from cc_transcript.mining.candidates import DedupKey
from cc_transcript.mining.store import FeedbackStore, StoreSchema, TransactionConflictError
from tests import store_contract_fixtures as fx

EXTRA_SCHEMA = """
CREATE TABLE IF NOT EXISTS notes (
  path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
  body TEXT NOT NULL
);
"""


def open_store(path: Path, *, extra: str = "") -> FeedbackStore:
    return FeedbackStore.open(path, StoreSchema(extra_ddl=(extra,) if extra else ()))


def test_record_and_read_mtimes(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db") as store:
        store.record_file("/a.jsonl", 1.0)
        store.record_file("/b.jsonl", 2.0)
        store.record_file("/a.jsonl", 3.0)
        assert store.file_mtimes() == {"/a.jsonl": 3.0, "/b.jsonl": 2.0}


def test_extra_schema_table_created(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        store.record_file("/a.jsonl", 1.0)
        with store.transaction() as db:
            db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/a.jsonl", "hi"))
        rows = [(row["path"], row["body"]) for row in store.sql("SELECT path, body FROM notes")]
    assert rows == [("/a.jsonl", "hi")]


def test_transaction_is_atomic_across_record_and_extra(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        with pytest.raises(sqlite3.IntegrityError):
            with store.transaction() as db:
                store.record_file("/c.jsonl", 9.0)
                db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/missing.jsonl", "x"))
        assert store.file_mtimes() == {}


def test_transaction_commits_record_and_extra_together(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        with store.transaction() as db:
            store.record_file("/d.jsonl", 4.0)
            db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/d.jsonl", "note"))
        notes = store.sql("SELECT COUNT(*) AS n FROM notes")[0]["n"]
        assert store.file_mtimes() == {"/d.jsonl": 4.0}
        assert notes == 1


def test_close_releases_the_connection(tmp_path: Path) -> None:
    store = open_store(tmp_path / "state.db")
    with store:
        store.record_file("/a.jsonl", 1.0)
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        store.sql("SELECT 1")


def test_double_close_is_a_noop(tmp_path: Path) -> None:
    store = open_store(tmp_path / "state.db")
    store.close()
    store.close()


def test_exit_after_manual_close_does_not_raise(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db") as store:
        store.record_file("/a.jsonl", 1.0)
        store.close()


def test_aliased_engine_is_refused_after_close(tmp_path: Path) -> None:
    store = open_store(tmp_path / "state.db")
    engine = store.engine
    store.close()
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        engine.sql("SELECT 1")


def test_cross_thread_use_raises_catchable_programming_error(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db") as store:
        store.record_file("/a.jsonl", 1.0)
        caught: list[Exception] = []

        def use() -> None:
            try:
                store.sql("SELECT 1")
            except Exception as error:  # noqa: BLE001 — the contract under test: an Exception subclass
                caught.append(error)

        thread = threading.Thread(target=use)
        thread.start()
        thread.join()
        [error] = caught
        assert isinstance(error, sqlite3.ProgrammingError)
        assert re.fullmatch(
            r"SQLite objects created in a thread can only be used in that same thread\. "
            r"The object was created in thread id \d+ and this is thread id \d+\.",
            str(error),
        )


class SummaryVerdict:
    category = "wrong_approach"
    accepted = True
    summary = "seed summary"
    confidence = 0.9
    rationale = "stub"
    canonical_key = None


def test_unlimited_filtered_refresh_matches_the_eager_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(store_module, "REFRESH_PAGE_SIZE", 3)
    with FeedbackStore.open(tmp_path / "f.db", StoreSchema(event_filter="e.id > 0")) as store:
        store.record_file_scan("/scan.jsonl", 1.0, [fx.candidate(f"k{i}") for i in range(8)])
        for i in range(6):
            store.record_verdict(
                DedupKey(f"k{i}"), SummaryVerdict(), role="judge", prompt_version=1, model="m", fidelity="summary"
            )
        unprobed = store.unjudged(role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False)
        assert unprobed == store._loadall_refresh("judge", 1, None, False)
        assert [row["dedup_key"] for row in unprobed] == [f"k{i}" for i in (6, 7, 0, 1, 2, 3, 4, 5)]
        probed = store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
        assert probed == store._loadall_refresh("judge", 1, None, True)
        assert [row["dedup_key"] for row in probed][:2] == ["k6", "k7"]


def test_standalone_write_never_joins_a_foreign_transaction(tmp_path: Path) -> None:
    async def scenario(store: FeedbackStore) -> None:
        in_transaction = asyncio.Event()

        async def owner() -> None:
            with pytest.raises(RuntimeError, match="owner rolls back"):
                with store.transaction():
                    store.record_file("/owned.jsonl", 1.0)
                    in_transaction.set()
                    await asyncio.sleep(0.01)
                    raise RuntimeError("owner rolls back")

        async def outsider() -> None:
            await in_transaction.wait()
            with pytest.raises(TransactionConflictError):
                store.record_file("/outsider.jsonl", 2.0)

        await asyncio.gather(owner(), outsider())

    with open_store(tmp_path / "state.db") as store:
        asyncio.run(scenario(store))
        store.record_file("/outsider.jsonl", 2.0)
        assert store.file_mtimes() == {"/outsider.jsonl": 2.0}


def test_owner_task_composes_inside_its_own_transaction(tmp_path: Path) -> None:
    async def scenario(store: FeedbackStore) -> None:
        with store.transaction():
            await asyncio.sleep(0)
            store.record_file("/owned.jsonl", 1.0)

    with open_store(tmp_path / "state.db") as store:
        asyncio.run(scenario(store))
        assert store.file_mtimes() == {"/owned.jsonl": 1.0}


def test_nested_transaction_raises(tmp_path: Path) -> None:
    with open_store(tmp_path / "state.db") as store:
        with store.transaction(), pytest.raises(TransactionConflictError), store.transaction():
            raise AssertionError("unreachable")
