from __future__ import annotations

import hashlib
import sys
from importlib.util import find_spec
from typing import TYPE_CHECKING

import anyio
import pytest

from cc_transcript.judge import similar
from cc_transcript.judge.similar import (
    KeyOverlap,
    Suggestion,
    near_duplicate_keys,
    suggest_canonical_keys,
)
from cc_transcript.judge.verdicts import VerdictStoreMixin
from cc_transcript.mining.candidates import DedupKey
from cc_transcript.mining.store import FEEDBACK_DDL, FeedbackStore
from cc_transcript.store import FileStateStore

if TYPE_CHECKING:
    from pathlib import Path

    import numpy as np

INSERT_EVENT = (
    "INSERT INTO feedback_events (dedup_key, source_kind, occurred_at, text, context_json, ingested_at) "
    "VALUES (?, 'transcript_message', '2026-01-01T00:00:00+00:00', ?, '{}', '2026-01-01T00:00:00+00:00')"
)


class GenericStore(VerdictStoreMixin, FeedbackStore):
    pass


def fake_embed(text: str) -> np.ndarray:
    import numpy as np

    vector = np.zeros(similar.EMBED_DIM, dtype=np.float32)
    for token in text.lower().split():
        vector[int(hashlib.md5(token.encode()).hexdigest(), 16) % similar.EMBED_DIM] += 1.0
    return vector / (norm if (norm := np.linalg.norm(vector)) else 1.0)


@pytest.fixture
def fake_embedder(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(similar, "default_embedder", lambda: fake_embed)


async def open_store(tmp_path: Path) -> GenericStore:
    db = await FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + GenericStore.verdicts_ddl())
    return GenericStore(db)


async def seed(
    store: GenericStore, key: str, text: str, canonical_key: str, *, prompt_version: int = 1, fidelity: str = "full"
) -> None:
    await store.store.conn.execute(INSERT_EVENT, (key, text))
    await store.record_verdict(
        DedupKey(key),
        StubVerdict(canonical_key=canonical_key),
        role="judge",
        prompt_version=prompt_version,
        model="sonnet",
        fidelity=fidelity,
    )


class StubVerdict:
    def __init__(self, *, canonical_key: str | None, summary: str = "") -> None:
        self.category = "wrong_approach"
        self.summary = summary
        self.confidence = 0.9
        self.rationale = "r"
        self.accepted = True
        self.canonical_key = canonical_key


async def counts(store: GenericStore) -> tuple[int, int]:
    vec = (await (await store.store.conn.execute("SELECT COUNT(*) AS n FROM verdict_vectors")).fetchone())["n"]
    evi = (await (await store.store.conn.execute("SELECT COUNT(*) AS n FROM verdict_evidence")).fetchone())["n"]
    return vec, evi


async def table_exists(store: GenericStore, name: str) -> bool:
    cur = await store.store.conn.execute("SELECT 1 FROM sqlite_master WHERE name = ?", (name,))
    return (await cur.fetchone()) is not None


def test_record_verdict_inserts_one_vector_and_rerecord_upserts(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> dict[str, object]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "always use uv not pip"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="summary",
            )
            after_first = await counts(store)
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip", summary="hydrated"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            return {"after_first": after_first, "after_upgrade": await counts(store)}

    result = anyio.run(go)
    assert result["after_first"] == (1, 1)
    assert result["after_upgrade"] == (1, 1)


def test_verdict_without_canonical_key_never_touches_the_vector_store(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> bool:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "some feedback"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key=None),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
            return await table_exists(store, "verdict_vectors")

    assert anyio.run(go) is False


def test_full_to_full_noop_does_not_re_upsert_the_vector(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> list[tuple[object, object]]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "use uv not pip"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="first-rule"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="second-rule"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            cur = await store.store.conn.execute("SELECT canonical_key FROM verdict_evidence")
            evidence = [(row["canonical_key"],) async for row in cur]
            return evidence

    assert anyio.run(go) == [("first-rule",)]


def test_dropping_canonical_key_on_upgrade_clears_stale_evidence(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> dict[str, object]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "always use uv not pip"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="summary",
            )
            before = await counts(store)
            keys_before = [
                s.canonical_key for s in await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=5)
            ]
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key=None, summary="hydrated"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            verdict_ck = (
                await (
                    await store.store.conn.execute("SELECT canonical_key FROM verdicts WHERE dedup_key = 'k1'")
                ).fetchone()
            )["canonical_key"]
            return {
                "before": before,
                "after": await counts(store),
                "keys_before": keys_before,
                "keys_after": [
                    s.canonical_key
                    for s in await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=5)
                ],
                "verdict_ck": verdict_ck,
            }

    result = anyio.run(go)
    assert result["before"] == (1, 1)
    assert result["keys_before"] == ["use-uv-not-pip"]
    assert result["after"] == (0, 0)
    assert result["keys_after"] == []
    assert result["verdict_ck"] is None


def test_dropping_canonical_key_clears_evidence_on_a_fresh_connection(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> dict[str, object]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "always use uv not pip"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="summary",
            )
            seeded = await counts(store)
        async with await open_store(tmp_path) as store:
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key=None, summary="hydrated"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            return {"seeded": seeded, "cleared": await counts(store)}

    result = anyio.run(go)
    assert result["seeded"] == (1, 1)
    assert result["cleared"] == (0, 0)


def test_dropping_canonical_key_clears_evidence_with_only_sqlite_vec_installed(
    tmp_path: Path, fake_embedder: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def go() -> dict[str, object]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "always use uv not pip"))
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="summary",
            )
            seeded = await counts(store)
        monkeypatch.setitem(sys.modules, "model2vec", None)
        async with await open_store(tmp_path) as store:
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key=None, summary="hydrated"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            return {"seeded": seeded, "cleared": await counts(store)}

    result = anyio.run(go)
    assert result["seeded"] == (1, 1)
    assert result["cleared"] == (0, 0)


def test_judge_and_auditor_evidence_coexist_on_one_event(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> tuple[tuple[int, int], list[tuple[object, object]]]:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "always use uv not pip"))
            for role, canonical_key in (("judge", "use-uv-judge"), ("auditor", "use-uv-auditor")):
                await store.record_verdict(
                    DedupKey("k1"),
                    StubVerdict(canonical_key=canonical_key),
                    role=role,
                    prompt_version=1,
                    model="sonnet",
                    fidelity="full",
                )
            cur = await store.store.conn.execute("SELECT role, canonical_key FROM verdict_evidence ORDER BY role")
            rows = [(row["role"], row["canonical_key"]) async for row in cur]
            return await counts(store), rows

    (vec, evi), rows = anyio.run(go)
    assert (vec, evi) == (2, 2)
    assert rows == [("auditor", "use-uv-auditor"), ("judge", "use-uv-judge")]


def test_concurrent_record_verdict_prepares_the_fresh_connection_once(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> tuple[tuple[int, int], int]:
        async with await open_store(tmp_path) as store:
            for i in range(4):
                await store.store.conn.execute(INSERT_EVENT, (f"k{i}", f"use uv variant {i}"))

            async def record(i: int) -> None:
                await store.record_verdict(
                    DedupKey(f"k{i}"),
                    StubVerdict(canonical_key=f"rule-{i}"),
                    role="judge",
                    prompt_version=1,
                    model="sonnet",
                    fidelity="full",
                )

            async with anyio.create_task_group() as tg:
                for i in range(4):
                    tg.start_soon(record, i)
            verdicts = (await (await store.store.conn.execute("SELECT COUNT(*) AS n FROM verdicts")).fetchone())["n"]
            return await counts(store), verdicts

    (vec, evi), verdicts = anyio.run(go)
    assert (vec, evi) == (4, 4)
    assert verdicts == 4


def test_suggest_returns_the_seeded_neighbor_first(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> list[Suggestion]:
        async with await open_store(tmp_path) as store:
            await seed(store, "k1", "always use uv not pip", "use-uv-not-pip")
            await seed(store, "k2", "never force push to main", "no-force-push")
            await seed(store, "k3", "run the test suite before commit", "run-tests-first")
            return await suggest_canonical_keys(store, "please use uv instead of pip", prompt_version=1, k=3)

    result = anyio.run(go)
    assert result[0].canonical_key == "use-uv-not-pip"
    assert result[0].score > result[1].score


def test_suggest_aggregates_multiple_evidence_rows_per_key(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> list[Suggestion]:
        async with await open_store(tmp_path) as store:
            await seed(store, "k1", "use uv not pip", "use-uv-not-pip")
            await seed(store, "k2", "use uv over pip", "use-uv-not-pip")
            await seed(store, "k3", "write more tests", "add-tests")
            return await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=5)

    result = anyio.run(go)
    keys = [s.canonical_key for s in result]
    assert keys.count("use-uv-not-pip") == 1
    top = next(s for s in result if s.canonical_key == "use-uv-not-pip")
    assert top.score == pytest.approx(1.0, abs=1e-5)
    assert top.sentences == ("use uv not pip", "use uv over pip")


def test_suggest_caps_sentences_at_three(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> Suggestion:
        async with await open_store(tmp_path) as store:
            for i in range(5):
                await seed(store, f"k{i}", f"use uv not pip variant {i}", "use-uv-not-pip")
            result = await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=1)
            return result[0]

    top = anyio.run(go)
    assert top.canonical_key == "use-uv-not-pip"
    assert len(top.sentences) == 3


def test_suggest_scopes_to_prompt_version(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> list[Suggestion]:
        async with await open_store(tmp_path) as store:
            await seed(store, "k1", "use uv not pip", "use-uv-not-pip", prompt_version=1)
            await seed(store, "k2", "use uv not pip", "other-rule-pv2", prompt_version=2)
            return await suggest_canonical_keys(store, "use uv not pip", prompt_version=2, k=5)

    result = anyio.run(go)
    assert [s.canonical_key for s in result] == ["other-rule-pv2"]


def test_near_duplicate_finds_synonym_pair_and_respects_threshold(tmp_path: Path, fake_embedder: None) -> None:
    async def go() -> dict[str, list[KeyOverlap]]:
        async with await open_store(tmp_path) as store:
            await seed(store, "k1", "use uv not pip", "use-uv")
            await seed(store, "k2", "use uv over pip", "prefer-uv")
            await seed(store, "k3", "run the test suite", "run-tests")
            return {
                "low": await near_duplicate_keys(store, prompt_version=1, threshold=0.5),
                "high": await near_duplicate_keys(store, prompt_version=1, threshold=0.95),
            }

    result = anyio.run(go)
    assert [(o.key_a, o.key_b) for o in result["low"]] == [("prefer-uv", "use-uv")]
    assert result["low"][0].similarity == pytest.approx(0.75, abs=0.1)
    assert result["high"] == []


@pytest.mark.parametrize("missing", ["model2vec", "numpy", "sqlite_vec"])
def test_missing_extra_raises_importerror_naming_the_extra(monkeypatch: pytest.MonkeyPatch, missing: str) -> None:
    monkeypatch.setitem(sys.modules, missing, None)
    with pytest.raises(ImportError, match=r"cc-transcript\[judge\]"):
        similar.require_judge_extra()


def test_record_verdict_with_canonical_key_fails_loud_and_writes_no_verdict_without_the_extra(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setitem(sys.modules, "sqlite_vec", None)

    async def go() -> int:
        async with await open_store(tmp_path) as store:
            await store.store.conn.execute(INSERT_EVENT, ("k1", "use uv not pip"))
            with pytest.raises(ImportError, match=r"cc-transcript\[judge\]"):
                await store.record_verdict(
                    DedupKey("k1"),
                    StubVerdict(canonical_key="use-uv-not-pip"),
                    role="judge",
                    prompt_version=1,
                    model="sonnet",
                    fidelity="full",
                )
            cur = await store.store.conn.execute("SELECT COUNT(*) AS n FROM verdicts")
            return (await cur.fetchone())["n"]

    assert anyio.run(go) == 0


def potion_is_cached() -> bool:
    if find_spec("model2vec") is None or find_spec("huggingface_hub") is None:
        return False
    from huggingface_hub import try_to_load_from_cache

    return isinstance(try_to_load_from_cache(similar.EMBED_MODEL, "model.safetensors"), str)


@pytest.mark.integration
@pytest.mark.skipif(not potion_is_cached(), reason="potion-retrieval-32M not cached; prefetch to opt in")
def test_real_embedder_ranks_the_paraphrased_correction_first(tmp_path: Path) -> None:
    async def go() -> list[Suggestion]:
        async with await open_store(tmp_path) as store:
            await seed(store, "k1", "always install dependencies with uv instead of pip", "use-uv-not-pip")
            await seed(store, "k2", "do not force push to the main branch", "never-force-push")
            await seed(store, "k3", "run the full test suite before committing changes", "run-tests-first")
            return await suggest_canonical_keys(
                store, "please use uv rather than pip to add packages", prompt_version=1, k=3
            )

    result = anyio.run(go)
    assert result[0].canonical_key == "use-uv-not-pip"
