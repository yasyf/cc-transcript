"""The native async persistence contract on ``RustFeedbackStore``.

The actor thread owns the connection: ops return an ``asyncio.Future`` resolved from that
thread. These pin the lifecycle (drain-on-close, idempotent double-close, the synchronous
closed-handle and no-running-loop errors), aiosqlite's cancellation no-op, cross-loop sharing
of one handle, open-failure surfacing through ``await open()``, and exception fidelity through
the future — all matching the strings the sync engine raised.
"""

from __future__ import annotations

import asyncio
import re
import sqlite3
import time
from collections.abc import Mapping

import pytest

from cc_transcript import _native
from cc_transcript.mining.store import DEFAULT_SCHEMA_DDL

pytestmark = pytest.mark.anyio

DEFAULTS: dict[str, object] = {
    "schema_identity": "cc-transcript-feedback-test",
    "schema_ddl": DEFAULT_SCHEMA_DDL,
    "event_columns": [],
    "extension_paths": [],
    "verdict_table": "verdicts",
    "accepted_column": "accepted",
    "summary_column": "summary",
    "event_filter": None,
}


def make_store(path: object, **overrides: object) -> _native.RustFeedbackStore:
    return _native.RustFeedbackStore(str(path), **(DEFAULTS | overrides))


async def open_store(path: object, **overrides: object) -> _native.RustFeedbackStore:
    store = make_store(path, **overrides)
    await store.open()
    return store


async def test_queued_writes_drain_before_close_resolves(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    order: list[object] = []
    futures = []
    for i in range(20):
        future = store.execute("INSERT INTO files(path, mtime) VALUES (?, ?)", [f"/{i}", i])
        future.add_done_callback(lambda _f, i=i: order.append(i))
        futures.append(future)
    close_future = store.close()
    close_future.add_done_callback(lambda _f: order.append("close"))
    await asyncio.gather(*futures, close_future)
    assert order[-1] == "close"
    assert sorted(order[:-1]) == list(range(20))

    reopened = await open_store(tmp_path / "feedback.db")
    rows = await reopened.sql("SELECT mtime FROM files ORDER BY rowid")
    assert [row["mtime"] for row in rows] == list(range(20))
    await reopened.close()


async def test_cancelling_a_future_still_applies_the_write(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    future = store.execute("INSERT INTO files(path, mtime) VALUES (?, ?)", ["/x", 99])
    future.cancel()
    with pytest.raises(asyncio.CancelledError):
        await future
    rows = await store.sql("SELECT mtime FROM files")
    assert [row["mtime"] for row in rows] == [99]
    await store.close()


async def test_ops_after_close_raise_synchronously_and_double_close_is_idempotent(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    await store.close()
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        store.sql("SELECT 1")
    await store.close()


async def test_op_without_a_running_loop_raises_runtimeerror(tmp_path) -> None:
    store = make_store(tmp_path / "feedback.db")
    with pytest.raises(RuntimeError):
        await asyncio.to_thread(store.open)


async def test_two_event_loops_share_one_handle(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")

    def worker(name: str) -> int:
        async def body() -> int:
            await store.execute("INSERT INTO files(path, mtime) VALUES (?, 1)", [name])
            return (await store.sql("SELECT COUNT(*) AS n FROM files"))[0]["n"]

        return asyncio.run(body())

    counts = await asyncio.gather(
        asyncio.to_thread(worker, "a"),
        asyncio.to_thread(worker, "b"),
    )
    assert all(1 <= count <= 2 for count in counts)
    rows = await store.sql("SELECT path FROM files ORDER BY path")
    assert [row["path"] for row in rows] == ["a", "b"]
    await store.close()


async def test_bad_path_open_failure_surfaces_through_await_open(tmp_path) -> None:
    a_directory = tmp_path / "not-a-file"
    a_directory.mkdir()
    store = make_store(a_directory)
    with pytest.raises(sqlite3.OperationalError):
        await store.open()
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        await store.sql("SELECT 1")
    await store.close()


async def test_foreign_schema_open_failure_surfaces_through_await_open(tmp_path) -> None:
    path = tmp_path / "feedback.db"
    conn = sqlite3.connect(path)
    conn.execute("CREATE TABLE foreign_table(id INTEGER)")
    conn.close()
    store = make_store(path, readonly=True)
    with pytest.raises(sqlite3.DatabaseError, match="schema"):
        await store.open()
    with pytest.raises(sqlite3.ProgrammingError, match=r"Cannot operate on a closed database\."):
        await store.sql("SELECT 1")
    await store.close()


async def test_bind_arity_error_arrives_through_the_future(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    with pytest.raises(
        sqlite3.ProgrammingError,
        match=re.escape(
            "Incorrect number of bindings supplied. The current statement uses 1, "
            "and there are 0 supplied."
        ),
    ):
        await store.sql("SELECT ?", [])
    await store.close()


async def test_integrity_error_carries_sqlite_errorname_through_the_future(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    await store.execute("INSERT INTO files(path, mtime) VALUES ('x', 1)", [])
    with pytest.raises(sqlite3.IntegrityError) as excinfo:
        await store.execute("INSERT INTO files(path, mtime) VALUES ('x', 2)", [])
    assert excinfo.value.sqlite_errorname == "SQLITE_CONSTRAINT_PRIMARYKEY"
    await store.close()


async def test_multiple_statements_error_arrives_through_the_future(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    with pytest.raises(
        sqlite3.ProgrammingError, match="You can only execute one statement at a time."
    ):
        await store.sql("SELECT 1; SELECT 2")
    await store.close()


async def test_executescript_refused_mid_transaction_through_the_future(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    await store.begin_immediate()
    with pytest.raises(sqlite3.ProgrammingError, match="cannot executescript"):
        await store.executescript("INSERT INTO files(path, mtime) VALUES ('x', 1);")
    await store.rollback()
    await store.close()


async def test_fifo_order_preserved_across_concurrent_submits(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    futures = [
        store.execute("INSERT INTO files(path, mtime) VALUES (?, ?)", [f"/{i}", i])
        for i in range(50)
    ]
    await asyncio.gather(*futures)
    assert await store.last_insert_rowid() == 50
    rows = await store.sql("SELECT mtime FROM files ORDER BY rowid")
    assert [row["mtime"] for row in rows] == list(range(50))
    await store.close()


async def test_a_slow_query_runs_without_holding_the_gil(tmp_path) -> None:
    store = await open_store(tmp_path / "feedback.db")
    # A recursive CTE that spins for a measurable stretch of pure SQL on the actor thread;
    # the GIL must be free for that whole stretch so other Python threads keep running.
    slow_sql = (
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < ?) "
        "SELECT count(*) AS n FROM c"
    )
    span = 5_000_000

    def pure_python_work() -> float:
        start = time.perf_counter()
        acc = 0
        for i in range(100_000):
            acc += i * i
        return time.perf_counter() - start

    query_start = time.perf_counter()
    await store.sql(slow_sql, [span])
    query_seconds = time.perf_counter() - query_start

    worker_seconds, _ = await asyncio.gather(
        asyncio.to_thread(pure_python_work),
        store.sql(slow_sql, [span]),
    )
    # A GIL held for the query would stall the worker ~query long; GIL-free SQL lets it finish
    # in a small fraction. Generous bound (half the query), no tight timing.
    assert worker_seconds < query_seconds * 0.5
    await store.close()


async def test_reentrant_detail_close_during_append_does_not_deadlock(tmp_path) -> None:
    engine = _native.RustCorrectionLog(str(tmp_path / "corrections.db"))
    await engine.open()

    class ClosingDetail(Mapping):
        def __init__(self, log: object) -> None:
            self._log = log

        def __iter__(self):
            self._log.close()  # re-enter the binding mid-append
            return iter(())

        def __len__(self) -> int:
            return 0

        def __getitem__(self, key: object) -> object:
            raise KeyError(key)

    def append_in_its_own_loop() -> None:
        async def body() -> None:
            await engine.append(
                0, "s", "src", "a", None, "f", "old", "new",
                None, None, None, None, None, None, 0.0, ClosingDetail(engine),
            )

        asyncio.run(body())

    # dict(detail)'s iteration runs before the state lock, so a close() it triggers cannot
    # deadlock on that lock: the append resolves or raises the closed-db error, never hangs.
    try:
        await asyncio.wait_for(asyncio.to_thread(append_in_its_own_loop), timeout=10)
    except TimeoutError:
        pytest.fail("append deadlocked when detail iteration re-entered close()")
    except sqlite3.ProgrammingError:
        pass
