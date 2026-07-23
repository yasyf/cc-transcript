from __future__ import annotations

import sqlite3
from typing import TYPE_CHECKING

import pytest

from cc_transcript.decision_store import (
    EXPECTED_DDL_FINGERPRINT,
    EXPECTED_OBJECT_FINGERPRINT,
    SCHEMA_COMPONENT,
    SCHEMA_VERSION,
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
    ("initial", "mutation", "error"),
    [
        ("raw", "CREATE TABLE decisions(id INTEGER PRIMARY KEY)", "schema version"),
        ("raw", "CREATE TABLE cc_review_decisions_schema_v1(id INTEGER PRIMARY KEY)", "schema version"),
        ("raw", "PRAGMA user_version = 77", "schema version"),
        ("exact", "DROP INDEX idx_decisions_tool_digest", "object fingerprint"),
        ("exact", "CREATE TABLE foreign_state(id TEXT PRIMARY KEY)", "object fingerprint"),
        (
            "exact",
            "UPDATE cc_review_decisions_schema_v1 SET ddl_fingerprint='foreign' WHERE id=1",
            "DDL fingerprint",
        ),
        ("exact", "PRAGMA user_version = 2", "schema version"),
    ],
    ids=("old", "partial", "nonzero-empty", "missing", "extra", "foreign-fingerprint", "foreign-version"),
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
