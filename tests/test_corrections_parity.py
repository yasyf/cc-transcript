"""Rust↔Python parity for the correction ledger on the same on-disk file.

The ledger format is a cross-language contract — cc-review's Go reads it directly — so
the Rust ``RustCorrectionLog`` engine and an independent ``sqlite3``-library reference
must stay byte-compatible. The shipping ``CorrectionLog`` facade delegates to the native
engine (one engine per process), so the reference leg lives here as ``PyReferenceLog``,
the pre-v14 sqlite3 implementation. These tests write with one engine and read with the
other over one database file, pinning: identical query results (record shape), identical ``sqlite_master``
schema, WAL journal mode, ``PRAGMA busy_timeout``, raw row bytes (``detail_json`` included),
``INSERT OR IGNORE`` idempotency, the single-statement ``sql`` rule, ``dict``-normalized
detail (NaN/Infinity/lone surrogates and non-mapping raises), storage-class reads, FIFO
ordering of concurrently submitted ops, cross-connection write-lock contention, and the
``sqlite3``/``OSError`` exception types and payloads raised.

The native engine is async: the actor thread owns the connection, so a handle opens with
``await log.open()`` and every op returns an ``asyncio.Future``. Open-time failures (bad
path, non-database file, ``OSError``) surface through ``await log.open()``; ``sql`` and
other engine errors surface through the awaited op; param conversion (non-mapping detail)
still raises synchronously, before any future.

Two SQLite libraries in one process cannot coordinate POSIX locks, so these tests access
each ledger file SEQUENTIALLY across engines (never concurrent Python-sqlite3 + Rust on one
file).
"""

from __future__ import annotations

import asyncio
import json
import math
import pathlib
import sqlite3
from dataclasses import asdict

import pytest

from cc_transcript import _native
from cc_transcript.corrections import Correction
from cc_transcript.ids import EventUuid, SessionId
from tests.support import ANCHOR, DIGEST_A, DIGEST_B, DIGEST_C, OTHER_SESSION, SESSION, correction, requires_rust

pytestmark = pytest.mark.anyio

OTHER_ANCHOR = EventUuid("anchor-2")

COLUMNS = (
    "ts_ms",
    "session_id",
    "source",
    "anchor_uuid",
    "incorrect_digest",
    "incorrect_file",
    "incorrect_old",
    "incorrect_new",
    "correction_origin",
    "correction_file",
    "correction_old",
    "correction_new",
    "correction_commit",
    "correction_text",
    "overlap",
    "detail_json",
)


class PyReferenceLog:
    """The sqlite3-library reference engine — the pre-v14 CorrectionLog, verbatim."""

    def __init__(self, conn: sqlite3.Connection) -> None:
        self.conn = conn

    @classmethod
    def open(cls, path: pathlib.Path) -> PyReferenceLog:
        conn = sqlite3.connect(path, autocommit=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode = WAL")
        conn.execute("PRAGMA busy_timeout = 2000")
        return cls(conn)

    def append(self, record: Correction) -> None:
        self.conn.execute(
            f"INSERT OR IGNORE INTO corrections ({', '.join(COLUMNS)}) VALUES ({', '.join(['?'] * len(COLUMNS))})",
            tuple(
                json.dumps(dict(record.detail)) if column == "detail_json" else getattr(record, column)
                for column in COLUMNS
            ),
        )

    def row_to_record(self, row: sqlite3.Row) -> Correction:
        return Correction(
            **{column: row[column] for column in COLUMNS if column != "detail_json"},
            detail=json.loads(row["detail_json"]),
        )

    def query(self, sql: str, params: tuple[object, ...]) -> tuple[Correction, ...]:
        return tuple(self.row_to_record(row) for row in self.conn.execute(sql, params))

    def for_session(self, session_id: str) -> tuple[Correction, ...]:
        return self.query("SELECT * FROM corrections WHERE session_id = ? ORDER BY ts_ms, id", (session_id,))

    def for_repo(self, repo: str) -> tuple[Correction, ...]:
        return self.query(
            "SELECT * FROM corrections WHERE json_extract(detail_json, '$.repo') = ? ORDER BY ts_ms, id", (repo,)
        )

    def since(self, ts_ms: int, *, source: str | None = None) -> tuple[Correction, ...]:
        if source is None:
            return self.query("SELECT * FROM corrections WHERE ts_ms > ? ORDER BY ts_ms, id", (ts_ms,))
        return self.query(
            "SELECT * FROM corrections WHERE ts_ms > ? AND source = ? ORDER BY ts_ms, id", (ts_ms, source)
        )

    def for_anchor(self, session_id: str, anchor_uuid: str) -> tuple[Correction, ...]:
        return self.query(
            "SELECT * FROM corrections WHERE session_id = ? AND anchor_uuid = ? ORDER BY ts_ms, id",
            (session_id, anchor_uuid),
        )

    def by_digest(self, session_id: str, *, incorrect_digest: str) -> tuple[Correction, ...]:
        return self.query(
            "SELECT * FROM corrections WHERE session_id = ? AND incorrect_digest = ? ORDER BY ts_ms, id",
            (session_id, incorrect_digest),
        )


def fixture_rows() -> list[Correction]:
    return [
        correction(ts_ms=1_000, incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR, detail={"rule": "overlap", "turn": 3}),
        correction(
            ts_ms=2_000,
            incorrect_digest=DIGEST_B,
            anchor_uuid=OTHER_ANCHOR,
            correction_origin=None,
            correction_file=None,
            correction_old=None,
            correction_new=None,
            correction_commit=None,
            overlap=0.0,
            detail={},
        ),
        Correction(
            ts_ms=3_000,
            session_id=SESSION,
            source="cc-review",
            anchor_uuid=EventUuid("review:r1:7"),
            incorrect_digest=None,
            incorrect_file="/a.py",
            incorrect_old="",
            incorrect_new="pip install requests",
            correction_origin="review",
            correction_text="use uv add — not pip install",
            detail={"repo": "github.com/yasyf/café", "naïve": True},
        ),
        correction(
            ts_ms=4_000,
            session_id=OTHER_SESSION,
            incorrect_digest=DIGEST_C,
            detail={"repo": "r-b", "score": 0.5, "nested": {"a": [1, 2, 3], "b": None}},
        ),
        correction(
            ts_ms=5_000,
            incorrect_digest=DIGEST_A,
            anchor_uuid=EventUuid("anchor-3"),
            correction_origin="git",
            correction_commit="deadbeef" * 5,
            detail={"repo": "r-a"},
        ),
    ]


async def open_log(path: pathlib.Path) -> _native.RustCorrectionLog:
    log = _native.RustCorrectionLog(str(path))
    await log.open()
    return log


async def open_python_log(path: pathlib.Path) -> PyReferenceLog:
    initializer = await open_log(path)
    await initializer.close()
    return PyReferenceLog.open(path)


async def rust_append(rust: _native.RustCorrectionLog, row: Correction) -> None:
    # Pass the detail object as-is; the engine mirrors CorrectionLog.append's
    # json.dumps(dict(record.detail)), so the stored detail_json matches byte-for-byte.
    await rust.append(
        row.ts_ms,
        row.session_id,
        row.source,
        row.anchor_uuid,
        row.incorrect_digest,
        row.incorrect_file,
        row.incorrect_old,
        row.incorrect_new,
        row.correction_origin,
        row.correction_file,
        row.correction_old,
        row.correction_new,
        row.correction_commit,
        row.correction_text,
        row.overlap,
        row.detail,
    )


def read_conn(path: pathlib.Path) -> sqlite3.Connection:
    conn = sqlite3.connect(path)
    conn.row_factory = sqlite3.Row
    return conn


def journal_mode(path: pathlib.Path) -> str:
    return read_conn(path).execute("PRAGMA journal_mode").fetchone()[0]


def schema_dump(path: pathlib.Path) -> list[tuple[str, str, str | None]]:
    rows = read_conn(path).execute("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
    return [tuple(row) for row in rows]


def raw_rows(path: pathlib.Path) -> list[dict[str, object]]:
    return [dict(row) for row in read_conn(path).execute("SELECT * FROM corrections ORDER BY id")]


@requires_rust
async def test_rust_reads_python_written_rows(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "corrections.db"
    py_log = await open_python_log(db)
    for row in fixture_rows():
        py_log.append(row)
    rust = await open_log(db)

    assert (await rust.for_session(SESSION)) == [asdict(c) for c in py_log.for_session(SESSION)]
    assert (await rust.for_session(OTHER_SESSION)) == [asdict(c) for c in py_log.for_session(OTHER_SESSION)]
    assert (await rust.by_digest(SESSION, DIGEST_A)) == [
        asdict(c) for c in py_log.by_digest(SESSION, incorrect_digest=DIGEST_A)
    ]
    assert (await rust.for_repo("r-a")) == [asdict(c) for c in py_log.for_repo("r-a")]
    assert (await rust.for_repo("r-b")) == [asdict(c) for c in py_log.for_repo("r-b")]
    assert (await rust.since(1_000)) == [asdict(c) for c in py_log.since(1_000)]
    assert (await rust.since(0, "cc-review")) == [asdict(c) for c in py_log.since(0, source="cc-review")]
    assert (await rust.for_anchor(SESSION, ANCHOR)) == [asdict(c) for c in py_log.for_anchor(SESSION, ANCHOR)]


@requires_rust
async def test_on_disk_bytes_match_between_engines(tmp_path: pathlib.Path) -> None:
    rows = fixture_rows()
    db_py = tmp_path / "py.db"
    db_rust = tmp_path / "rust.db"
    py_log = await open_python_log(db_py)
    for row in rows:
        py_log.append(row)
    rust = await open_log(db_rust)
    for row in rows:
        await rust_append(rust, row)

    assert journal_mode(db_py) == "wal" == journal_mode(db_rust)
    assert schema_dump(db_py) == schema_dump(db_rust)
    assert raw_rows(db_py) == raw_rows(db_rust)

    stored = {row["ts_ms"]: row["detail_json"] for row in raw_rows(db_rust)}
    for row in rows:
        assert stored[row.ts_ms] == json.dumps(dict(row.detail))


@requires_rust
async def test_python_reads_rust_written_rows(tmp_path: pathlib.Path) -> None:
    rows = fixture_rows()
    db = tmp_path / "rust.db"
    rust = await open_log(db)
    for row in rows:
        await rust_append(rust, row)
    await rust.close()

    py_log = await open_python_log(db)
    assert py_log.for_session(SESSION) == tuple(row for row in rows if row.session_id == SESSION)
    assert py_log.for_session(OTHER_SESSION) == tuple(row for row in rows if row.session_id == OTHER_SESSION)


@requires_rust
async def test_sql_passthrough_and_busy_timeout_parity(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "corrections.db"
    py_log = await open_python_log(db)
    for row in fixture_rows():
        py_log.append(row)
    rust = await open_log(db)

    for statement in (
        "SELECT COUNT(*) AS n FROM corrections",
        "SELECT id, ts_ms, source, overlap, detail_json FROM corrections ORDER BY id",
        "SELECT session_id, incorrect_digest FROM corrections WHERE incorrect_digest IS NULL",
        "SELECT 1 AS x, 2 AS x",  # exact-duplicate column: dict(Row) collapses to the first
        "SELECT 1 AS x, 2 AS X",  # case-differing duplicate: both keys map to the first column
        "PRAGMA busy_timeout",
    ):
        assert (await rust.sql(statement)) == [dict(row) for row in py_log.conn.execute(statement).fetchall()], statement


@requires_rust
async def test_sql_unique_violation_raises_integrity_error_with_extended_name(tmp_path: pathlib.Path) -> None:
    rust = await open_log(tmp_path / "c.db")
    insert = (
        "INSERT INTO corrections (ts_ms, session_id, source, anchor_uuid, incorrect_file, "
        "incorrect_old, incorrect_new, incorrect_digest) VALUES (1, 's', 'x', 'a', '/f', '', '', 'd')"
    )
    await rust.sql(insert)
    with pytest.raises(sqlite3.IntegrityError) as info:
        await rust.sql(insert)  # same UNIQUE (session_id, anchor_uuid, incorrect_digest)
    assert info.value.sqlite_errorcode == 2067
    assert info.value.sqlite_errorname == "SQLITE_CONSTRAINT_UNIQUE"


@requires_rust
async def test_sql_enforces_the_single_statement_rule(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "corrections.db"
    py_log = await open_python_log(db)
    for row in fixture_rows():
        py_log.append(row)
    rust = await open_log(db)

    # A trailing statement (even a bare ";") raises before the head executes.
    before = len(await rust.for_session(SESSION))
    for multi in ("DELETE FROM corrections; DELETE FROM corrections", "SELECT 1; ;", "SELECT 1; SELECT 2"):
        with pytest.raises(sqlite3.ProgrammingError):
            await rust.sql(multi)
    assert len(await rust.for_session(SESSION)) == before  # nothing deleted
    # Comment/whitespace-only SQL yields no rows; a leading ";" is skipped, so "; SELECT 1" runs.
    for empty in ("", "   ", "-- just a comment", "/* block */", ";"):
        assert (await rust.sql(empty)) == []
    assert (await rust.sql("; SELECT 1 AS n")) == [{"n": 1}]


@requires_rust
async def test_open_bad_and_empty_paths_raise_operational_error() -> None:
    for path in ("/", ""):
        log = _native.RustCorrectionLog(path)
        with pytest.raises(sqlite3.OperationalError) as info:
            await log.open()
        assert info.value.sqlite_errorcode == 14
        assert info.value.sqlite_errorname == "SQLITE_CANTOPEN"


@requires_rust
async def test_non_database_file_raises_database_error(tmp_path: pathlib.Path) -> None:
    notdb = tmp_path / "notdb.db"
    notdb.write_bytes(b"this is not an sqlite database file, padding padding padding")
    log = _native.RustCorrectionLog(str(notdb))
    with pytest.raises(sqlite3.DatabaseError) as info:
        await log.open()
    assert info.value.sqlite_errorcode == 26
    assert info.value.sqlite_errorname == "SQLITE_NOTADB"


@requires_rust
async def test_invalid_utf8_text_raises_operational_error(tmp_path: pathlib.Path) -> None:
    rust = await open_log(tmp_path / "c.db")
    with pytest.raises(sqlite3.OperationalError) as info:
        await rust.sql("SELECT CAST(X'80' AS TEXT)")
    assert "Could not decode to UTF-8" in str(info.value)
    # A decode failure is a Python-side error, not a SQLite result code.
    assert getattr(info.value, "sqlite_errorcode", None) is None


@requires_rust
async def test_null_byte_in_sql_raises_programming_error(tmp_path: pathlib.Path) -> None:
    rust = await open_log(tmp_path / "c.db")
    with pytest.raises(sqlite3.ProgrammingError):
        await rust.sql("SELECT 1\x00; DROP TABLE corrections")


@requires_rust
async def test_os_error_carries_errno_and_filename(tmp_path: pathlib.Path) -> None:
    a_file = tmp_path / "afile"
    a_file.write_text("x")
    target = a_file / "sub" / "c.db"  # a parent component is a file, so mkdir fails ENOTDIR
    log = _native.RustCorrectionLog(str(target))
    with pytest.raises(NotADirectoryError) as info:
        await log.open()
    assert info.value.errno == 20
    assert str(info.value) == f"[Errno 20] Not a directory: '{a_file / 'sub'}'"


@requires_rust
async def test_concurrent_appends_resolve_in_submission_order(tmp_path: pathlib.Path) -> None:
    # The actor-model successor to the old GIL-convoy pin: many futures submitted at once
    # all resolve, applied in submission order (a distinct anchor_uuid per INSERT).
    rust = await open_log(tmp_path / "c.db")
    count = 50
    futures = [
        rust.append(i, "s", "x", str(i), None, "/f", "", "", None, None, None, None, None, None, 0.0, {})
        for i in range(count)
    ]
    await asyncio.gather(*futures)
    rows = await rust.sql("SELECT ts_ms FROM corrections ORDER BY id")
    assert [row["ts_ms"] for row in rows] == list(range(count))
    await rust.close()


@requires_rust
async def test_second_writer_times_out_against_a_held_write_lock(tmp_path: pathlib.Path) -> None:
    # Two handles of one SQLite library coordinate POSIX locks: the writer's append blocks the
    # busy_timeout against the holder's BEGIN IMMEDIATE, then raises "database is locked".
    db = tmp_path / "lock.db"
    holder = await open_log(db)
    writer = await open_log(db)
    await holder.sql("BEGIN IMMEDIATE")  # hold the write lock for the whole test
    with pytest.raises(sqlite3.OperationalError) as info:
        await writer.append(1, "s", "x", "a", None, "/f", "", "", None, None, None, None, None, None, 0.0, {})
    assert "locked" in str(info.value).lower()
    assert info.value.sqlite_errorcode == 5  # SQLITE_BUSY

    await holder.sql("ROLLBACK")
    await holder.close()
    await writer.close()


@requires_rust
async def test_nonfinite_and_lone_surrogate_detail(tmp_path: pathlib.Path) -> None:
    surrogate = {"k": chr(0xD800)}
    nonfinite = {"n": float("nan"), "i": float("inf"), "ni": float("-inf")}
    rows = [
        correction(ts_ms=1_000, incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR, detail=surrogate),
        correction(ts_ms=2_000, incorrect_digest=DIGEST_B, anchor_uuid=OTHER_ANCHOR, detail=nonfinite),
    ]
    db_py = tmp_path / "py.db"
    db_rust = tmp_path / "rust.db"
    py_log = await open_python_log(db_py)
    for row in rows:
        py_log.append(row)
    rust = await open_log(db_rust)
    for row in rows:
        await rust_append(rust, row)

    assert raw_rows(db_py) == raw_rows(db_rust)
    stored = {row["ts_ms"]: row["detail_json"] for row in raw_rows(db_rust)}
    assert stored[1_000] == json.dumps(surrogate)
    assert stored[2_000] == json.dumps(nonfinite)

    assert (await rust.for_session(SESSION))[0]["detail"] == surrogate  # lone surrogate round-trips
    got = (await rust.for_session(SESSION))[1]["detail"]
    assert math.isnan(got["n"]) and got["i"] == float("inf") and got["ni"] == float("-inf")


@requires_rust
async def test_non_mapping_detail_raises_like_dict(tmp_path: pathlib.Path) -> None:
    rust = await open_log(tmp_path / "c.db")
    args = (1_000, "s", "src", "a", None, "/f", "", "", None, None, None, None, None, None, 0.0)
    for bad in (None, [1, 2], "ab"):
        with pytest.raises((TypeError, ValueError)):
            rust.append(*args, bad)  # dict(detail) raises synchronously, before any future
    await rust.append(*args, [])  # dict([]) == {}: an empty list normalizes to the empty object
    assert (await rust.for_session("s"))[0]["detail"] == {}


@requires_rust
async def test_out_of_range_ts_ms_reads_back_by_storage_class(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "c.db"
    py_log = await open_python_log(db)
    rust = await open_log(db)
    # 2^63 overflows SQLite's signed-64-bit INTEGER, landing in ts_ms as REAL; Python's
    # row_to_record returns a float there, so the Rust projection must too (never an i64 error).
    await rust.sql(
        "INSERT INTO corrections (ts_ms, session_id, source, anchor_uuid, incorrect_file, "
        "incorrect_old, incorrect_new) VALUES (9223372036854775808, 's', 'x', 'a', '/f', '', '')"
    )
    got = await rust.for_session("s")
    assert got == [asdict(c) for c in py_log.for_session(SessionId("s"))]
    assert isinstance(got[0]["ts_ms"], float)


@requires_rust
async def test_insert_or_ignore_is_idempotent_across_engines(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "corrections.db"
    row = correction(ts_ms=1_000, incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR)
    rust = await open_log(db)
    await rust_append(rust, row)
    await rust_append(rust, row)
    assert len(await rust.for_session(SESSION)) == 1

    (await open_python_log(db)).append(row)
    assert len(await rust.for_session(SESSION)) == 1
