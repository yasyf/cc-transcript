"""Store-tier contract fixtures: the one module the native-store flip re-points.

Every construction of the three store configurations — the platform default, a
cc-steer-shaped subclass, and a captain-hook-shaped subclass — lives here, built
against today's synchronous subclassing API (:class:`VerdictStoreMixin` over
:class:`FeedbackStore` over :class:`FileStateStore`). ``test_store_contract.py``
talks to a store only through the helpers this module exports — never
``store.store.conn`` directly — so the later swap (Python impl → native-engine
facade) edits this module alone and every assertion in the suite carries over
verbatim.

The two downstream shapes are replicated locally rather than imported: cc-steer is
on the v13-era async ``aiosqlite`` API, so its schema (``origin_path`` /
``quarantined_reason`` columns, the ``triage`` verdict naming, the triage/refine/
gate views) is mirrored here against the sync API; captain-hook is already sync but
its store pulls in the whole review package, so its six review tables and its
guarded-ALTER migration runner are mirrored here too. Both mirrors reproduce the
schema and observable behaviour the downstream subclass produces, not its code.
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass, field
from datetime import datetime
from importlib.util import find_spec
from typing import TYPE_CHECKING, ClassVar, Self

import pytest

from cc_transcript.context import ContextWindow, TurnRef
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.judge import similar
from cc_transcript.judge.verdicts import EVENT_COLUMNS, VerdictStoreMixin, hydratable
from cc_transcript.mining.candidates import DedupKey, FeedbackCandidate
from cc_transcript.mining.confidence import CandidateSignal, Confidence
from cc_transcript.mining.sourcekind import SourceKind
from cc_transcript.mining.store import FEEDBACK_DDL as BASE_FEEDBACK_DDL
from cc_transcript.mining.store import FeedbackStore, event_row, now
from cc_transcript.store import FileStateStore, TransactionConflictError

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping, Sequence
    from pathlib import Path

# TransactionConflictError moves to mining.store at the flip; the suite imports it from here.
__all__ = [
    "CONFIG_NAMES",
    "FIXED_NOW",
    "StoreClock",
    "TransactionConflictError",
    "candidate",
    "committed_fixture_state",
    "count",
    "event_rows",
    "evidence_rows",
    "execute",
    "fake_embed",
    "open_config",
    "query",
    "raw_schema_dump",
    "record_file",
    "reject_evidence_metadata_writes",
    "requires_judge",
    "schema_dump",
    "store_clock",
    "store_transaction",
    "verdict_rows",
]

FIXED_NOW = "2026-01-01T00:00:00+00:00"
SESSION = SessionId("sess-0001")
SOURCE_KIND = SourceKind("transcript_message")

JUDGE_DEPS = ("sqlite_vec", "model2vec", "numpy")
JUDGE_DEPS_PRESENT = all(find_spec(name) is not None for name in JUDGE_DEPS)

SCHEMA_QUERY = (
    "SELECT type, name, tbl_name, sql FROM sqlite_master "
    "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name, tbl_name"
)

STEER_FEEDBACK_DDL = BASE_FEEDBACK_DDL.replace(
    "  ingested_at TEXT NOT NULL\n",
    "  ingested_at TEXT NOT NULL,\n  origin_path TEXT,\n  quarantined_reason TEXT\n",
)
STEER_ACCRUED_EMPTY_REASON = "accrued_context_empty"
STEER_REBUILD_QUARANTINE_REASONS = (
    STEER_ACCRUED_EMPTY_REASON,
    "transcript_not_found",
    "transcript_parse_failed",
    "anchor_not_found",
    "rebuilt_context_empty",
)
STEER_QUARANTINE_ELIGIBLE = (
    "(quarantined_reason IS NULL OR quarantined_reason IN ("
    + ", ".join(f"'{reason}'" for reason in STEER_REBUILD_QUARANTINE_REASONS)
    + "))"
)
STEER_INSERT_EVENT = """
INSERT OR IGNORE INTO feedback_events (
  dedup_key, source_kind, session_id, event_uuid,
  occurred_at, text, payload_json, context_json, cc_version, ingested_at, origin_path
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""
STEER_QUARANTINE_CONTEXT = f"""
UPDATE feedback_events SET quarantined_reason = ?
WHERE dedup_key = ? AND {STEER_QUARANTINE_ELIGIBLE}
"""
STEER_TRIAGE_VIEWS_DDL = """DROP VIEW IF EXISTS training_pairs;
DROP VIEW IF EXISTS latest_judge;
CREATE VIEW latest_judge AS
SELECT * FROM (
  SELECT t.*, ROW_NUMBER() OVER (
    PARTITION BY t.dedup_key ORDER BY t.prompt_version DESC, t.judged_at DESC, t.id DESC
  ) AS rn
  FROM triage t
  WHERE t.role = 'judge'
) WHERE rn = 1;
DROP VIEW IF EXISTS latest_auditor;
CREATE VIEW latest_auditor AS
SELECT * FROM (
  SELECT t.*, ROW_NUMBER() OVER (
    PARTITION BY t.dedup_key ORDER BY t.prompt_version DESC, t.judged_at DESC, t.id DESC
  ) AS rn
  FROM triage t
  WHERE t.role = 'auditor'
) WHERE rn = 1;
DROP VIEW IF EXISTS accepted_steering;
CREATE VIEW accepted_steering AS
SELECT
  e.id AS event_id,
  e.dedup_key,
  e.source_kind,
  e.text,
  e.context_json,
  e.payload_json,
  t.category,
  t.what_claude_did,
  e.origin_path
FROM feedback_events e
JOIN latest_judge t ON t.dedup_key = e.dedup_key
WHERE t.is_steering = 1 AND e.quarantined_reason IS NULL;
"""
STEER_REFINE_DDL = """
CREATE TABLE IF NOT EXISTS refinement (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  pair_index INTEGER NOT NULL,
  action TEXT NOT NULL,
  direction_verbatim TEXT NOT NULL,
  direction TEXT NOT NULL,
  refined_at TEXT NOT NULL,
  UNIQUE(dedup_key, prompt_version, model, pair_index)
);
CREATE INDEX IF NOT EXISTS idx_refinement_dedup ON refinement(dedup_key);
DROP VIEW IF EXISTS latest_refinement;
CREATE VIEW latest_refinement AS
WITH gens AS (
  SELECT dedup_key, prompt_version, model, refined_at,
    ROW_NUMBER() OVER (
      PARTITION BY dedup_key ORDER BY prompt_version DESC, refined_at DESC
    ) AS g
  FROM (SELECT DISTINCT dedup_key, prompt_version, model, refined_at FROM refinement)
)
SELECT r.*
FROM refinement r
JOIN gens ON gens.dedup_key = r.dedup_key AND gens.prompt_version = r.prompt_version
         AND gens.model = r.model AND gens.refined_at = r.refined_at AND gens.g = 1;
DROP VIEW IF EXISTS refined_pairs;
CREATE VIEW refined_pairs AS
SELECT
  e.id AS event_id,
  r.dedup_key,
  r.pair_index,
  r.action,
  r.direction_verbatim,
  r.direction,
  e.text AS original_message,
  ap.category,
  e.source_kind,
  e.session_id,
  e.event_uuid,
  e.occurred_at,
  e.origin_path,
  r.prompt_version,
  r.model
FROM latest_refinement r
JOIN feedback_events e ON e.dedup_key = r.dedup_key
JOIN accepted_steering ap ON ap.dedup_key = r.dedup_key
ORDER BY e.id, r.pair_index;
"""
STEER_GATE_DDL = """
CREATE TABLE IF NOT EXISTS gate_sample (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sample_key TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  dedup_key TEXT,
  session_id TEXT NOT NULL,
  anchor_uuid TEXT NOT NULL,
  occurred_at TEXT,
  offset_turns INTEGER NOT NULL DEFAULT 0,
  window_json TEXT NOT NULL,
  seed INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_gate_sample_kind ON gate_sample(kind);
CREATE INDEX IF NOT EXISTS idx_gate_sample_session ON gate_sample(session_id);
CREATE TABLE IF NOT EXISTS sampled_session (
  session_id TEXT PRIMARY KEY,
  sampled_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS exemplar_embedding (
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  model TEXT NOT NULL,
  text_digest TEXT NOT NULL,
  dim INTEGER NOT NULL,
  vector BLOB NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(dedup_key, model)
);
"""
HOOK_REVIEW_DDL = """
CREATE TABLE IF NOT EXISTS candidates (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_key TEXT NOT NULL,
  candidate_kind TEXT NOT NULL CHECK (candidate_kind IN ('create', 'fix')),
  rule TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('watching', 'pr_open', 'stale', 'accepted', 'rejected')),
  pr_url TEXT,
  pr_opened_at TEXT,
  target_source_file TEXT,
  target_hook_name TEXT,
  misfire_class TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK (
    (candidate_kind = 'create' AND target_source_file IS NULL AND target_hook_name IS NULL
      AND misfire_class IS NULL)
    OR (candidate_kind = 'fix' AND target_source_file IS NOT NULL AND target_hook_name IS NOT NULL)
  )
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_candidates_create_key
  ON candidates(repo_key, rule) WHERE candidate_kind = 'create';
CREATE UNIQUE INDEX IF NOT EXISTS idx_candidates_fix_key
  ON candidates(repo_key, target_hook_name, target_source_file) WHERE candidate_kind = 'fix';
CREATE INDEX IF NOT EXISTS idx_candidates_repo_status ON candidates(repo_key, status);
CREATE TABLE IF NOT EXISTS candidate_observations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  candidate_id INTEGER NOT NULL REFERENCES candidates(id),
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  session_id TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  UNIQUE(candidate_id, dedup_key)
);
CREATE INDEX IF NOT EXISTS idx_observations_dedup ON candidate_observations(dedup_key);
CREATE TABLE IF NOT EXISTS repos (
  repo_key TEXT PRIMARY KEY,
  watching INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS spawn_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  transcript TEXT NOT NULL,
  ok INTEGER NOT NULL,
  error TEXT,
  report_json TEXT,
  CHECK ((ok = 1) = (error IS NULL))
);
CREATE TABLE IF NOT EXISTS review_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pr_states (
  pr_url TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  merged_at TEXT,
  fetched_at TEXT NOT NULL
);
"""

requires_judge = pytest.mark.skipif(
    not JUDGE_DEPS_PRESENT, reason="cc-transcript[judge] extra (sqlite-vec, model2vec, numpy) not installed"
)


def fake_embed(text: str):  # noqa: ANN201 — numpy only imported under the [judge] extra
    """A deterministic, dependency-light stand-in for the potion embedder."""
    import hashlib

    import numpy as np

    vector = np.zeros(similar.EMBED_DIM, dtype=np.float32)
    for token in text.lower().split():
        vector[int(hashlib.md5(token.encode()).hexdigest(), 16) % similar.EMBED_DIM] += 1.0
    return vector / (norm if (norm := np.linalg.norm(vector)) else 1.0)


@pytest.fixture
def fake_embedder(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(similar, "default_embedder", lambda: fake_embed)


@dataclass(slots=True)
class StoreClock:
    value: str = FIXED_NOW

    def __call__(self) -> str:
        return self.value


@pytest.fixture
def store_clock(monkeypatch: pytest.MonkeyPatch) -> StoreClock:
    clock = StoreClock()
    monkeypatch.setattr("cc_transcript.mining.store.now", clock)
    monkeypatch.setattr("cc_transcript.judge.verdicts.now", clock)
    monkeypatch.setattr(__name__ + ".now", clock)
    return clock


def candidate(
    dedup_key: str,
    *,
    text: str = "always use uv not pip",
    empty_window: bool = False,
    occurred_at: str = FIXED_NOW,
    payload: Mapping[str, object] | None = None,
) -> FeedbackCandidate:
    """Builds one deterministic :class:`FeedbackCandidate`.

    ``empty_window`` produces a window with no ``before`` turns — the shape the
    cc-steer mirror quarantines, a faithful stand-in for its ``has_substantive_content``
    predicate (which lives in the cc-steer renderer and is not reproduced here).
    """
    anchor = EventRef(SESSION, EventUuid(f"evt-{dedup_key}"))
    window = ContextWindow(
        anchor=anchor,
        before=() if empty_window else (TurnRef(role="user", refs=(anchor,), preview=text, tool_digests=()),),
        trigger=TurnRef(role="user", refs=(anchor,), preview=text, tool_digests=()),
        after=(),
        fidelity="summary",
        preview_chars=200,
    )
    return FeedbackCandidate(
        dedup_key=DedupKey(dedup_key),
        source_kind=SOURCE_KIND,
        occurred_at=datetime.fromisoformat(occurred_at),
        text=text,
        window=window,
        ref=anchor,
        signal=CandidateSignal(confidence=Confidence(0.9), reasons=("stub",), durable=True),
        session_id=SESSION,
        cc_version="1.0.0",
        payload=payload,
    )


def query(store: object, sql: str, params: Sequence[object] = ()) -> list[dict[str, object]]:
    """Runs a read statement, returning rows as dicts (flip-safe)."""
    return [dict(row) for row in store.store.conn.execute(sql, tuple(params))]  # type: ignore[attr-defined]


def execute(store: object, sql: str, params: Sequence[object] = ()) -> None:
    """Runs one write statement, autocommitting or joining the open transaction (flip-safe)."""
    store.store.conn.execute(sql, tuple(params))  # type: ignore[attr-defined]


def count(store: object, table: str, where: str = "", params: Sequence[object] = ()) -> int:
    """Counts rows in ``table``, optionally filtered by a ``WHERE`` clause."""
    clause = f" WHERE {where}" if where else ""
    return query(store, f"SELECT COUNT(*) AS n FROM {table}{clause}", params)[0]["n"]  # type: ignore[return-value]


def store_transaction(store: object):  # noqa: ANN201 — context manager, type varies at the flip
    """The store's write transaction context (flip-safe)."""
    return store.store.transaction()  # type: ignore[attr-defined]


def record_file(store: object, path: str, mtime: float) -> None:
    """Upserts a scanned file's mtime through the store (flip-safe)."""
    store.store.record_file(path, mtime)  # type: ignore[attr-defined]


def event_rows(store: object, name: str) -> list[dict[str, object]]:
    """Reads every persisted event field in insertion order (flip-safe)."""
    extra = ", origin_path, quarantined_reason" if name == "steer" else ""
    return query(
        store,
        "SELECT id, dedup_key, source_kind, session_id, event_uuid, occurred_at, text, payload_json, "
        f"context_json, cc_version, ingested_at{extra} FROM feedback_events ORDER BY id",
    )


def evidence_rows(store: object) -> list[dict[str, object]]:
    """Reads persisted verdict evidence by verdict identity (flip-safe)."""
    return query(
        store,
        "SELECT dedup_key, role, prompt_version, canonical_key, evidence_text "
        "FROM verdict_evidence ORDER BY dedup_key, role, prompt_version",
    )


def reject_evidence_metadata_writes(store: object) -> None:
    """Makes the metadata insert fail after the real sqlite-vec insert runs."""
    similar.prepare_connection(store.store)  # type: ignore[attr-defined]
    execute(
        store,
        "CREATE TRIGGER reject_evidence_metadata BEFORE INSERT ON verdict_evidence "
        "BEGIN SELECT RAISE(ABORT, 'evidence metadata write failed'); END",
    )


def _format_schema(rows: Sequence[dict[str, object]]) -> str:
    return "".join(
        f"-- {row['type']} {row['name']} (on {row['tbl_name']})\n{(row['sql'] or '').strip()};\n\n" for row in rows
    )


def schema_dump(store: object) -> str:
    """The store's live schema as a deterministic dump, read through the store."""
    return _format_schema(query(store, SCHEMA_QUERY))


def raw_schema_dump(path: Path) -> str:
    """The on-disk schema, read with an independent sqlite3 connection (engine-agnostic)."""
    conn = sqlite3.connect(path)
    conn.row_factory = sqlite3.Row
    try:
        return _format_schema([dict(row) for row in conn.execute(SCHEMA_QUERY)])
    finally:
        conn.close()


class PlatformStore(VerdictStoreMixin, FeedbackStore):
    """Config (a): platform default — generic ``verdicts`` / ``accepted`` / ``summary`` naming."""

    @classmethod
    def open(cls, path: Path) -> Self:
        return cls(FileStateStore.open(path, extra_schema=BASE_FEEDBACK_DDL + cls.verdicts_ddl()))


class SteerStore(VerdictStoreMixin, FeedbackStore):
    """Config (b): cc-steer shape (sync) — ``triage`` naming, quarantine column + event filter."""

    VERDICT_TABLE: ClassVar[str] = "triage"
    ACCEPTED_COLUMN: ClassVar[str] = "is_steering"
    SUMMARY_COLUMN: ClassVar[str] = "what_claude_did"
    EVENT_FILTER: ClassVar[str] = "e.quarantined_reason IS NULL"

    @classmethod
    def open(cls, path: Path) -> Self:
        store = FileStateStore.open(
            path,
            extra_schema=STEER_FEEDBACK_DDL
            + cls.verdicts_ddl()
            + STEER_TRIAGE_VIEWS_DDL
            + STEER_REFINE_DDL
            + STEER_GATE_DDL,
        )
        return cls(store)

    def record_file_scan(self, path: str, mtime: float, candidates: Sequence[FeedbackCandidate]) -> int:
        ingested_at = now()
        with self.store.transaction() as conn:
            inserted: list[FeedbackCandidate] = []
            for cand in candidates:
                before = conn.total_changes
                conn.execute(STEER_INSERT_EVENT, (*event_row(cand, ingested_at), path))
                if conn.total_changes > before:
                    inserted.append(cand)
            conn.executemany(
                STEER_QUARANTINE_CONTEXT,
                [(STEER_ACCRUED_EMPTY_REASON, cand.dedup_key) for cand in inserted if not cand.window.before],
            )
            self.store.record_file(path, mtime)
            return len(inserted)

    def unjudged(
        self,
        *,
        role: str,
        prompt_version: int,
        limit: int | None = None,
        refresh_summary: bool = False,
        probe_hydration: bool = True,
    ) -> list[dict[str, object]]:
        self.ensure_verdict_schema()
        if not refresh_summary:
            sql = (
                f"SELECT {EVENT_COLUMNS} FROM feedback_events e "
                f"LEFT JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
                "AND t.role = ? AND t.prompt_version = ? "
                f"WHERE t.id IS NULL AND {self.EVENT_FILTER} ORDER BY e.id"
            )
            params: tuple[object, ...] = (role, prompt_version)
            if limit is not None:
                sql += " LIMIT ?"
                params = (*params, limit)
            return [dict(row) for row in self.store.conn.execute(sql, params)]
        if limit == 0:
            return []
        sql = (
            f"SELECT {EVENT_COLUMNS}, t.id AS verdict_id FROM feedback_events e "
            f"LEFT JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
            "AND t.role = ? AND t.prompt_version = ? "
            f"WHERE {self.EVENT_FILTER} AND (t.id IS NULL OR t.fidelity = 'summary') "
            "ORDER BY (t.id IS NOT NULL), e.id"
        )
        params = (role, prompt_version)
        kept: list[dict[str, object]] = []
        if limit is None:
            for raw in self.store.conn.execute(sql, params):
                row = dict(raw)
                fresh = row.pop("verdict_id") is None
                if not probe_hydration or fresh or hydratable(str(row["context_json"])):
                    kept.append(row)
            return kept
        offset = 0
        while len(kept) < limit:
            page = [dict(row) for row in self.store.conn.execute(sql + " LIMIT ? OFFSET ?", (*params, limit, offset))]
            if not page:
                break
            offset += len(page)
            for row in page:
                fresh = row.pop("verdict_id") is None
                if not probe_hydration or fresh or hydratable(str(row["context_json"])):
                    kept.append(row)
                    if len(kept) >= limit:
                        break
        return kept

    def judged(self, *, role: str, prompt_version: int) -> list[dict[str, object]]:
        self.ensure_verdict_schema()
        cur = self.store.conn.execute(
            f"SELECT {EVENT_COLUMNS}, t.category, t.{self.ACCEPTED_COLUMN} AS accepted, t.confidence, "
            f"t.{self.SUMMARY_COLUMN} AS summary, t.rationale, t.model "
            f"FROM feedback_events e JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
            f"WHERE t.role = ? AND t.prompt_version = ? AND {self.EVENT_FILTER} ORDER BY e.id",
            (role, prompt_version),
        )
        return [dict(row) for row in cur]


@dataclass(frozen=True, slots=True)
class ColumnMigration:
    column: str
    ddl: str
    backfill: str | None = None


HOOK_CANDIDATE_MIGRATIONS: tuple[ColumnMigration, ...] = (
    ColumnMigration("generation", "generation INTEGER NOT NULL DEFAULT 1"),
    ColumnMigration(
        "resolved_at", "resolved_at TEXT", "UPDATE candidates SET resolved_at = updated_at WHERE status = 'accepted'"
    ),
    ColumnMigration("origin_repo_key", "origin_repo_key TEXT"),
    ColumnMigration("pack_name", "pack_name TEXT"),
    ColumnMigration(
        "announced_status",
        "announced_status TEXT",
        "UPDATE candidates SET announced_status = status WHERE status NOT IN ('watching', 'pr_open')",
    ),
)
HOOK_FEEDBACK_MIGRATIONS: tuple[ColumnMigration, ...] = (ColumnMigration("triage", "triage TEXT"),)


class HookStore(VerdictStoreMixin, FeedbackStore):
    """Config (c): captain-hook shape — generic naming + review tables + guarded-ALTER migrations."""

    @classmethod
    def open(cls, path: Path, *, busy_timeout_ms: int | None = None) -> Self:
        store = cls(FileStateStore.open(path, extra_schema=BASE_FEEDBACK_DDL + cls.verdicts_ddl() + HOOK_REVIEW_DDL))
        if busy_timeout_ms is not None:
            store.store.conn.execute(f"PRAGMA busy_timeout = {busy_timeout_ms}")
        store.migrate_columns("candidates", HOOK_CANDIDATE_MIGRATIONS)
        store.migrate_columns("feedback_events", HOOK_FEEDBACK_MIGRATIONS)
        return store

    def migrate_columns(self, table: str, migrations: tuple[ColumnMigration, ...]) -> None:
        def pending(conn: sqlite3.Connection) -> list[ColumnMigration]:
            existing = {str(row["name"]) for row in conn.execute(f"PRAGMA table_info({table})")}
            return [migration for migration in migrations if migration.column not in existing]

        if not pending(self.store.conn):
            return
        with self.store.transaction() as conn:
            for migration in pending(conn):
                conn.execute(f"ALTER TABLE {table} ADD COLUMN {migration.ddl}")
                if migration.backfill is not None:
                    conn.execute(migration.backfill)


@dataclass(frozen=True, slots=True)
class StoreConfig:
    name: str
    open: Callable[[Path], object]
    verdict_table: str
    accepted_column: str
    summary_column: str
    extra_columns: tuple[str, ...] = ()
    event_filter: str | None = None
    fixture_db: str = field(init=False, default="")

    def __post_init__(self) -> None:
        object.__setattr__(self, "fixture_db", f"store_fixture_{self.name}.db")

    def schema_golden(self) -> str:
        return f"store_schema_{self.name}.sql"


CONFIGS: dict[str, StoreConfig] = {
    "platform": StoreConfig(
        name="platform",
        open=PlatformStore.open,
        verdict_table="verdicts",
        accepted_column="accepted",
        summary_column="summary",
    ),
    "steer": StoreConfig(
        name="steer",
        open=SteerStore.open,
        verdict_table="triage",
        accepted_column="is_steering",
        summary_column="what_claude_did",
        extra_columns=("origin_path", "quarantined_reason"),
        event_filter="e.quarantined_reason IS NULL",
    ),
    "hook": StoreConfig(
        name="hook",
        open=HookStore.open,
        verdict_table="verdicts",
        accepted_column="accepted",
        summary_column="summary",
    ),
}
CONFIG_NAMES: tuple[str, ...] = tuple(CONFIGS)


def verdict_rows(store: object, name: str) -> list[dict[str, object]]:
    """Reads every persisted verdict field under normalized column names."""
    config = CONFIGS[name]
    rows = query(
        store,
        f"SELECT id, dedup_key, role, prompt_version, model, category, "
        f"{config.accepted_column} AS accepted, {config.summary_column} AS summary, "
        f"confidence, rationale, canonical_key, fidelity, judged_at FROM {config.verdict_table} ORDER BY id",
    )
    for row in rows:
        row["accepted"] = bool(row["accepted"])
    return rows


def committed_fixture_state(store: object, name: str) -> dict[str, object]:
    """Reads a committed fixture through store APIs plus downstream seed projections."""
    state: dict[str, object] = {
        "events": store.events(),  # type: ignore[attr-defined]
        "unjudged": store.unjudged(  # type: ignore[attr-defined]
            role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False
        ),
        "judged": store.judged(role="judge", prompt_version=1),  # type: ignore[attr-defined]
        "file_mtimes": store.file_mtimes(),  # type: ignore[attr-defined]
    }
    if name == "steer":
        state["quarantine"] = query(
            store,
            "SELECT dedup_key, origin_path, quarantined_reason FROM feedback_events ORDER BY id",
        )
    if name == "hook":
        state["candidates"] = query(
            store,
            "SELECT id, repo_key, candidate_kind, rule, source_kind, status, created_at, updated_at, "
            "generation, resolved_at, origin_repo_key, pack_name, announced_status FROM candidates ORDER BY id",
        )
        state["observations"] = query(
            store,
            "SELECT id, candidate_id, dedup_key, session_id, occurred_at "
            "FROM candidate_observations ORDER BY id",
        )
        state["repos"] = query(store, "SELECT repo_key, watching FROM repos ORDER BY repo_key")
    return state


def open_config(name: str, path: Path) -> object:
    """Opens the named configuration's store at ``path`` (the flip-point factory)."""
    return CONFIGS[name].open(path)
