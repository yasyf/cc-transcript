"""Generate the store-tier schema goldens and committed fixture databases.

Freezes, for each of the three store configurations built via the composition API
(platform default, cc-steer-shaped, captain-hook-shaped), two artifacts that
``tests/test_store_contract.py`` pins:

* ``tests/testdata/store_schema_<config>.sql`` — the ``sqlite_master`` dump a fresh open
  produces (user tables/indexes/views only), the schema the native engine must reproduce
  byte-for-byte after the flip.
* ``tests/testdata/store_fixture_<config>.db`` — a small, deterministic seeded database
  the native engine must open without schema drift. Seeded under a pinned ``now()`` so a
  regen is reproducible; canonical keys are never assigned, so the ``[judge]`` extra is
  not required to build the fixtures.

Run: ``uv run --no-sync python scripts/gen_store_fixtures.py``
"""

from __future__ import annotations

import asyncio
import sqlite3
import sys
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from cc_transcript.mining.candidates import DedupKey  # noqa: E402
from tests import store_contract_fixtures as fx  # noqa: E402

TESTDATA = REPO_ROOT / "tests" / "testdata"
SCAN_PATH = "/repo/project/session.jsonl"
SCAN_MTIME = 1_700_000_000.125


class StubVerdict:
    """A minimal verdict with no canonical key (no embedding, no [judge] extra)."""

    def __init__(self, *, summary: str) -> None:
        self.category = "wrong_approach"
        self.summary = summary
        self.confidence = 0.9
        self.rationale = "seed rationale"
        self.accepted = True
        self.canonical_key = None


async def seed(store: object, name: str) -> None:
    candidates = [fx.candidate("k1"), fx.candidate("k2"), fx.candidate("k3", empty_window=True)]
    await store.record_file_scan(SCAN_PATH, SCAN_MTIME, candidates)
    await store.record_verdict(
        DedupKey("k1"), StubVerdict(summary="seed summary"), role="judge", prompt_version=1, model="sonnet",
        fidelity="full",
    )
    if name == "hook":
        await fx.execute(
            store,
            "INSERT INTO candidates (repo_key, candidate_kind, rule, source_kind, status, created_at, updated_at) "
            "VALUES (?, 'create', ?, ?, 'watching', ?, ?)",
            ("github.com/acme/repo", "always-use-uv", str(fx.SOURCE_KIND), fx.FIXED_NOW, fx.FIXED_NOW),
        )
        await fx.execute(
            store,
            "INSERT INTO candidate_observations (candidate_id, dedup_key, session_id, occurred_at) VALUES (1, ?, ?, ?)",
            ("k1", str(fx.SESSION), fx.FIXED_NOW),
        )
        await fx.execute(store, "INSERT INTO repos (repo_key, watching) VALUES (?, 1)", ("github.com/acme/repo",))


async def finalize(store: object, path: Path) -> None:
    await store.close()
    conn = sqlite3.connect(path)
    try:
        conn.execute("VACUUM")
        conn.execute("PRAGMA journal_mode = DELETE")
    finally:
        conn.close()


def clear(path: Path) -> None:
    for suffix in ("", "-wal", "-shm"):
        path.with_name(path.name + suffix).unlink(missing_ok=True)


async def generate() -> None:
    TESTDATA.mkdir(parents=True, exist_ok=True)
    with mock.patch.object(fx, "now", lambda: fx.FIXED_NOW), \
        mock.patch("cc_transcript.mining.store.now", lambda: fx.FIXED_NOW):
        for name in fx.CONFIG_NAMES:
            config = fx.CONFIGS[name]
            db_path = TESTDATA / config.fixture_db
            clear(db_path)
            store = await fx.open_config(name, db_path)
            await seed(store, name)
            await finalize(store, db_path)
            for suffix in ("-wal", "-shm"):
                db_path.with_name(db_path.name + suffix).unlink(missing_ok=True)
            golden = TESTDATA / config.schema_golden()
            golden.write_text(fx.raw_schema_dump(db_path))
            print(f"wrote {golden.relative_to(REPO_ROOT)} and {db_path.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    asyncio.run(generate())
