"""The file-state ledger and transaction discipline, now on the ``FeedbackStore`` facade.

``FileStateStore`` folded into the facade at the native-store flip; these pin the
scanned-file mtime table, an exact schema with a foreign-keyed consumer
table, and the ``_txn_owner`` transaction-conflict guard the facade carries in Python.
"""

from __future__ import annotations

import asyncio
import ctypes
import ctypes.util
import gc
import os
import sqlite3
import sys
import threading
import time
from pathlib import Path

import pytest

from cc_transcript.corrections import CorrectionLog
from cc_transcript.mining import store as store_module
from cc_transcript.mining.candidates import DedupKey
from cc_transcript.mining.store import FeedbackStore, StoreSchema, TransactionConflictError
from tests import store_contract_fixtures as fx

pytestmark = pytest.mark.anyio


def os_thread_count() -> int:
    """Live OS-thread count for this process — the actor threads are raw Rust ``std::thread``s
    invisible to ``threading.active_count``, so a leak only shows at the OS level."""
    if sys.platform == "linux":
        return len(os.listdir("/proc/self/task"))
    if sys.platform != "darwin":
        pytest.skip(f"no OS-thread count on {sys.platform}")
    mach = ctypes.CDLL(ctypes.util.find_library("System"), use_errno=True)
    mach.mach_task_self.restype = ctypes.c_uint
    mach.task_threads.argtypes = [
        ctypes.c_uint,
        ctypes.POINTER(ctypes.POINTER(ctypes.c_uint)),
        ctypes.POINTER(ctypes.c_uint),
    ]
    mach.vm_deallocate.argtypes = [ctypes.c_uint, ctypes.c_void_p, ctypes.c_size_t]
    mach.mach_port_deallocate.argtypes = [ctypes.c_uint, ctypes.c_uint]
    task = mach.mach_task_self()
    threads = ctypes.POINTER(ctypes.c_uint)()
    count = ctypes.c_uint(0)
    if mach.task_threads(task, ctypes.byref(threads), ctypes.byref(count)) != 0:
        raise OSError("task_threads failed")
    total = count.value
    for i in range(total):
        mach.mach_port_deallocate(task, threads[i])
    mach.vm_deallocate(task, ctypes.cast(threads, ctypes.c_void_p), total * ctypes.sizeof(ctypes.c_uint))
    return total


async def settled_thread_count(baseline: int, *, tolerance: int, tries: int = 60) -> int:
    for _ in range(tries):
        gc.collect()
        if (count := os_thread_count()) - baseline <= tolerance:
            return count
        await asyncio.sleep(0.05)
    return os_thread_count()

EXTRA_SCHEMA = """
CREATE TABLE notes (
  path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
  body TEXT NOT NULL
);
"""


async def open_store(path: Path, *, extra: str = "") -> FeedbackStore:
    if not extra:
        return await FeedbackStore.open(path)
    return await FeedbackStore.open(
        path,
        StoreSchema(
            identity="cc-transcript-feedback-notes-test",
            ddl=store_module.DEFAULT_SCHEMA_DDL + extra,
        ),
    )


async def test_record_and_read_mtimes(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db") as store:
        await store.record_file("/a.jsonl", 1.0)
        await store.record_file("/b.jsonl", 2.0)
        await store.record_file("/a.jsonl", 3.0)
        assert await store.file_mtimes() == {"/a.jsonl": 3.0, "/b.jsonl": 2.0}


async def test_extra_schema_table_created(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        await store.record_file("/a.jsonl", 1.0)
        async with store.transaction() as db:
            await db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/a.jsonl", "hi"))
        rows = [(row["path"], row["body"]) for row in await store.sql("SELECT path, body FROM notes")]
    assert rows == [("/a.jsonl", "hi")]


async def test_transaction_is_atomic_across_record_and_extra(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        with pytest.raises(sqlite3.IntegrityError):
            async with store.transaction() as db:
                await store.record_file("/c.jsonl", 9.0)
                await db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/missing.jsonl", "x"))
        assert await store.file_mtimes() == {}


async def test_transaction_commits_record_and_extra_together(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db", extra=EXTRA_SCHEMA) as store:
        async with store.transaction() as db:
            await store.record_file("/d.jsonl", 4.0)
            await db.execute("INSERT INTO notes(path, body) VALUES(?, ?)", ("/d.jsonl", "note"))
        notes = (await store.sql("SELECT COUNT(*) AS n FROM notes"))[0]["n"]
        assert await store.file_mtimes() == {"/d.jsonl": 4.0}
        assert notes == 1


async def test_close_releases_the_connection(tmp_path: Path) -> None:
    store = await open_store(tmp_path / "state.db")
    async with store:
        await store.record_file("/a.jsonl", 1.0)
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        await store.sql("SELECT 1")


async def test_double_close_is_a_noop(tmp_path: Path) -> None:
    store = await open_store(tmp_path / "state.db")
    await store.close()
    await store.close()


async def test_exit_after_manual_close_does_not_raise(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db") as store:
        await store.record_file("/a.jsonl", 1.0)
        await store.close()


async def test_aliased_engine_is_refused_after_close(tmp_path: Path) -> None:
    store = await open_store(tmp_path / "state.db")
    engine = store.engine
    await store.close()
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        engine.sql("SELECT 1")


class SummaryVerdict:
    category = "wrong_approach"
    accepted = True
    summary = "seed summary"
    confidence = 0.9
    rationale = "stub"
    canonical_key = None


async def test_unlimited_filtered_refresh_matches_the_eager_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(store_module, "REFRESH_PAGE_SIZE", 3)
    async with await FeedbackStore.open(tmp_path / "f.db", StoreSchema(event_filter="e.id > 0")) as store:
        await store.record_file_scan("/scan.jsonl", 1.0, [fx.candidate(f"k{i}") for i in range(8)])
        for i in range(6):
            await store.record_verdict(
                DedupKey(f"k{i}"), SummaryVerdict(), role="judge", prompt_version=1, model="m", fidelity="summary"
            )
        unprobed = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False)
        assert unprobed == await store._loadall_refresh("judge", 1, None, False)
        assert [row["dedup_key"] for row in unprobed] == [f"k{i}" for i in (6, 7, 0, 1, 2, 3, 4, 5)]
        probed = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
        assert probed == await store._loadall_refresh("judge", 1, None, True)
        assert [row["dedup_key"] for row in probed][:2] == ["k6", "k7"]


async def test_standalone_write_never_joins_a_foreign_transaction(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db") as store:
        in_transaction = asyncio.Event()

        async def owner() -> None:
            with pytest.raises(RuntimeError, match="owner rolls back"):
                async with store.transaction():
                    await store.record_file("/owned.jsonl", 1.0)
                    in_transaction.set()
                    await asyncio.sleep(0.01)
                    raise RuntimeError("owner rolls back")

        async def outsider() -> None:
            await in_transaction.wait()
            with pytest.raises(TransactionConflictError):
                await store.record_file("/outsider.jsonl", 2.0)

        await asyncio.gather(owner(), outsider())
        await store.record_file("/outsider.jsonl", 2.0)
        assert await store.file_mtimes() == {"/outsider.jsonl": 2.0}


async def test_owner_task_composes_inside_its_own_transaction(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db") as store:
        async with store.transaction():
            await asyncio.sleep(0)
            await store.record_file("/owned.jsonl", 1.0)
        assert await store.file_mtimes() == {"/owned.jsonl": 1.0}


async def test_nested_transaction_raises(tmp_path: Path) -> None:
    async with await open_store(tmp_path / "state.db") as store:
        async with store.transaction():
            with pytest.raises(TransactionConflictError):
                async with store.transaction():
                    raise AssertionError("unreachable")


async def test_concurrent_cross_thread_transactions_conflict_not_operational(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The delayed current_owner widens the check->set window an unguarded facade would race
    # (two begins -> native OperationalError); the lock keeps it atomic across the two loops.
    real_owner = store_module.current_owner

    def slow_owner() -> tuple[int, int | None]:
        time.sleep(0.005)
        return real_owner()

    monkeypatch.setattr(store_module, "current_owner", slow_owner)

    async with await open_store(tmp_path / "state.db") as store:
        barrier = threading.Barrier(2)

        def worker() -> str | BaseException:
            async def body() -> str:
                barrier.wait(timeout=5)
                async with store.transaction():
                    await asyncio.sleep(0.01)
                return "ok"

            try:
                return asyncio.run(body())
            except BaseException as error:  # noqa: BLE001 — classify winner vs. loser
                return error

        outcomes = await asyncio.gather(asyncio.to_thread(worker), asyncio.to_thread(worker))

    committed = [outcome for outcome in outcomes if outcome == "ok"]
    errors = [outcome for outcome in outcomes if isinstance(outcome, BaseException)]
    assert len(committed) == 1
    assert len(errors) == 1
    assert isinstance(errors[0], TransactionConflictError)
    assert not isinstance(errors[0], sqlite3.OperationalError)


async def test_failed_open_does_not_leak_actor_threads(tmp_path: Path) -> None:
    # Retaining the raised exceptions pins their frames and the discarded engine locals, so an
    # unclosed actor stays alive; the open() guard drains it before re-raising.
    notdb = tmp_path / "bad.db"
    notdb.write_bytes(b"this is not an sqlite database file, padding padding padding")
    retained: list[BaseException] = []

    gc.collect()
    await asyncio.sleep(0.1)
    baseline = os_thread_count()

    for _ in range(12):
        try:
            await FeedbackStore.open(notdb)
        except sqlite3.DatabaseError as error:
            retained.append(error)
        try:
            await CorrectionLog.open(notdb)
        except sqlite3.DatabaseError as error:
            retained.append(error)

    assert len(retained) == 24
    leaked = await settled_thread_count(baseline, tolerance=4) - baseline
    assert leaked <= 4, f"leaked {leaked} OS threads across 24 retained failed opens"
