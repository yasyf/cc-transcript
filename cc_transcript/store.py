from __future__ import annotations

import sqlite3
import threading
from contextlib import contextmanager
from typing import TYPE_CHECKING, Self

if TYPE_CHECKING:
    from collections.abc import Generator
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

    Backed by a single SQLite database with WAL journaling and a process-wide
    lock, so it is safe to share one store across threads. Consumers compose
    their own writes alongside :meth:`record_file` inside :meth:`transaction`
    to keep ingestion state and derived records atomic.

    Example:
        >>> store = FileStateStore.open(Path("state.db"), extra_schema=MY_SCHEMA)
        >>> with store.transaction() as conn:
        ...     conn.execute("INSERT INTO my_table VALUES (?)", (value,))
        ...     store.record_file(str(path), mtime)
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn
        self.lock = threading.RLock()
        self._in_transaction = False

    @classmethod
    def open(cls, path: Path, *, extra_schema: str = "") -> Self:
        """Opens (creating if needed) the store at ``path``.

        Args:
            path: The database file path; its parent is created if absent.
            extra_schema: Additional DDL to execute after the file schema,
                e.g. consumer tables that reference ``files(path)``.

        Returns:
            The opened store.
        """
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(path), check_same_thread=False)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys = ON")
        conn.execute("PRAGMA journal_mode = WAL")
        conn.executescript(FILE_SCHEMA + extra_schema)
        conn.commit()
        return cls(conn)

    def close(self) -> None:
        """Closes the underlying connection."""
        with self.lock:
            self.conn.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    @contextmanager
    def transaction(self) -> Generator[sqlite3.Connection]:
        """Yields the locked connection inside a single committed transaction.

        Use this to compose consumer writes with :meth:`record_file` so they
        commit or roll back together. :meth:`record_file` called within the
        block joins this transaction instead of opening its own.

        Yields:
            The store's connection, held under the store lock.
        """
        with self.lock, self.conn:
            self._in_transaction = True
            try:
                yield self.conn
            finally:
                self._in_transaction = False

    def file_mtimes(self) -> dict[str, float]:
        """Returns the recorded ``path`` to ``mtime`` map."""
        with self.lock:
            return {row["path"]: row["mtime"] for row in self.conn.execute("SELECT path, mtime FROM files")}

    def record_file(self, path: str, mtime: float) -> None:
        """Upserts the recorded mtime for ``path``.

        Call inside :meth:`transaction` to commit alongside consumer writes;
        called on its own it commits immediately.
        """
        with self.lock:
            if self._in_transaction:
                self.upsert_file(path, mtime)
                return
            with self.conn:
                self.upsert_file(path, mtime)

    def upsert_file(self, path: str, mtime: float) -> None:
        self.conn.execute(
            "INSERT INTO files(path, mtime) VALUES(?, ?) ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
            (path, mtime),
        )


__all__ = ["FILE_SCHEMA", "FileStateStore"]
