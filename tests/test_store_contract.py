"""The store-tier contract suite, pinned on today's Python implementation before the native port.

Every configuration is built and every store touched through
``tests.store_contract_fixtures`` — the one module the native-store flip re-points — so
the assertions here carry over verbatim when the Python impl is swapped for the
native-engine facade. Coverage: schema goldens and committed-fixture drift for the three
downstream shapes (platform default, cc-steer, captain-hook), ``INSERT OR IGNORE`` dedup
counts, the ``record_verdict`` fidelity matrix, the verdict↔sqlite-vec single-transaction
property (``[judge]`` extra, stubbed embedder), ``unjudged`` ordering plus cc-steer's
paged ``OFFSET`` probe loop, the captain-hook exact schema,
transaction-conflict discipline, the ``sqlite3`` exception types callers observe, and the
``FileStateStore`` file-mtime ledger.
"""

from __future__ import annotations

import asyncio
import json
import shutil
import sqlite3
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import SessionId
from cc_transcript.judge import similar
from cc_transcript.mining.candidates import DedupKey, FeedbackCandidate
from cc_transcript.mining.store import FeedbackStore
from tests import store_contract_fixtures as fx
from tests.store_contract_fixtures import (
    CONFIG_NAMES,
    CONFIGS,
    TransactionConflictError,
    candidate,
    committed_fixture_state,
    count,
    event_rows,
    evidence_rows,
    execute,
    open_config,
    query,
    raw_schema_dump,
    record_file,
    requires_judge,
    store_transaction,
    verdict_rows,
)

if TYPE_CHECKING:
    from collections.abc import Sequence

fake_embedder = fx.fake_embedder
store_clock = fx.store_clock

TESTDATA = Path(__file__).parent / "testdata"
CONFIG_PARAMS = [pytest.param(name, id=name) for name in CONFIG_NAMES]

pytestmark = pytest.mark.anyio


@dataclass(frozen=True, slots=True)
class Verdict:
    """A VerdictLike test double — every attribute record_verdict reads."""

    category: str = "wrong_approach"
    summary: str = "s"
    confidence: float = 0.9
    rationale: str = "r"
    accepted: bool = True
    canonical_key: str | None = None


@dataclass(frozen=True, slots=True)
class VerdictWrite:
    summary: str
    model: str
    fidelity: str
    category: str
    accepted: bool
    confidence: float
    rationale: str
    judged_at: str
    canonical_key: str | None = None


async def seed_events(store: object, keys: Sequence[str]) -> int:
    return await store.record_file_scan("/scan/session.jsonl", 1.0, [candidate(k) for k in keys])  # type: ignore[attr-defined]


async def record_verdict(
    store: object,
    key: str,
    *,
    summary: str = "s",
    model: str = "sonnet",
    fidelity: str = "full",
    canonical_key: str | None = None,
    category: str = "wrong_approach",
    accepted: bool = True,
    confidence: float = 0.9,
    rationale: str = "r",
    role: str = "judge",
    prompt_version: int = 1,
) -> None:
    await store.record_verdict(  # type: ignore[attr-defined]
        DedupKey(key),
        Verdict(
            category=category,
            summary=summary,
            confidence=confidence,
            rationale=rationale,
            accepted=accepted,
            canonical_key=canonical_key,
        ),
        role=role,
        prompt_version=prompt_version,
        model=model,
        fidelity=fidelity,
    )


def expected_stored_event(
    cand: FeedbackCandidate, *, event_id: int, name: str, ingested_at: str, origin_path: str
) -> dict[str, object]:
    payload = dict(cand.payload or {})
    payload["signal"] = {"confidence": 0.9, "reasons": ["stub"], "durable": True}
    row: dict[str, object] = {
        "id": event_id,
        "dedup_key": str(cand.dedup_key),
        "source_kind": str(cand.source_kind),
        "session_id": str(cand.session_id),
        "event_uuid": str(cand.ref.event_uuid),
        "occurred_at": cand.occurred_at.isoformat(),
        "text": cand.text,
        "payload_json": json.dumps(payload),
        "context_json": cand.window.to_json(),
        "cc_version": cand.cc_version,
        "ingested_at": ingested_at,
    }
    if name == "steer":
        row |= {"origin_path": origin_path, "quarantined_reason": None}
    return row


def expected_api_event(cand: FeedbackCandidate, *, event_id: int) -> dict[str, object]:
    return {
        key: value
        for key, value in expected_stored_event(
            cand, event_id=event_id, name="platform", ingested_at=fx.FIXED_NOW, origin_path=""
        ).items()
        if key
        in {
            "id",
            "source_kind",
            "occurred_at",
            "text",
            "payload_json",
            "context_json",
            "event_uuid",
            "session_id",
        }
    }


def expected_unjudged_event(cand: FeedbackCandidate, *, event_id: int) -> dict[str, object]:
    return expected_api_event(cand, event_id=event_id) | {"dedup_key": str(cand.dedup_key)}


def expected_verdict_row(write: VerdictWrite) -> dict[str, object]:
    return {
        "id": 1,
        "dedup_key": "k1",
        "role": "judge",
        "prompt_version": 1,
        "model": write.model,
        "category": write.category,
        "accepted": write.accepted,
        "summary": write.summary,
        "confidence": write.confidence,
        "rationale": write.rationale,
        "canonical_key": write.canonical_key,
        "fidelity": write.fidelity,
        "judged_at": write.judged_at,
    }


@pytest.fixture
def dead_transcript(monkeypatch: pytest.MonkeyPatch) -> None:
    def expired(session_id: SessionId, **_: object) -> object:
        raise TranscriptExpiredError(session_id)

    monkeypatch.setattr(SessionActivity, "from_session", staticmethod(expired))


# --- Schema goldens + committed-fixture drift -------------------------------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_fresh_open_matches_schema_golden(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "fresh.db")
    await store.close()  # type: ignore[attr-defined]
    golden = (TESTDATA / CONFIGS[name].schema_golden()).read_text()
    assert raw_schema_dump(tmp_path / "fresh.db") == golden


@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_committed_fixture_opens_without_schema_drift(name: str, tmp_path: Path) -> None:
    config = CONFIGS[name]
    dst = tmp_path / config.fixture_db
    shutil.copy(TESTDATA / config.fixture_db, dst)
    golden = (TESTDATA / config.schema_golden()).read_text()
    before = raw_schema_dump(dst)
    store = await open_config(name, dst)
    await store.close()  # type: ignore[attr-defined]
    after = raw_schema_dump(dst)
    assert before == golden
    assert after == golden


@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_committed_fixture_reads_seeded_rows_through_store_api(name: str, tmp_path: Path) -> None:
    config = CONFIGS[name]
    dst = tmp_path / config.fixture_db
    shutil.copy(TESTDATA / config.fixture_db, dst)
    store = await open_config(name, dst)
    seeded = {
        "k1": candidate("k1"),
        "k2": candidate("k2"),
        "k3": candidate("k3", empty_window=True),
    }
    judged = expected_unjudged_event(seeded["k1"], event_id=1) | {
        "category": "wrong_approach",
        "accepted": 1,
        "confidence": 0.9,
        "summary": "seed summary",
        "rationale": "seed rationale",
        "model": "sonnet",
    }
    expected: dict[str, object] = {
        "events": [
            expected_api_event(seeded["k3"], event_id=3),
            expected_api_event(seeded["k2"], event_id=2),
            expected_api_event(seeded["k1"], event_id=1),
        ],
        "unjudged": [
            expected_unjudged_event(seeded["k2"], event_id=2),
            *(
                []
                if name == "steer"
                else [expected_unjudged_event(seeded["k3"], event_id=3)]
            ),
        ],
        "judged": [judged],
        "file_mtimes": {"/repo/project/session.jsonl": 1_700_000_000.125},
    }
    if name == "steer":
        expected["quarantine"] = [
            {"dedup_key": "k1", "origin_path": "/repo/project/session.jsonl", "quarantined_reason": None},
            {"dedup_key": "k2", "origin_path": "/repo/project/session.jsonl", "quarantined_reason": None},
            {
                "dedup_key": "k3",
                "origin_path": "/repo/project/session.jsonl",
                "quarantined_reason": "accrued_context_empty",
            },
        ]
    if name == "hook":
        expected |= {
            "candidates": [
                {
                    "id": 1,
                    "repo_key": "github.com/acme/repo",
                    "candidate_kind": "create",
                    "rule": "always-use-uv",
                    "source_kind": "transcript_message",
                    "status": "watching",
                    "created_at": fx.FIXED_NOW,
                    "updated_at": fx.FIXED_NOW,
                    "generation": 1,
                    "resolved_at": None,
                    "origin_repo_key": None,
                    "pack_name": None,
                    "announced_status": None,
                }
            ],
            "observations": [
                {
                    "id": 1,
                    "candidate_id": 1,
                    "dedup_key": "k1",
                    "session_id": "sess-0001",
                    "occurred_at": fx.FIXED_NOW,
                }
            ],
            "repos": [{"repo_key": "github.com/acme/repo", "watching": 1}],
        }
    assert await committed_fixture_state(store, name) == expected
    await store.close()  # type: ignore[attr-defined]


# --- INSERT OR IGNORE dedup counts ------------------------------------------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_record_file_scan_dedups_without_replacing_original_rows(
    name: str, tmp_path: Path, store_clock: fx.StoreClock
) -> None:
    store = await open_config(name, tmp_path / "s.db")
    original = candidate(
        "k1", text="original text", occurred_at="2026-01-01T00:00:01+00:00", payload={"version": "original"}
    )
    sibling = candidate("k2", text="sibling text", payload={"version": "sibling"})
    same_batch_replacement = candidate(
        "k1",
        text="same-batch replacement",
        occurred_at="2026-01-01T00:00:02+00:00",
        payload={"version": "same-batch replacement"},
    )
    assert await store.record_file_scan(  # type: ignore[attr-defined]
        "/original.jsonl", 1_700_000_000.125, [original, sibling, same_batch_replacement]
    ) == 2
    expected = [
        expected_stored_event(
            original, event_id=1, name=name, ingested_at=fx.FIXED_NOW, origin_path="/original.jsonl"
        ),
        expected_stored_event(
            sibling, event_id=2, name=name, ingested_at=fx.FIXED_NOW, origin_path="/original.jsonl"
        ),
    ]
    assert await event_rows(store, name) == expected

    store_clock.value = "2026-01-01T00:01:00+00:00"
    later_replacement = candidate(
        "k1",
        text="later replacement",
        occurred_at="2026-01-01T00:00:03+00:00",
        payload={"version": "later replacement"},
    )
    assert await store.record_file_scan(  # type: ignore[attr-defined]
        "/replacement.jsonl", 1_700_000_000.875, [later_replacement]
    ) == 0
    assert await event_rows(store, name) == expected
    assert await store.file_mtimes() == {  # type: ignore[attr-defined]
        "/original.jsonl": 1_700_000_000.125,
        "/replacement.jsonl": 1_700_000_000.875,
    }


# --- record_verdict fidelity matrix -----------------------------------------
FIDELITY_CASES = [
    pytest.param(
        [
            VerdictWrite(
                "preview",
                "sonnet",
                "summary",
                "preview_category",
                False,
                0.25,
                "preview rationale",
                "2026-01-01T00:00:01+00:00",
            ),
            VerdictWrite(
                "hydrated",
                "opus",
                "full",
                "hydrated_category",
                True,
                0.95,
                "hydrated rationale",
                "2026-01-01T00:00:02+00:00",
            ),
        ],
        VerdictWrite(
            "hydrated",
            "opus",
            "full",
            "hydrated_category",
            True,
            0.95,
            "hydrated rationale",
            "2026-01-01T00:00:02+00:00",
        ),
        id="summary-then-full-cross-model-upgrades",
    ),
    pytest.param(
        [
            VerdictWrite(
                "first",
                "sonnet",
                "full",
                "first_category",
                False,
                0.35,
                "first rationale",
                "2026-01-01T00:00:03+00:00",
            ),
            VerdictWrite(
                "second",
                "opus",
                "full",
                "second_category",
                True,
                0.85,
                "second rationale",
                "2026-01-01T00:00:04+00:00",
            ),
        ],
        VerdictWrite(
            "first",
            "sonnet",
            "full",
            "first_category",
            False,
            0.35,
            "first rationale",
            "2026-01-01T00:00:03+00:00",
        ),
        id="full-then-full-first-full-wins-noop",
    ),
    pytest.param(
        [
            VerdictWrite(
                "full first",
                "sonnet",
                "full",
                "full_category",
                True,
                0.75,
                "full rationale",
                "2026-01-01T00:00:05+00:00",
            ),
            VerdictWrite(
                "summary second",
                "opus",
                "summary",
                "summary_category",
                False,
                0.15,
                "summary rationale",
                "2026-01-01T00:00:06+00:00",
            ),
        ],
        VerdictWrite(
            "full first",
            "sonnet",
            "full",
            "full_category",
            True,
            0.75,
            "full rationale",
            "2026-01-01T00:00:05+00:00",
        ),
        id="full-then-summary-does-not-downgrade",
    ),
    pytest.param(
        [
            VerdictWrite(
                "a",
                "sonnet",
                "summary",
                "a_category",
                False,
                0.45,
                "a rationale",
                "2026-01-01T00:00:07+00:00",
            ),
            VerdictWrite(
                "b",
                "opus",
                "summary",
                "b_category",
                True,
                0.65,
                "b rationale",
                "2026-01-01T00:00:08+00:00",
            ),
        ],
        VerdictWrite(
            "a",
            "sonnet",
            "summary",
            "a_category",
            False,
            0.45,
            "a rationale",
            "2026-01-01T00:00:07+00:00",
        ),
        id="summary-then-summary-first-wins",
    ),
    pytest.param(
        [
            VerdictWrite(
                "only",
                "sonnet",
                "full",
                "only_category",
                True,
                0.55,
                "only rationale",
                "2026-01-01T00:00:09+00:00",
            )
        ],
        VerdictWrite(
            "only",
            "sonnet",
            "full",
            "only_category",
            True,
            0.55,
            "only rationale",
            "2026-01-01T00:00:09+00:00",
        ),
        id="single-full-roundtrips-none-canonical-key",
    ),
]


@pytest.mark.parametrize("name", CONFIG_PARAMS)
@pytest.mark.parametrize(("ops", "expected"), FIDELITY_CASES)
async def test_record_verdict_fidelity_matrix(
    name: str,
    ops: list[VerdictWrite],
    expected: VerdictWrite,
    tmp_path: Path,
    store_clock: fx.StoreClock,
) -> None:
    store = await open_config(name, tmp_path / "s.db")
    await seed_events(store, ["k1"])
    for write in ops:
        store_clock.value = write.judged_at
        await record_verdict(
            store,
            "k1",
            summary=write.summary,
            model=write.model,
            fidelity=write.fidelity,
            canonical_key=write.canonical_key,
            category=write.category,
            accepted=write.accepted,
            confidence=write.confidence,
            rationale=write.rationale,
        )
    assert await verdict_rows(store, name) == [expected_verdict_row(expected)]


@requires_judge
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_summary_to_full_upgrade_carries_the_new_canonical_key(
    name: str, tmp_path: Path, fake_embedder: None, store_clock: fx.StoreClock
) -> None:
    store = await open_config(name, tmp_path / "s.db", vectors=True)
    await seed_events(store, ["k1"])
    store_clock.value = "2026-01-01T00:01:01+00:00"
    await record_verdict(store, "k1", summary="preview", model="sonnet", fidelity="summary", canonical_key="old-rule")
    store_clock.value = "2026-01-01T00:01:02+00:00"
    await record_verdict(store, "k1", summary="hydrated", model="opus", fidelity="full", canonical_key="new-rule")
    expected = VerdictWrite(
        "hydrated",
        "opus",
        "full",
        "wrong_approach",
        True,
        0.9,
        "r",
        "2026-01-01T00:01:02+00:00",
        "new-rule",
    )
    assert await verdict_rows(store, name) == [expected_verdict_row(expected)]
    assert await evidence_rows(store) == [
        {
            "dedup_key": "k1",
            "role": "judge",
            "prompt_version": 1,
            "canonical_key": "new-rule",
            "evidence_text": "always use uv not pip",
        }
    ]
    assert await count(store, "verdict_vectors") == 1


@requires_judge
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_summary_to_full_upgrade_without_canonical_key_clears_evidence(
    name: str, tmp_path: Path, fake_embedder: None, store_clock: fx.StoreClock
) -> None:
    store = await open_config(name, tmp_path / "s.db", vectors=True)
    await seed_events(store, ["k1"])
    store_clock.value = "2026-01-01T00:02:01+00:00"
    await record_verdict(store, "k1", summary="preview", fidelity="summary", canonical_key="old-rule")
    assert await evidence_rows(store) == [
        {
            "dedup_key": "k1",
            "role": "judge",
            "prompt_version": 1,
            "canonical_key": "old-rule",
            "evidence_text": "always use uv not pip",
        }
    ]
    assert await count(store, "verdict_vectors") == 1

    store_clock.value = "2026-01-01T00:02:02+00:00"
    await record_verdict(store, "k1", summary="hydrated", model="opus", fidelity="full", canonical_key=None)
    expected = VerdictWrite(
        "hydrated",
        "opus",
        "full",
        "wrong_approach",
        True,
        0.9,
        "r",
        "2026-01-01T00:02:02+00:00",
    )
    assert await verdict_rows(store, name) == [expected_verdict_row(expected)]
    assert await evidence_rows(store) == []
    assert await count(store, "verdict_vectors") == 0


# --- verdict ↔ sqlite-vec evidence: the single-transaction property ----------
@requires_judge
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_verdict_and_evidence_commit_together(
    name: str, tmp_path: Path, fake_embedder: None
) -> None:
    config = CONFIGS[name]
    store = await open_config(name, tmp_path / "s.db", vectors=True)
    await seed_events(store, ["k1"])
    await record_verdict(store, "k1", summary="preview", canonical_key="use-uv", fidelity="full")
    assert await count(store, config.verdict_table) == 1
    assert await count(store, "verdict_vectors") == 1
    assert await evidence_rows(store) == [
        {
            "dedup_key": "k1",
            "role": "judge",
            "prompt_version": 1,
            "canonical_key": "use-uv",
            "evidence_text": "always use uv not pip",
        }
    ]


@requires_judge
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_verdict_and_evidence_roll_back_when_evidence_path_fails(
    name: str, tmp_path: Path, fake_embedder: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = CONFIGS[name]
    store = await open_config(name, tmp_path / "s.db", vectors=True)
    await seed_events(store, ["k1"])
    record_evidence = similar.record_evidence

    async def fail_after_writing(
        evidence_store: FeedbackStore,
        *,
        dedup_key: DedupKey,
        role: str,
        prompt_version: int,
        evidence: similar.Evidence,
    ) -> None:
        await record_evidence(
            evidence_store,
            dedup_key=dedup_key,
            role=role,
            prompt_version=prompt_version,
            evidence=evidence,
        )
        raise sqlite3.IntegrityError("evidence write failed")

    monkeypatch.setattr(similar, "record_evidence", fail_after_writing)
    with pytest.raises(sqlite3.IntegrityError, match="evidence write failed"):
        await record_verdict(store, "k1", summary="preview", canonical_key="use-uv", fidelity="full")
    assert await count(store, config.verdict_table) == 0
    assert await count(store, "verdict_evidence") == 0
    assert await count(store, "verdict_vectors") == 0


@requires_judge
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_full_to_full_noop_does_not_re_upsert_evidence(
    name: str, tmp_path: Path, fake_embedder: None
) -> None:
    store = await open_config(name, tmp_path / "s.db", vectors=True)
    await seed_events(store, ["k1"])
    await record_verdict(store, "k1", model="sonnet", canonical_key="first-rule", fidelity="full")
    await record_verdict(store, "k1", model="opus", canonical_key="second-rule", fidelity="full")
    assert await evidence_rows(store) == [
        {
            "dedup_key": "k1",
            "role": "judge",
            "prompt_version": 1,
            "canonical_key": "first-rule",
            "evidence_text": "always use uv not pip",
        }
    ]


# --- unjudged ordering + cc-steer's paged OFFSET probe loop ------------------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_unjudged_orders_truly_unjudged_before_summary_refresh(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    await seed_events(store, ["k1", "k2", "k3"])
    await record_verdict(store, "k2", summary="preview", fidelity="summary")
    rows = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False)  # type: ignore[attr-defined]
    assert [row["dedup_key"] for row in rows] == ["k1", "k3", "k2"]


async def test_steer_paged_offset_loop_pages_past_dead_summary_rows(tmp_path: Path, dead_transcript: None) -> None:
    store = await open_config("steer", tmp_path / "s.db")
    await seed_events(store, ["f1", "f2"])
    await seed_events(store, ["s1", "s2", "s3"])
    for key in ("s1", "s2", "s3"):
        await record_verdict(store, key, summary="preview", fidelity="summary")
    paged = await store.unjudged(  # type: ignore[attr-defined]
        role="judge", prompt_version=1, refresh_summary=True, probe_hydration=True, limit=3
    )
    unpaged = await store.unjudged(  # type: ignore[attr-defined]
        role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False, limit=3
    )
    assert [row["dedup_key"] for row in paged] == ["f1", "f2"]
    assert [row["dedup_key"] for row in unpaged] == ["f1", "f2", "s1"]


@pytest.mark.parametrize(
    ("name", "expected_keys"),
    [
        pytest.param("platform", ["k1"], id="platform-currently-returns-one"),
        pytest.param("steer", [], id="steer-returns-zero"),
        pytest.param("hook", ["k1"], id="hook-currently-returns-one"),
    ],
)
async def test_refresh_summary_limit_zero_observable_contract(
    name: str, expected_keys: list[str], tmp_path: Path
) -> None:
    store = await open_config(name, tmp_path / "s.db")
    await seed_events(store, ["k1", "k2"])
    rows = await store.unjudged(  # type: ignore[attr-defined]
        role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False, limit=0
    )
    assert [row["dedup_key"] for row in rows] == expected_keys


async def test_steer_event_filter_excludes_quarantined_from_unjudged_and_judged(tmp_path: Path) -> None:
    store = await open_config("steer", tmp_path / "s.db")
    await store.record_file_scan("/scan.jsonl", 1.0, [candidate("live"), candidate("dead", empty_window=True)])  # type: ignore[attr-defined]
    unjudged = await store.unjudged(role="judge", prompt_version=1)  # type: ignore[attr-defined]
    assert [row["dedup_key"] for row in unjudged] == ["live"]
    await record_verdict(store, "live")
    await record_verdict(store, "dead")
    judged = await store.judged(role="judge", prompt_version=1)  # type: ignore[attr-defined]
    assert [row["dedup_key"] for row in judged] == ["live"]
    assert await count(store, "feedback_events", "quarantined_reason IS NOT NULL") == 1


async def test_hook_schema_contains_every_current_column_at_creation(tmp_path: Path) -> None:
    store = await open_config("hook", tmp_path / "hook.db")
    columns = {row["name"] for row in await query(store, "PRAGMA table_info(candidates)")}
    assert {"generation", "resolved_at", "origin_repo_key", "pack_name", "announced_status"} <= columns


# --- transaction-conflict discipline ----------------------------------------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_standalone_write_never_joins_a_foreign_transaction(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    in_transaction = asyncio.Event()

    async def owner() -> None:
        with pytest.raises(RuntimeError, match="owner rolls back"):
            async with store_transaction(store):
                await record_file(store, "/owned.jsonl", 1.0)
                in_transaction.set()
                await asyncio.sleep(0.01)
                raise RuntimeError("owner rolls back")

    async def outsider() -> None:
        await in_transaction.wait()
        with pytest.raises(TransactionConflictError):
            await record_file(store, "/outsider.jsonl", 2.0)

    await asyncio.gather(owner(), outsider())
    assert await store.file_mtimes() == {}  # type: ignore[attr-defined]


@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_nested_transaction_raises_conflict(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    async with store_transaction(store):
        with pytest.raises(TransactionConflictError):
            async with store_transaction(store):
                raise AssertionError("unreachable")


# --- sqlite3 exception parity — the exception types callers observe ----------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_unique_violation_raises_integrity_error(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    await seed_events(store, ["k1"])
    insert = (
        "INSERT INTO feedback_events (dedup_key, source_kind, occurred_at, text, context_json, ingested_at) "
        "VALUES ('k1', 'transcript_message', '2026-01-01T00:00:00+00:00', 't', '{}', '2026-01-01T00:00:00+00:00')"
    )
    with pytest.raises(sqlite3.IntegrityError):
        await execute(store, insert)


@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_foreign_key_violation_raises_integrity_error(name: str, tmp_path: Path) -> None:
    config = CONFIGS[name]
    store = await open_config(name, tmp_path / "s.db")
    insert = (
        f"INSERT INTO {config.verdict_table} "
        f"(dedup_key, role, prompt_version, model, category, {config.accepted_column}, {config.summary_column}, "
        "confidence, rationale, fidelity, judged_at) "
        "VALUES ('missing', 'judge', 1, 'sonnet', 'c', 1, 's', 0.9, 'r', 'full', '2026-01-01T00:00:00+00:00')"
    )
    with pytest.raises(sqlite3.IntegrityError):
        await execute(store, insert)


@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_unknown_table_raises_operational_error(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    with pytest.raises(sqlite3.OperationalError):
        await query(store, "SELECT * FROM no_such_table")


# --- FileStateStore file-mtime ledger ---------------------------------------
@pytest.mark.parametrize("name", CONFIG_PARAMS)
async def test_record_file_upserts_and_file_mtimes_reads_back(name: str, tmp_path: Path) -> None:
    store = await open_config(name, tmp_path / "s.db")
    await record_file(store, "/a.jsonl", 1_700_000_000.125)
    await record_file(store, "/b.jsonl", 1_700_000_000.5)
    await record_file(store, "/a.jsonl", 1_700_000_000.875)
    assert await store.file_mtimes() == {  # type: ignore[attr-defined]
        "/a.jsonl": 1_700_000_000.875,
        "/b.jsonl": 1_700_000_000.5,
    }
