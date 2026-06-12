from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import anyio
import pytest

from cc_transcript.judge.verdicts import (
    AuditEstimate,
    GoldenResult,
    GoldenRow,
    Metrics,
    VerdictStoreMixin,
    exact_upper_bound,
    flip_pairs,
    golden_result,
    run_verdicts,
    sample_audit,
)
from cc_transcript.mining.candidates import DedupKey
from cc_transcript.mining.store import FEEDBACK_DDL, FeedbackStore
from cc_transcript.store import FileStateStore

if TYPE_CHECKING:
    from collections.abc import Mapping
    from pathlib import Path

CC_PUSHBACK_TRIAGE_DDL = """
CREATE TABLE IF NOT EXISTS triage (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  is_pushback INTEGER NOT NULL,
  what_claude_did TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version, model)
);
CREATE INDEX IF NOT EXISTS idx_triage_dedup ON triage(dedup_key);
"""

INSERT_EVENT = (
    "INSERT INTO feedback_events (dedup_key, source_kind, occurred_at, text, context_json, ingested_at) "
    "VALUES (?, ?, ?, ?, '{}', '2026-01-01T00:00:00+00:00')"
)

EMPTY_GOLDEN = GoldenResult(total=0, passed=0, sha256="", failures=())


@dataclass(frozen=True, slots=True)
class PlainVerdict:
    category: str
    summary: str
    confidence: float
    rationale: str
    accepted: bool


@dataclass(frozen=True, slots=True)
class AliasedVerdict:
    category: str
    what_claude_did: str
    confidence: float
    rationale: str
    is_pushback: bool

    @property
    def summary(self) -> str:
        return self.what_claude_did

    @property
    def accepted(self) -> bool:
        return self.is_pushback


class GenericStore(VerdictStoreMixin, FeedbackStore):
    pass


class LegacyStore(VerdictStoreMixin, FeedbackStore):
    VERDICT_TABLE = "triage"
    ACCEPTED_COLUMN = "is_pushback"
    SUMMARY_COLUMN = "what_claude_did"


def judged_row(
    key: str, *, kind: str = "transcript_message", accepted: bool = True, confidence: float = 0.9
) -> dict[str, object]:
    return {"dedup_key": key, "source_kind": kind, "accepted": accepted, "confidence": confidence}


def test_legacy_params_reproduce_cc_pushback_triage_ddl_byte_for_byte() -> None:
    assert LegacyStore.verdicts_ddl() == CC_PUSHBACK_TRIAGE_DDL


def test_generic_params_name_generic_table_and_columns() -> None:
    ddl = GenericStore.verdicts_ddl()
    assert "CREATE TABLE IF NOT EXISTS verdicts (" in ddl
    assert "  accepted INTEGER NOT NULL,\n  summary TEXT NOT NULL," in ddl
    assert "CREATE INDEX IF NOT EXISTS idx_verdicts_dedup ON verdicts(dedup_key);" in ddl


@pytest.mark.parametrize(
    ("store_cls", "verdict"),
    [
        pytest.param(
            GenericStore,
            PlainVerdict(
                category="wrong_approach",
                summary="Force-pushed",
                confidence=0.9,
                rationale="rejects plan",
                accepted=True,
            ),
            id="generic-names",
        ),
        pytest.param(
            LegacyStore,
            AliasedVerdict(
                category="wrong_approach",
                what_claude_did="Force-pushed",
                confidence=0.9,
                rationale="rejects plan",
                is_pushback=True,
            ),
            id="legacy-triage-names",
        ),
    ],
)
def test_verdict_store_roundtrip(
    tmp_path: Path, store_cls: type[GenericStore | LegacyStore], verdict: PlainVerdict | AliasedVerdict
) -> None:
    async def go() -> dict[str, object]:
        db = await FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + store_cls.verdicts_ddl())
        async with store_cls(db) as store:
            for i, key in enumerate(("k1", "k2")):
                await store.store.conn.execute(
                    INSERT_EVENT, (key, "transcript_message", f"2026-01-0{i + 1}T00:00:00+00:00", f"text {key}")
                )
            before = await store.unjudged(role="judge", prompt_version=1, model="sonnet")
            for _ in range(2):
                await store.record_verdict(
                    DedupKey("k1"), verdict, role="judge", prompt_version=1, model="sonnet", fidelity="full"
                )
            count_cur = await store.store.conn.execute(f"SELECT COUNT(*) AS n FROM {store_cls.VERDICT_TABLE}")
            physical_cur = await store.store.conn.execute(
                f"SELECT {store_cls.ACCEPTED_COLUMN} AS a, {store_cls.SUMMARY_COLUMN} AS s, fidelity "
                f"FROM {store_cls.VERDICT_TABLE}"
            )
            return {
                "before": [row["dedup_key"] for row in before],
                "rows": [row["n"] async for row in count_cur][0],
                "physical": [(row["a"], row["s"], row["fidelity"]) async for row in physical_cur],
                "after": [
                    row["dedup_key"] for row in await store.unjudged(role="judge", prompt_version=1, model="sonnet")
                ],
                "other_model": [
                    row["dedup_key"] for row in await store.unjudged(role="judge", prompt_version=1, model="opus")
                ],
                "judged": await store.judged(role="judge", prompt_version=1),
                "keys": await store.dedup_keys(),
            }

    result = anyio.run(go)
    assert result["before"] == ["k1", "k2"]
    assert result["rows"] == 1
    assert result["physical"] == [(1, "Force-pushed", "full")]
    assert result["after"] == ["k2"]
    assert result["other_model"] == ["k1", "k2"]
    judged = result["judged"]
    assert isinstance(judged, list) and len(judged) == 1
    assert judged[0]["dedup_key"] == "k1"
    assert judged[0]["accepted"] == 1
    assert judged[0]["summary"] == "Force-pushed"
    assert (judged[0]["category"], judged[0]["rationale"]) == ("wrong_approach", "rejects plan")
    assert judged[0]["model"] == "sonnet"
    assert result["keys"] == {"k1", "k2"}


def test_run_verdicts_persists_each_verdict_and_skips_failed_rows() -> None:
    rows = [{"dedup_key": f"k{i}", "text": f"t{i}"} for i in range(4)]
    persisted: list[tuple[str, str]] = []

    async def judge(prompt: str) -> str:
        if prompt == "prompt:t2":
            raise RuntimeError("boom")
        return prompt.upper()

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        persisted.append((str(row["dedup_key"]), verdict))

    async def go() -> tuple[int, int]:
        return await run_verdicts(rows, lambda row: f"prompt:{row['text']}", judge, persist, concurrency=2)

    assert anyio.run(go) == (3, 1)
    assert sorted(persisted) == [("k0", "PROMPT:T0"), ("k1", "PROMPT:T1"), ("k3", "PROMPT:T3")]


def test_run_verdicts_respects_concurrency() -> None:
    state = {"active": 0, "peak": 0}

    async def judge(prompt: str) -> str:
        state["active"] += 1
        state["peak"] = max(state["peak"], state["active"])
        await anyio.sleep(0.01)
        state["active"] -= 1
        return prompt

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        del row, verdict

    async def go() -> tuple[int, int]:
        rows = [{"dedup_key": str(i)} for i in range(8)]
        return await run_verdicts(rows, lambda row: str(row["dedup_key"]), judge, persist, concurrency=2)

    assert anyio.run(go) == (8, 0)
    assert state["peak"] == 2


def test_sample_audit_is_seeded_deterministic_and_oversamples_lowest_confidence() -> None:
    rows = [judged_row(f"k{i:02d}", accepted=i % 2 == 0, confidence=0.5 + i / 100) for i in range(30)]
    draws = [
        sample_audit(
            rows,
            accepts=5,
            rejects=5,
            seed=7,
            quotas={"interrupt_rejection": None},
            remainder_kind="transcript_message",
        )
        for _ in range(2)
    ]
    assert draws[0] == draws[1]
    assert len(draws[0].core) == 6
    assert [row["dedup_key"] for row in draws[0].oversample] == ["k00", "k02", "k01", "k03"]
    assert not {str(row["dedup_key"]) for row in draws[0].core} & {str(row["dedup_key"]) for row in draws[0].oversample}
    accepted_core = [row for row in draws[0].core if row["accepted"]]
    assert len(accepted_core) == 3


def test_sample_audit_exhausts_none_quota_kinds_before_remainder() -> None:
    interrupts = [judged_row(f"i{i}", kind="interrupt_rejection", confidence=0.9) for i in range(3)]
    transcripts = [judged_row(f"t{i:02d}", confidence=0.5 + i / 100) for i in range(10)]
    sample = sample_audit(
        interrupts + transcripts,
        accepts=5,
        rejects=0,
        seed=1,
        quotas={"interrupt_rejection": None},
        remainder_kind="transcript_message",
    )
    core_keys = {str(row["dedup_key"]) for row in sample.core}
    assert {"i0", "i1", "i2"} <= core_keys
    assert len(sample.core) == 4
    assert [row["dedup_key"] for row in sample.oversample] == ["t00"]


@pytest.mark.parametrize(
    ("hits", "n", "expected"),
    [
        pytest.param(0, 3, 0.6315968501359612, id="rule-of-three-n3"),
        pytest.param(0, 60, 0.04870291331009746, id="rule-of-three-n60"),
        pytest.param(1, 10, 0.3941633024365048, id="one-hit-in-ten"),
        pytest.param(2, 20, 0.2826185248858609, id="two-hits-in-twenty"),
    ],
)
def test_exact_upper_bound_matches_clopper_pearson(hits: int, n: int, expected: float) -> None:
    assert exact_upper_bound(hits, n) == pytest.approx(expected, abs=1e-9)


def test_exact_upper_bound_saturates_at_one() -> None:
    assert exact_upper_bound(3, 3) == 1.0
    assert exact_upper_bound(5, 3) == 1.0


def metrics(*, judged: int, accepted: int, core_accepts: AuditEstimate, core_rejects: AuditEstimate) -> Metrics:
    return Metrics(
        prompt_version=3,
        total=judged + 20,
        judged=judged,
        accepted=accepted,
        golden=EMPTY_GOLDEN,
        core_accepts=core_accepts,
        core_rejects=core_rejects,
        pool_accepts=AuditEstimate(audited=0, hits=0),
        pool_rejects=AuditEstimate(audited=0, hits=0),
        by_kind={},
        disagreements=(),
    )


def test_metrics_arithmetic() -> None:
    m = metrics(judged=100, accepted=40, core_accepts=AuditEstimate(20, 18), core_rejects=AuditEstimate(20, 2))
    assert m.precision == pytest.approx(0.9)
    assert m.contamination == pytest.approx(0.1)
    assert m.contamination_upper == pytest.approx(0.2826185248858609, abs=1e-9)
    assert m.recall_hat == pytest.approx(36 / 42)
    assert m.pool_accepts.rate is None


def test_metrics_unaudited_yields_none() -> None:
    m = metrics(judged=100, accepted=40, core_accepts=AuditEstimate(0, 0), core_rejects=AuditEstimate(0, 0))
    assert m.precision is None
    assert m.contamination is None
    assert m.contamination_upper is None
    assert m.recall_hat is None


def golden(key: str, *, expected: bool) -> GoldenRow:
    return GoldenRow(
        dedup_key=key, source_kind="transcript_message", text=f"text {key}", expected=expected, note="frozen"
    )


def test_golden_result_hard_fails_on_corpus_drift() -> None:
    with pytest.raises(LookupError, match=r"drift.*\['gone'\]"):
        golden_result((golden("gone", expected=True),), {"present"}, {}, "sha")


def test_golden_result_counts_matches_mismatches_and_unjudged() -> None:
    rows = (golden("match", expected=True), golden("mismatch", expected=True), golden("unjudged", expected=False))
    judge_by_key: dict[str, Mapping[str, object]] = {
        "match": {"accepted": 1, "category": "wrong_approach", "rationale": "faults the plan"},
        "mismatch": {"accepted": 0, "category": "question", "rationale": "seeks information"},
    }
    result = golden_result(rows, {"match", "mismatch", "unjudged"}, judge_by_key, "sha")
    assert (result.total, result.passed, result.sha256) == (3, 1, "sha")
    by_key = {failure.dedup_key: failure for failure in result.failures}
    assert set(by_key) == {"mismatch", "unjudged"}
    assert (by_key["mismatch"].category, by_key["mismatch"].rationale) == ("question", "seeks information")
    assert (by_key["unjudged"].category, by_key["unjudged"].rationale) == (None, None)
    assert by_key["unjudged"].expected is False


def test_flip_pairs_reports_only_side_changes_over_the_overlap() -> None:
    earlier = [
        {"dedup_key": "a", "text": "ta", "category": "question", "accepted": 0},
        {"dedup_key": "b", "text": "tb", "category": "premature", "accepted": 1},
        {"dedup_key": "earlier-only", "text": "tc", "category": "other", "accepted": 0},
    ]
    later = [
        {"dedup_key": "b", "text": "tb2", "category": "premature", "accepted": 1},
        {"dedup_key": "a", "text": "ta2", "category": "wrong_approach", "accepted": 1},
        {"dedup_key": "later-only", "text": "td", "category": "other", "accepted": 1},
    ]
    report = flip_pairs(earlier, later)
    assert report.common == 2
    assert report.rate == 0.5
    assert [(f.dedup_key, f.from_category, f.to_category, f.text) for f in report.flips] == [
        ("a", "question", "wrong_approach", "ta2")
    ]
    assert flip_pairs([], []).rate is None


def test_refresh_summary_re_yields_summary_rows_until_a_full_verdict_replaces(tmp_path: Path) -> None:
    summary_verdict = PlainVerdict(
        category="wrong_approach", summary="from previews", confidence=0.6, rationale="summary window", accepted=False
    )
    full_verdict = PlainVerdict(
        category="wrong_approach", summary="from transcript", confidence=0.9, rationale="hydrated", accepted=True
    )

    async def record(store: GenericStore, verdict: PlainVerdict, fidelity: str) -> None:
        await store.record_verdict(
            DedupKey("k1"), verdict, role="judge", prompt_version=1, model="sonnet", fidelity=fidelity
        )

    async def go() -> dict[str, object]:
        db = await FileStateStore.open(
            tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + GenericStore.verdicts_ddl()
        )
        async with GenericStore(db) as store:
            await store.store.conn.execute(
                INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "text k1")
            )
            await record(store, summary_verdict, "summary")
            plain = await store.unjudged(role="judge", prompt_version=1, model="sonnet")
            refresh = await store.unjudged(role="judge", prompt_version=1, model="sonnet", refresh_summary=True)
            await record(store, full_verdict, "full")
            after_full = await store.unjudged(role="judge", prompt_version=1, model="sonnet", refresh_summary=True)
            await record(store, summary_verdict, "summary")
            return {
                "plain": plain,
                "refresh": [row["dedup_key"] for row in refresh],
                "after_full": after_full,
                "judged": await store.judged(role="judge", prompt_version=1),
            }

    result = anyio.run(go)
    assert result["plain"] == []
    assert result["refresh"] == ["k1"]
    assert result["after_full"] == []
    judged = result["judged"]
    assert isinstance(judged, list) and len(judged) == 1
    assert (judged[0]["summary"], judged[0]["accepted"]) == ("from transcript", 1)


def test_run_verdicts_awaits_an_async_prompt_for() -> None:
    rows = [{"dedup_key": f"k{i}", "text": f"t{i}"} for i in range(3)]
    persisted: list[tuple[str, str]] = []

    async def prompt_for(row: Mapping[str, object]) -> str:
        await anyio.sleep(0)
        return f"prompt:{row['text']}"

    async def judge(prompt: str) -> str:
        return prompt.upper()

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        persisted.append((str(row["dedup_key"]), verdict))

    async def go() -> tuple[int, int]:
        return await run_verdicts(rows, prompt_for, judge, persist, concurrency=2)

    assert anyio.run(go) == (3, 0)
    assert sorted(persisted) == [("k0", "PROMPT:T0"), ("k1", "PROMPT:T1"), ("k2", "PROMPT:T2")]


def test_resolved_model_matches_the_backend_table() -> None:
    spawnllm = pytest.importorskip("spawnllm", reason="[llm] extra not installed")
    from cc_transcript.judge.llm import resolved_model

    assert resolved_model("medium") == spawnllm.ClaudeCliBackend.models["medium"]


def test_structured_judge_defers_the_structured_call() -> None:
    pytest.importorskip("spawnllm", reason="[llm] extra not installed")
    import inspect

    from pydantic import BaseModel

    from cc_transcript.judge.llm import structured_judge

    class Toy(BaseModel):
        value: int

    judge = structured_judge(Toy, tier="medium", timeout=1)
    coroutine = judge("prompt")
    assert inspect.iscoroutine(coroutine)
    coroutine.close()
