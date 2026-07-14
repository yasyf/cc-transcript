"""The append-only SQLite base under the decision ledger.

A family ledger is one durable, WAL-mode, ``INSERT OR IGNORE`` table behind
a fixed schema, so :class:`SyncLedger` owns the connection, the ``open`` plumbing,
and the schema-driven append and read — a ledger supplies only its DDL,
filename, table, columns, and row mapper. The corrections ledger left this base
in v14: its one write codepath is the native engine behind
:class:`~cc_transcript.corrections.CorrectionLog`.

Import-light by contract, like :mod:`cc_transcript.ids`: the standard library
plus identity primitives only, so a hook reading a ledger pays nothing for the
parser.
"""

from __future__ import annotations

import json
import sqlite3
from abc import ABC, abstractmethod
from pathlib import Path
from typing import TYPE_CHECKING, ClassVar, Protocol, Self

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

    from cc_transcript.ids import SessionId


class LedgerRecord(Protocol):
    @property
    def detail(self) -> Mapping[str, Any]: ...


class SyncLedger[R: LedgerRecord](ABC):
    DDL: ClassVar[str]
    FILENAME: ClassVar[str]
    TABLE: ClassVar[str]
    COLUMNS: ClassVar[tuple[str, ...]]

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn

    @abstractmethod
    def row_to_record(self, row: sqlite3.Row) -> R: ...

    @classmethod
    def open(cls, path: Path | None = None) -> Self:
        """Opens (creating if needed) the ledger at ``path``.

        Args:
            path: The database file path; its parents are created if absent.
                Defaults to the ledger's file under ``~/.cc-transcript``.

        Returns:
            The opened log.
        """
        path = path or Path.home() / ".cc-transcript" / cls.FILENAME
        path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(path, autocommit=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA busy_timeout = 2000")
        conn.executescript(cls.DDL)
        return cls(conn)

    def append(self, record: R) -> None:
        """Appends ``record`` as a single ``INSERT OR IGNORE``.

        Idempotent on the table's UNIQUE key, so re-running a writer writes one
        row; SQLite treats NULL key columns as distinct, so rows whose key
        carries a NULL rely on the writer not repeating the same values.
        """
        self.conn.execute(
            f"INSERT OR IGNORE INTO {self.TABLE} ({', '.join(self.COLUMNS)}) "
            f"VALUES ({', '.join(['?'] * len(self.COLUMNS))})",
            tuple(
                json.dumps(dict(record.detail)) if column == "detail_json" else getattr(record, column)
                for column in self.COLUMNS
            ),
        )

    def for_session(self, session_id: SessionId) -> tuple[R, ...]:
        """All records for ``session_id``, ordered by timestamp."""
        return tuple(
            self.row_to_record(row)
            for row in self.conn.execute(
                f"SELECT * FROM {self.TABLE} WHERE session_id = ? ORDER BY ts_ms, id", (session_id,)
            )
        )
