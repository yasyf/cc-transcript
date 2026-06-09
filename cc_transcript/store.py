from __future__ import annotations

from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Self

import aiosqlite
import anyio

if TYPE_CHECKING:
    from collections.abc import AsyncIterator
    from pathlib import Path
    from types import TracebackType

FILE_SCHEMA = """
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY,
  mtime REAL NOT NULL
);
"""


class FileStateStore:
    """Tracks which transcript files have been ingested, keyed by mtime.

    Backed by a single async SQLite (``aiosqlite``) database with WAL journaling
    and a task lock, so it is safe to share one store across concurrent tasks.
    Consumers compose their own writes alongside :meth:`record_file` inside
    :meth:`transaction` to keep ingestion state and derived records atomic.

    Example:
        >>> store = await FileStateStore.open(Path("state.db"), extra_schema=MY_SCHEMA)
        >>> async with store.transaction() as conn:
        ...     await conn.execute("INSERT INTO my_table VALUES (?)", (value,))
        ...     await store.record_file(str(path), mtime)
    """

    def __init__(self, conn: aiosqlite.Connection) -> None:
        self.conn = conn
        self.lock = anyio.Lock()
        self._txn_owner: int | None = None

    @classmethod
    async def open(cls, path: Path, *, extra_schema: str = "") -> Self:
        """Opens (creating if needed) the store at ``path``.

        Args:
            path: The database file path; its parent is created if absent.
            extra_schema: Additional DDL to execute after the file schema,
                e.g. consumer tables that reference ``files(path)``.

        Returns:
            The opened store.
        """
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = await aiosqlite.connect(str(path), isolation_level=None)
        conn.row_factory = aiosqlite.Row
        await conn.execute("PRAGMA foreign_keys = ON")
        await conn.execute("PRAGMA journal_mode = WAL")
        await conn.executescript(FILE_SCHEMA + extra_schema)
        return cls(conn)

    async def close(self) -> None:
        """Closes the underlying connection."""
        async with self.lock:
            await self.conn.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.close()

    @asynccontextmanager
    async def transaction(self) -> AsyncIterator[aiosqlite.Connection]:
        """Yields the locked connection inside a single committed transaction.

        Use this to compose consumer writes with :meth:`record_file` so they
        commit or roll back together. :meth:`record_file` called within the
        block joins this transaction instead of opening its own.

        Yields:
            The store's connection, held under the store lock.
        """
        async with self.lock:
            self._txn_owner = anyio.get_current_task().id
            await self.conn.execute("BEGIN IMMEDIATE")
            try:
                yield self.conn
            except BaseException:
                await self.conn.rollback()
                raise
            else:
                await self.conn.commit()
            finally:
                self._txn_owner = None

    async def file_mtimes(self) -> dict[str, float]:
        """Returns the recorded ``path`` to ``mtime`` map."""
        async with self.lock, self.conn.execute("SELECT path, mtime FROM files") as cur:
            return {row["path"]: row["mtime"] async for row in cur}

    async def record_file(self, path: str, mtime: float) -> None:
        """Upserts the recorded mtime for ``path``.

        Call inside :meth:`transaction` to commit alongside consumer writes;
        called on its own it commits immediately.
        """
        if self._txn_owner == anyio.get_current_task().id:
            await self.upsert_file(path, mtime)
            return
        async with self.lock:
            await self.conn.execute("BEGIN IMMEDIATE")
            try:
                await self.upsert_file(path, mtime)
            except BaseException:
                await self.conn.rollback()
                raise
            else:
                await self.conn.commit()

    async def upsert_file(self, path: str, mtime: float) -> None:
        await self.conn.execute(
            "INSERT INTO files(path, mtime) VALUES(?, ?) ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
            (path, mtime),
        )
