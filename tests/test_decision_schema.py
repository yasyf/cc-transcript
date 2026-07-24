from __future__ import annotations

import logging
import sqlite3
from concurrent.futures import ThreadPoolExecutor
from threading import Barrier
from typing import TYPE_CHECKING

import pytest

from cc_transcript.decision_store import (
    EXPECTED_DDL_FINGERPRINT,
    EXPECTED_OBJECT_FINGERPRINT,
    SCHEMA_COMPONENT,
    SCHEMA_VERSION,
    _archive_incompatible_database,
    ddl_fingerprint,
    object_fingerprint,
    open_decisions_sqlite,
)
from cc_transcript.decisions import DecisionLog
from cc_transcript.heartbeats import HeartbeatLog

if TYPE_CHECKING:
    from pathlib import Path

pytestmark = pytest.mark.anyio


def snapshot(path: Path) -> tuple[int, tuple[tuple[object, ...], ...], tuple[object, ...]]:
    with sqlite3.connect(path) as conn:
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        objects = tuple(conn.execute("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name"))
        try:
            marker = conn.execute(
                "SELECT id, component, schema_version, ddl_fingerprint, object_fingerprint "
                "FROM cc_review_decisions_schema_v1"
            ).fetchone()
        except sqlite3.OperationalError as error:
            marker = (str(error),)
    return version, objects, marker or ()


def mutate(path: Path, statement: str) -> None:
    with sqlite3.connect(path) as conn:
        conn.execute(statement)


async def create_exact(path: Path) -> None:
    await (await DecisionLog.open(path)).close()


async def test_both_openers_create_and_reopen_exact_v1(tmp_path: Path) -> None:
    path = tmp_path / "decisions.db"
    await (await HeartbeatLog.open(path)).close()
    await (await DecisionLog.open(path)).close()
    with sqlite3.connect(path) as conn:
        marker = conn.execute(
            "SELECT component, schema_version, ddl_fingerprint, object_fingerprint "
            "FROM cc_review_decisions_schema_v1 WHERE id=1"
        ).fetchone()
        assert conn.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
        assert marker == (SCHEMA_COMPONENT, SCHEMA_VERSION, EXPECTED_DDL_FINGERPRINT, EXPECTED_OBJECT_FINGERPRINT)
        assert object_fingerprint(conn) == EXPECTED_OBJECT_FINGERPRINT
    assert ddl_fingerprint() == EXPECTED_DDL_FINGERPRINT


async def test_existing_empty_database_is_the_only_initializable_shape(tmp_path: Path) -> None:
    path = tmp_path / "decisions.db"
    path.touch(mode=0o600)
    await (await DecisionLog.open(path)).close()
    assert snapshot(path)[0] == SCHEMA_VERSION


@pytest.mark.parametrize(
    ("initial", "mutation", "version"),
    [
        ("raw", "CREATE TABLE decisions(id INTEGER PRIMARY KEY)", 0),
        ("raw", "CREATE TABLE cc_review_decisions_schema_v1(id INTEGER PRIMARY KEY)", 0),
        ("raw", "PRAGMA user_version = 77", 77),
        ("exact", "PRAGMA user_version = 2", 2),
    ],
    ids=("old", "partial", "nonzero-empty", "foreign-version"),
)
async def test_version_mismatches_are_archived_and_replaced(
    tmp_path: Path,
    caplog: pytest.LogCaptureFixture,
    initial: str,
    mutation: str,
    version: int,
) -> None:
    path = tmp_path / "decisions.db"
    if initial == "exact":
        await create_exact(path)
    mutate(path, mutation)
    before = snapshot(path)

    with caplog.at_level(logging.WARNING, logger="cc_transcript.decision_store"):
        await (await DecisionLog.open(path)).close()

    archive, = tmp_path.glob(f"decisions.db.v{version}.*.bak")
    assert snapshot(archive) == before
    with sqlite3.connect(path) as conn:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
        assert object_fingerprint(conn) == EXPECTED_OBJECT_FINGERPRINT
    warnings = [record for record in caplog.records if record.name == "cc_transcript.decision_store"]
    assert len(warnings) == 1
    message = warnings[0].getMessage()
    assert str(path) in message
    assert f"schema version {version}" in message
    assert f"expected {SCHEMA_VERSION}" in message
    assert str(archive) in message


async def test_concurrent_openers_archive_once_and_both_observe_exact_v1(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    path = tmp_path / "decisions.db"
    with sqlite3.connect(path) as conn:
        conn.execute("CREATE TABLE legacy(value TEXT)")
        conn.execute("INSERT INTO legacy VALUES ('preserved')")
        conn.execute("PRAGMA user_version = 77")
    barrier = Barrier(2)

    def open_after_barrier() -> tuple[int, str]:
        barrier.wait(timeout=5)
        conn = open_decisions_sqlite(path)
        try:
            return conn.execute("PRAGMA user_version").fetchone()[0], object_fingerprint(conn)
        finally:
            conn.close()

    with caplog.at_level(logging.WARNING, logger="cc_transcript.decision_store"):
        with ThreadPoolExecutor(max_workers=2) as executor:
            futures = tuple(executor.submit(open_after_barrier) for _ in range(2))
            results = tuple(future.result(timeout=5) for future in futures)

    assert results == ((SCHEMA_VERSION, EXPECTED_OBJECT_FINGERPRINT),) * 2
    archive, = tmp_path.glob("decisions.db.v77.*.bak")
    with sqlite3.connect(archive) as conn:
        assert conn.execute("SELECT value FROM legacy").fetchone()[0] == "preserved"
    assert snapshot(path)[0] == SCHEMA_VERSION
    warnings = [record for record in caplog.records if record.name == "cc_transcript.decision_store"]
    assert len(warnings) == 1


async def test_archive_checkpoints_wal_into_main_database(tmp_path: Path) -> None:
    path = tmp_path / "decisions.db"
    writer = sqlite3.connect(path, autocommit=True)
    try:
        assert writer.execute("PRAGMA journal_mode = WAL").fetchone()[0] == "wal"
        writer.execute("PRAGMA wal_autocheckpoint = 0")
        writer.execute("CREATE TABLE legacy(value TEXT)")
        writer.execute("PRAGMA user_version = 77")
        writer.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        writer.execute("INSERT INTO legacy VALUES ('preserved')")
        assert path.with_name(f"{path.name}-wal").exists()
        with sqlite3.connect(f"{path.as_uri()}?mode=ro&immutable=1", uri=True) as main:
            assert main.execute("SELECT count(*) FROM legacy").fetchone()[0] == 0

        await (await DecisionLog.open(path)).close()

        archive, = tmp_path.glob("decisions.db.v77.*.bak")
        with sqlite3.connect(f"{archive.as_uri()}?mode=ro&immutable=1", uri=True) as main:
            assert main.execute("PRAGMA user_version").fetchone()[0] == 77
            assert main.execute("SELECT value FROM legacy").fetchone()[0] == "preserved"
        for suffix in ("-wal", "-shm"):
            assert not archive.with_name(f"{archive.name}{suffix}").exists()
    finally:
        writer.close()


def test_same_second_archives_are_unique(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / "decisions.db"
    monkeypatch.setattr(
        "cc_transcript.decision_store.time.time_ns",
        lambda: 1_700_000_000_123_456_000,
    )

    for value in ("first", "second"):
        with sqlite3.connect(path) as conn:
            conn.execute("CREATE TABLE legacy(value TEXT)")
            conn.execute("INSERT INTO legacy VALUES (?)", (value,))
            conn.execute("PRAGMA user_version = 77")
        _archive_incompatible_database(path, 77)

    archives = tuple(sorted(tmp_path.glob("decisions.db.v77.*.bak")))
    assert len(archives) == 2
    preserved = set()
    for archive in archives:
        with sqlite3.connect(archive) as conn:
            preserved.add(conn.execute("SELECT value FROM legacy").fetchone()[0])
    assert preserved == {"first", "second"}


async def test_matching_version_preserves_data(tmp_path: Path) -> None:
    path = tmp_path / "decisions.db"
    await create_exact(path)
    with sqlite3.connect(path) as conn:
        conn.execute(
            "INSERT INTO decisions "
            "(ts_ms, session_id, source, kind, source_file, event, action, detail_json) "
            "VALUES (1, 'session', 'test', 'match', 'test.py', 'Stop', 'note', '{}')"
        )
    before = snapshot(path)

    await (await DecisionLog.open(path)).close()

    assert snapshot(path) == before
    with sqlite3.connect(path) as conn:
        assert conn.execute("SELECT session_id FROM decisions").fetchone()[0] == "session"


@pytest.mark.parametrize(
    ("initial", "mutation", "error"),
    [
        ("exact", "DROP INDEX idx_decisions_tool_digest", "object fingerprint"),
        ("exact", "CREATE TABLE foreign_state(id TEXT PRIMARY KEY)", "object fingerprint"),
        (
            "exact",
            "UPDATE cc_review_decisions_schema_v1 SET ddl_fingerprint='foreign' WHERE id=1",
            "DDL fingerprint",
        ),
    ],
    ids=("missing", "extra", "foreign-fingerprint"),
)
async def test_nonexact_shapes_are_rejected_without_mutation(
    tmp_path: Path, initial: str, mutation: str, error: str
) -> None:
    path = tmp_path / "decisions.db"
    if initial == "exact":
        await create_exact(path)
    mutate(path, mutation)
    before = snapshot(path)
    with pytest.raises(RuntimeError, match=error):
        await DecisionLog.open(path)
    assert snapshot(path) == before


def test_open_connection_cannot_mutate_exact_schema_or_attestation(tmp_path: Path) -> None:
    path = tmp_path / "decisions.db"
    conn = open_decisions_sqlite(path)
    before = snapshot(path)
    statements = (
        "CREATE TABLE probe(id INTEGER)",
        "DROP INDEX idx_decisions_tool_digest",
        "ALTER TABLE decisions ADD COLUMN probe TEXT",
        "UPDATE cc_review_decisions_schema_v1 SET ddl_fingerprint = printf('%064d', 0) WHERE id = 1",
        "DELETE FROM cc_review_decisions_schema_v1 WHERE id = 1",
        "PRAGMA user_version = 2",
        "PRAGMA writable_schema = ON",
        "UPDATE sqlite_schema SET sql = sql WHERE name = 'decisions'",
        f"ATTACH DATABASE '{path}' AS samefile",
    )
    try:
        for statement in statements:
            with pytest.raises(sqlite3.DatabaseError):
                conn.execute(statement)
        conn.execute("CREATE TEMP TABLE allowed(id INTEGER)")
        conn.execute("INSERT INTO allowed(id) VALUES (1)")
        assert conn.execute("SELECT id FROM allowed").fetchone()[0] == 1
    finally:
        conn.close()
    assert snapshot(path) == before
