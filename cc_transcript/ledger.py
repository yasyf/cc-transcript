"""The append-only SQLite base under the decision ledger.

A family ledger is one durable, WAL-mode, ``INSERT OR IGNORE`` table behind
a fixed schema, so :class:`AsyncLedger` owns a :class:`ConnectionActor`, the
``open`` plumbing, and the schema-driven append and read — a ledger supplies
only its DDL, filename, table, columns, and row mapper. The corrections ledger
left this base in v14: its one write codepath is the native engine behind
:class:`~cc_transcript.corrections.CorrectionLog`.

Import-light by contract, like :mod:`cc_transcript.ids`: the standard library
plus identity primitives only, so a hook reading a ledger pays nothing for the
parser. The async surface rides one dedicated ``sqlite3`` worker thread, so no
event loop ever blocks on disk while ``check_same_thread`` still holds.
"""

from __future__ import annotations

import asyncio
import json
import queue
import sqlite3
import threading
from abc import ABC, abstractmethod
from pathlib import Path
from typing import TYPE_CHECKING, Any, ClassVar, Protocol, Self

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

    from cc_transcript.ids import SessionId


def open_sqlite(path: Path | None, *, filename: str, ddl: str) -> sqlite3.Connection:
    path = path or Path.home() / ".cc-transcript" / filename
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path, autocommit=True)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode = WAL")
    conn.execute("PRAGMA busy_timeout = 2000")
    conn.executescript(ddl)
    return conn


class ConnectionActor:
    """A daemon worker thread owning one :mod:`sqlite3` connection.

    The connection is created on the worker thread and every statement runs
    there, so ``check_same_thread`` holds while the async API stays on the event
    loop. :meth:`run` captures the running loop live per call, so one held actor
    serves across successive :func:`asyncio.run` invocations — the hook idiom —
    and resolves each result on whichever loop submitted it. Exceptions surface
    on the awaiter as the real :mod:`sqlite3` classes; the thread is a daemon, so
    a leaked actor never blocks interpreter exit.
    """

    def __init__(self) -> None:
        self._jobs: queue.Queue[tuple[asyncio.AbstractEventLoop, asyncio.Future[Any], Callable[[], Any]]] = queue.Queue()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._closed = False
        self.conn: sqlite3.Connection

    def _serve(self) -> None:
        while True:
            loop, future, fn = self._jobs.get()
            try:
                result = fn()
            except Exception as exc:
                loop.call_soon_threadsafe(self._settle, future, exc, True)
            else:
                loop.call_soon_threadsafe(self._settle, future, result, False)

    @staticmethod
    def _settle(future: asyncio.Future[Any], value: Any, failed: bool) -> None:
        if future.cancelled():
            return
        future.set_exception(value) if failed else future.set_result(value)

    async def start(self, connect: Callable[[], sqlite3.Connection]) -> None:
        """Starts the worker thread and opens the connection as its first job."""
        self._thread.start()
        self.conn = await self.run(connect)

    async def run[T](self, fn: Callable[[], T]) -> T:
        """Runs ``fn`` on the worker thread, resolving on the calling loop."""
        loop = asyncio.get_running_loop()
        future: asyncio.Future[T] = loop.create_future()
        self._jobs.put((loop, future, fn))
        return await future

    async def close(self) -> None:
        """Closes the connection on the worker thread; a second call is a no-op."""
        if self._closed:
            return
        self._closed = True
        await self.run(self.conn.close)


class LedgerRecord(Protocol):
    @property
    def detail(self) -> Mapping[str, Any]: ...


class AsyncLedger[R: LedgerRecord](ABC):
    DDL: ClassVar[str]
    FILENAME: ClassVar[str]
    TABLE: ClassVar[str]
    COLUMNS: ClassVar[tuple[str, ...]]

    def __init__(self, actor: ConnectionActor) -> None:
        self._actor = actor

    @abstractmethod
    def row_to_record(self, row: sqlite3.Row) -> R: ...

    @classmethod
    async def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the ledger at ``path``.

        Args:
            path: The database file path; its parents are created if absent.
                Defaults to the ledger's file under ``~/.cc-transcript``.

        Returns:
            The opened log, backed by a live connection actor.
        """
        actor = ConnectionActor()
        await actor.start(lambda: open_sqlite(path, filename=cls.FILENAME, ddl=cls.DDL))
        return cls(actor)

    async def append(self, record: R) -> None:
        """Appends ``record`` as a single ``INSERT OR IGNORE``.

        Idempotent on the table's UNIQUE key, so re-running a writer writes one
        row; SQLite treats NULL key columns as distinct, so rows whose key
        carries a NULL rely on the writer not repeating the same values.
        """
        conn = self._actor.conn
        sql = (
            f"INSERT OR IGNORE INTO {self.TABLE} ({', '.join(self.COLUMNS)}) "
            f"VALUES ({', '.join(['?'] * len(self.COLUMNS))})"
        )
        params = tuple(
            json.dumps(dict(record.detail)) if column == "detail_json" else getattr(record, column)
            for column in self.COLUMNS
        )
        await self._actor.run(lambda: conn.execute(sql, params))

    async def for_session(self, session_id: SessionId) -> tuple[R, ...]:
        """All records for ``session_id``, ordered by timestamp."""
        conn = self._actor.conn
        sql = f"SELECT * FROM {self.TABLE} WHERE session_id = ? ORDER BY ts_ms, id"
        rows = await self._actor.run(lambda: conn.execute(sql, (session_id,)).fetchall())
        return tuple(self.row_to_record(row) for row in rows)

    async def close(self) -> None:
        """Closes the underlying connection; a second call is a no-op."""
        await self._actor.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()
