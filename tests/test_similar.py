from __future__ import annotations

import hashlib
import sys
from importlib.util import find_spec
from typing import TYPE_CHECKING

import pytest

from cc_transcript.judge import similar
from cc_transcript.judge.similar import (
    KeyOverlap,
    Suggestion,
    near_duplicate_keys,
    suggest_canonical_keys,
)
from cc_transcript.mining.candidates import DedupKey
from cc_transcript.mining.store import FeedbackStore

if TYPE_CHECKING:
    from pathlib import Path

    import numpy as np

pytestmark = pytest.mark.anyio

INSERT_EVENT = (
    "INSERT INTO feedback_events (dedup_key, source_kind, occurred_at, text, context_json, ingested_at) "
    "VALUES (?, 'transcript_message', '2026-01-01T00:00:00+00:00', ?, '{}', '2026-01-01T00:00:00+00:00')"
)


def fake_embed(text: str) -> np.ndarray:
    import numpy as np

    vector = np.zeros(similar.EMBED_DIM, dtype=np.float32)
    for token in text.lower().split():
        vector[int(hashlib.md5(token.encode()).hexdigest(), 16) % similar.EMBED_DIM] += 1.0
    return vector / (norm if (norm := np.linalg.norm(vector)) else 1.0)


@pytest.fixture
def fake_embedder(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(similar, "default_embedder", lambda: fake_embed)


async def open_store(tmp_path: Path) -> FeedbackStore:
    return await FeedbackStore.open(tmp_path / "feedback.db")


async def seed(
    store: FeedbackStore, key: str, text: str, canonical_key: str, *, prompt_version: int = 1, fidelity: str = "full"
) -> None:
    await store.execute(INSERT_EVENT, [key, text])
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


async def counts(store: FeedbackStore) -> tuple[int, int]:
    vec = (await store.sql("SELECT COUNT(*) AS n FROM verdict_vectors"))[0]["n"]
    evi = (await store.sql("SELECT COUNT(*) AS n FROM verdict_evidence"))[0]["n"]
    return vec, evi


async def table_exists(store: FeedbackStore, name: str) -> bool:
    return bool(await store.sql("SELECT 1 FROM sqlite_master WHERE name = ?", [name]))


async def test_record_verdict_inserts_one_vector_and_rerecord_upserts(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "always use uv not pip"])
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
            role="judge",
            prompt_version=1,
            model="sonnet",
            fidelity="summary",
        )
        assert await counts(store) == (1, 1)
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key="use-uv-not-pip", summary="hydrated"),
            role="judge",
            prompt_version=1,
            model="opus",
            fidelity="full",
        )
        assert await counts(store) == (1, 1)


async def test_verdict_without_canonical_key_never_touches_the_vector_store(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "some feedback"])
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key=None),
            role="judge",
            prompt_version=1,
            model="sonnet",
            fidelity="full",
        )
        assert await table_exists(store, "verdict_vectors") is False


async def test_full_to_full_noop_does_not_re_upsert_the_vector(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "use uv not pip"])
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
        evidence = [(row["canonical_key"],) for row in await store.sql("SELECT canonical_key FROM verdict_evidence")]
        assert evidence == [("first-rule",)]


async def test_dropping_canonical_key_on_upgrade_clears_stale_evidence(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "always use uv not pip"])
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
            role="judge",
            prompt_version=1,
            model="sonnet",
            fidelity="summary",
        )
        assert await counts(store) == (1, 1)
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
        verdict_ck = (await store.sql("SELECT canonical_key FROM verdicts WHERE dedup_key = 'k1'"))[0]["canonical_key"]
        assert keys_before == ["use-uv-not-pip"]
        assert await counts(store) == (0, 0)
        assert [
            s.canonical_key for s in await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=5)
        ] == []
        assert verdict_ck is None


async def test_dropping_canonical_key_clears_evidence_on_a_fresh_connection(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "always use uv not pip"])
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
            role="judge",
            prompt_version=1,
            model="sonnet",
            fidelity="summary",
        )
        assert await counts(store) == (1, 1)
    async with await open_store(tmp_path) as store:
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key=None, summary="hydrated"),
            role="judge",
            prompt_version=1,
            model="opus",
            fidelity="full",
        )
        assert await counts(store) == (0, 0)


async def test_dropping_canonical_key_clears_evidence_with_only_sqlite_vec_installed(
    tmp_path: Path, fake_embedder: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "always use uv not pip"])
        await store.record_verdict(
            DedupKey("k1"),
            StubVerdict(canonical_key="use-uv-not-pip", summary="preview"),
            role="judge",
            prompt_version=1,
            model="sonnet",
            fidelity="summary",
        )
        assert await counts(store) == (1, 1)
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
        assert await counts(store) == (0, 0)


async def test_judge_and_auditor_evidence_coexist_on_one_event(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "always use uv not pip"])
        for role, canonical_key in (("judge", "use-uv-judge"), ("auditor", "use-uv-auditor")):
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key=canonical_key),
                role=role,
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
        rows = [
            (row["role"], row["canonical_key"])
            for row in await store.sql("SELECT role, canonical_key FROM verdict_evidence ORDER BY role")
        ]
        assert await counts(store) == (2, 2)
        assert rows == [("auditor", "use-uv-auditor"), ("judge", "use-uv-judge")]


async def test_repeated_record_verdict_prepares_the_fresh_connection_once(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        for i in range(4):
            await store.execute(INSERT_EVENT, [f"k{i}", f"use uv variant {i}"])
        for i in range(4):
            await store.record_verdict(
                DedupKey(f"k{i}"),
                StubVerdict(canonical_key=f"rule-{i}"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
        verdicts = (await store.sql("SELECT COUNT(*) AS n FROM verdicts"))[0]["n"]
        assert await counts(store) == (4, 4)
        assert verdicts == 4


async def test_suggest_returns_the_seeded_neighbor_first(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await seed(store, "k1", "always use uv not pip", "use-uv-not-pip")
        await seed(store, "k2", "never force push to main", "no-force-push")
        await seed(store, "k3", "run the test suite before commit", "run-tests-first")
        result = await suggest_canonical_keys(store, "please use uv instead of pip", prompt_version=1, k=3)
    assert result[0].canonical_key == "use-uv-not-pip"
    assert result[0].score > result[1].score


async def test_suggest_aggregates_multiple_evidence_rows_per_key(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await seed(store, "k1", "use uv not pip", "use-uv-not-pip")
        await seed(store, "k2", "use uv over pip", "use-uv-not-pip")
        await seed(store, "k3", "write more tests", "add-tests")
        result = await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=5)
    keys = [s.canonical_key for s in result]
    assert keys.count("use-uv-not-pip") == 1
    top = next(s for s in result if s.canonical_key == "use-uv-not-pip")
    assert top.score == pytest.approx(1.0, abs=1e-5)
    assert top.sentences == ("use uv not pip", "use uv over pip")


async def test_suggest_caps_sentences_at_three(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        for i in range(5):
            await seed(store, f"k{i}", f"use uv not pip variant {i}", "use-uv-not-pip")
        top = (await suggest_canonical_keys(store, "use uv not pip", prompt_version=1, k=1))[0]
    assert top.canonical_key == "use-uv-not-pip"
    assert len(top.sentences) == 3


async def test_suggest_scopes_to_prompt_version(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await seed(store, "k1", "use uv not pip", "use-uv-not-pip", prompt_version=1)
        await seed(store, "k2", "use uv not pip", "other-rule-pv2", prompt_version=2)
        result = await suggest_canonical_keys(store, "use uv not pip", prompt_version=2, k=5)
    assert [s.canonical_key for s in result] == ["other-rule-pv2"]


async def test_near_duplicate_finds_synonym_pair_and_respects_threshold(tmp_path: Path, fake_embedder: None) -> None:
    async with await open_store(tmp_path) as store:
        await seed(store, "k1", "use uv not pip", "use-uv")
        await seed(store, "k2", "use uv over pip", "prefer-uv")
        await seed(store, "k3", "run the test suite", "run-tests")
        result = {
            "low": await near_duplicate_keys(store, prompt_version=1, threshold=0.5),
            "high": await near_duplicate_keys(store, prompt_version=1, threshold=0.95),
        }
    assert [(o.key_a, o.key_b) for o in result["low"]] == [("prefer-uv", "use-uv")]
    assert result["low"][0].similarity == pytest.approx(0.75, abs=0.1)
    assert result["high"] == []


@pytest.mark.parametrize("missing", ["model2vec", "numpy", "sqlite_vec"])
def test_missing_extra_raises_importerror_naming_the_extra(monkeypatch: pytest.MonkeyPatch, missing: str) -> None:
    monkeypatch.setitem(sys.modules, missing, None)
    with pytest.raises(ImportError, match=r"cc-transcript\[judge\]"):
        similar.require_judge_extra()


async def test_record_verdict_with_canonical_key_fails_loud_and_writes_no_verdict_without_the_extra(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setitem(sys.modules, "sqlite_vec", None)
    async with await open_store(tmp_path) as store:
        await store.execute(INSERT_EVENT, ["k1", "use uv not pip"])
        with pytest.raises(ImportError, match=r"cc-transcript\[judge\]"):
            await store.record_verdict(
                DedupKey("k1"),
                StubVerdict(canonical_key="use-uv-not-pip"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
        assert (await store.sql("SELECT COUNT(*) AS n FROM verdicts"))[0]["n"] == 0


def potion_is_cached() -> bool:
    if find_spec("model2vec") is None or find_spec("huggingface_hub") is None:
        return False
    from huggingface_hub import try_to_load_from_cache

    return isinstance(try_to_load_from_cache(similar.EMBED_MODEL, "model.safetensors"), str)


@pytest.mark.integration
@pytest.mark.skipif(not potion_is_cached(), reason="potion-retrieval-32M not cached; prefetch to opt in")
async def test_real_embedder_ranks_the_paraphrased_correction_first(tmp_path: Path) -> None:
    async with await open_store(tmp_path) as store:
        await seed(store, "k1", "always install dependencies with uv instead of pip", "use-uv-not-pip")
        await seed(store, "k2", "do not force push to the main branch", "never-force-push")
        await seed(store, "k3", "run the full test suite before committing changes", "run-tests-first")
        result = await suggest_canonical_keys(
            store, "please use uv rather than pip to add packages", prompt_version=1, k=3
        )
    assert result[0].canonical_key == "use-uv-not-pip"
