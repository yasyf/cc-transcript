from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import TYPE_CHECKING

import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.context import ContextWindow, TurnRef
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.judge.verdicts import (
    SLUG_PATTERN,
    AuditEstimate,
    GoldenResult,
    GoldenRow,
    JudgeError,
    Metrics,
    VerdictSchemaError,
    VerdictStoreMixin,
    canonical_slug,
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
  canonical_key TEXT,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version)
);
CREATE INDEX IF NOT EXISTS idx_triage_dedup ON triage(dedup_key);
"""

V8_VERDICT_DDL = """
CREATE TABLE IF NOT EXISTS verdicts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  accepted INTEGER NOT NULL,
  summary TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version, model)
);
CREATE INDEX IF NOT EXISTS idx_verdicts_dedup ON verdicts(dedup_key);
"""

V8_TRIAGE_DDL = """
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

INSERT_EVENT_CONTEXT = (
    "INSERT INTO feedback_events (dedup_key, source_kind, occurred_at, text, context_json, ingested_at) "
    "VALUES (?, ?, ?, ?, ?, '2026-01-01T00:00:00+00:00')"
)

EMPTY_GOLDEN = GoldenResult(total=0, passed=0, sha256="", failures=())


def window_json() -> str:
    anchor = EventRef(SessionId("s"), EventUuid("u"))
    return ContextWindow(
        anchor=anchor,
        before=(),
        trigger=TurnRef(role="user", refs=(anchor,), preview="p", tool_digests=()),
        after=(),
        fidelity="summary",
        preview_chars=200,
    ).to_json()


class LiveActivity:
    """A session whose transcript still resolves every ref the fence hydrates."""

    def turn_of(self, ref: EventRef) -> object:
        return ref


@pytest.fixture
def live_transcript(monkeypatch: pytest.MonkeyPatch) -> None:
    async def alive(session_id: SessionId, **_: object) -> LiveActivity:
        return LiveActivity()

    monkeypatch.setattr(SessionActivity, "from_session", staticmethod(alive))


@pytest.fixture
def dead_transcript(monkeypatch: pytest.MonkeyPatch) -> None:
    async def expired(session_id: SessionId, **_: object) -> LiveActivity:
        raise TranscriptExpiredError(session_id)

    monkeypatch.setattr(SessionActivity, "from_session", staticmethod(expired))


@dataclass(frozen=True, slots=True)
class PlainVerdict:
    category: str
    summary: str
    confidence: float
    rationale: str
    accepted: bool
    canonical_key: str | None = None


@dataclass(frozen=True, slots=True)
class AliasedVerdict:
    category: str
    what_claude_did: str
    confidence: float
    rationale: str
    is_pushback: bool
    canonical_key: str | None = None

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
    assert "  canonical_key TEXT,\n" in ddl
    assert "UNIQUE(dedup_key, role, prompt_version)\n" in ddl
    assert "prompt_version, model)" not in ddl
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
                canonical_key="never-force-push",
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
                canonical_key="never-force-push",
            ),
            id="legacy-triage-names",
        ),
    ],
)
def test_verdict_store_roundtrip(
    tmp_path: Path, store_cls: type[GenericStore | LegacyStore], verdict: PlainVerdict | AliasedVerdict
) -> None:
    async def go() -> dict[str, object]:
        db = FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + store_cls.verdicts_ddl())
        with store_cls(db) as store:
            for i, key in enumerate(("k1", "k2")):
                store.store.conn.execute(
                    INSERT_EVENT, (key, "transcript_message", f"2026-01-0{i + 1}T00:00:00+00:00", f"text {key}")
                )
            before = await store.unjudged(role="judge", prompt_version=1)
            for _ in range(2):
                await store.record_verdict(
                    DedupKey("k1"), verdict, role="judge", prompt_version=1, model="sonnet", fidelity="full"
                )
            count_cur = store.store.conn.execute(f"SELECT COUNT(*) AS n FROM {store_cls.VERDICT_TABLE}")
            physical_cur = store.store.conn.execute(
                f"SELECT {store_cls.ACCEPTED_COLUMN} AS a, {store_cls.SUMMARY_COLUMN} AS s, canonical_key AS ck, "
                f"fidelity FROM {store_cls.VERDICT_TABLE}"
            )
            return {
                "before": [row["dedup_key"] for row in before],
                "rows": [row["n"] for row in count_cur][0],
                "physical": [(row["a"], row["s"], row["ck"], row["fidelity"]) for row in physical_cur],
                "after": [row["dedup_key"] for row in await store.unjudged(role="judge", prompt_version=1)],
                "judged": store.judged(role="judge", prompt_version=1),
                "keys": store.dedup_keys(),
            }

    result = asyncio.run(go())
    assert result["before"] == ["k1", "k2"]
    assert result["rows"] == 1
    assert result["physical"] == [(1, "Force-pushed", "never-force-push", "full")]
    assert result["after"] == ["k2"]
    judged = result["judged"]
    assert isinstance(judged, list) and len(judged) == 1
    assert judged[0]["dedup_key"] == "k1"
    assert judged[0]["accepted"] == 1
    assert judged[0]["summary"] == "Force-pushed"
    assert (judged[0]["category"], judged[0]["rationale"]) == ("wrong_approach", "rejects plan")
    assert judged[0]["model"] == "sonnet"
    assert result["keys"] == {"k1", "k2"}


def plain(
    *, summary: str = "s", accepted: bool = True, canonical_key: str | None = None, confidence: float = 0.9
) -> PlainVerdict:
    return PlainVerdict(
        category="wrong_approach",
        summary=summary,
        confidence=confidence,
        rationale="r",
        accepted=accepted,
        canonical_key=canonical_key,
    )


def open_generic(tmp_path: Path) -> GenericStore:
    db = FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + GenericStore.verdicts_ddl())
    return GenericStore(db)


def one_verdict(store: GenericStore) -> dict[str, object]:
    cur = store.store.conn.execute(
        "SELECT COUNT(*) AS n, MAX(model) AS model, MAX(summary) AS summary, MAX(canonical_key) AS ck, "
        "MAX(fidelity) AS fidelity FROM verdicts"
    )
    return [dict(row) for row in cur][0]


def test_same_key_different_model_never_holds_two_rows(tmp_path: Path) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "t"))
            await store.record_verdict(
                DedupKey("k1"), plain(summary="a"), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            await store.record_verdict(
                DedupKey("k1"), plain(summary="b"), role="judge", prompt_version=1, model="opus", fidelity="summary"
            )
            after_summary = one_verdict(store)
            await store.record_verdict(
                DedupKey("k1"), plain(summary="c"), role="judge", prompt_version=1, model="haiku", fidelity="full"
            )
            return {"after_summary": after_summary, "after_full": one_verdict(store)}

    result = asyncio.run(go())
    assert result["after_summary"] == {"n": 1, "model": "sonnet", "summary": "a", "ck": None, "fidelity": "summary"}
    assert result["after_full"] == {"n": 1, "model": "haiku", "summary": "c", "ck": None, "fidelity": "full"}


def test_cross_model_summary_to_full_upgrade_updates_model_and_canonical_key(tmp_path: Path) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "t"))
            await store.record_verdict(
                DedupKey("k1"),
                plain(summary="preview", canonical_key="old-rule"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="summary",
            )
            await store.record_verdict(
                DedupKey("k1"),
                plain(summary="hydrated", canonical_key="new-rule"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            return one_verdict(store)

    assert asyncio.run(go()) == {"n": 1, "model": "opus", "summary": "hydrated", "ck": "new-rule", "fidelity": "full"}


def test_different_model_full_to_full_is_a_noop(tmp_path: Path) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "t"))
            await store.record_verdict(
                DedupKey("k1"),
                plain(summary="first", canonical_key="first-rule"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
            await store.record_verdict(
                DedupKey("k1"),
                plain(summary="second", canonical_key="second-rule"),
                role="judge",
                prompt_version=1,
                model="opus",
                fidelity="full",
            )
            return one_verdict(store)

    assert asyncio.run(go()) == {"n": 1, "model": "sonnet", "summary": "first", "ck": "first-rule", "fidelity": "full"}


def test_canonical_key_roundtrips_including_none(tmp_path: Path) -> None:
    async def go() -> list[tuple[object, object]]:
        with open_generic(tmp_path) as store:
            for key in ("k1", "k2"):
                store.store.conn.execute(
                    INSERT_EVENT, (key, "transcript_message", "2026-01-01T00:00:00+00:00", "t")
                )
            await store.record_verdict(
                DedupKey("k1"),
                plain(canonical_key="use-uv-not-pip"),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
            await store.record_verdict(
                DedupKey("k2"),
                plain(canonical_key=None),
                role="judge",
                prompt_version=1,
                model="sonnet",
                fidelity="full",
            )
            cur = store.store.conn.execute("SELECT dedup_key, canonical_key FROM verdicts ORDER BY dedup_key")
            return [(row["dedup_key"], row["canonical_key"]) for row in cur]

    assert asyncio.run(go()) == [("k1", "use-uv-not-pip"), ("k2", None)]


def test_unmigrated_v8_verdict_table_fails_loud_on_every_path(tmp_path: Path) -> None:
    async def go() -> None:
        db = FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + V8_VERDICT_DDL)
        with GenericStore(db) as store:
            store.store.conn.execute(INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "t"))
            with pytest.raises(VerdictSchemaError, match="v8-to-v9"):
                store.judged(role="judge", prompt_version=1)
            with pytest.raises(VerdictSchemaError, match="v8-to-v9"):
                await store.unjudged(role="judge", prompt_version=1)
            with pytest.raises(VerdictSchemaError, match="v8-to-v9"):
                await store.record_verdict(
                    DedupKey("k1"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="full"
                )

    asyncio.run(go())


def test_fresh_v9_verdict_table_passes_every_path(tmp_path: Path) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(INSERT_EVENT, ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "t"))
            unjudged = [row["dedup_key"] for row in await store.unjudged(role="judge", prompt_version=1)]
            await store.record_verdict(
                DedupKey("k1"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="full"
            )
            return {
                "unjudged": unjudged,
                "judged": [row["dedup_key"] for row in store.judged(role="judge", prompt_version=1)],
            }

    assert asyncio.run(go()) == {"unjudged": ["k1"], "judged": ["k1"]}


def test_legacy_aliased_v9_table_passes_schema_validation(tmp_path: Path) -> None:
    def go() -> None:
        db = FileStateStore.open(tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + LegacyStore.verdicts_ddl())
        with LegacyStore(db) as store:
            store.ensure_verdict_schema()

    go()


def test_second_verdict_table_on_a_shared_connection_still_fails_loud(tmp_path: Path) -> None:
    def go() -> None:
        db = FileStateStore.open(
            tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + GenericStore.verdicts_ddl() + V8_TRIAGE_DDL
        )
        with db:
            GenericStore(db).ensure_verdict_schema()
            with pytest.raises(VerdictSchemaError, match="v8-to-v9"):
                LegacyStore(db).ensure_verdict_schema()

    go()


def test_unjudged_orders_truly_unjudged_before_summary_refresh(tmp_path: Path, live_transcript: None) -> None:
    async def go() -> list[str]:
        with open_generic(tmp_path) as store:
            for i, key in enumerate(("k1", "k2", "k3")):
                store.store.conn.execute(
                    INSERT_EVENT_CONTEXT,
                    (key, "transcript_message", f"2026-01-0{i + 1}T00:00:00+00:00", "t", window_json()),
                )
            await store.record_verdict(
                DedupKey("k1"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            rows = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            return [str(row["dedup_key"]) for row in rows]

    assert asyncio.run(go()) == ["k2", "k3", "k1"]


def test_unjudged_keeps_a_summary_row_while_its_transcript_lives(tmp_path: Path, live_transcript: None) -> None:
    async def go() -> list[str]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(
                INSERT_EVENT_CONTEXT, ("live", "transcript_message", "2026-01-01T00:00:00+00:00", "t", window_json())
            )
            await store.record_verdict(
                DedupKey("live"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            rows = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            return [str(row["dedup_key"]) for row in rows]

    assert asyncio.run(go()) == ["live"]


def test_unjudged_drops_a_summary_row_whose_transcript_expired(tmp_path: Path, dead_transcript: None) -> None:
    # The row carries the populated refs capture_window always produces; only the
    # transcript is gone. The fence must hydrate to notice — refs alone never do.
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(
                INSERT_EVENT_CONTEXT, ("dead", "transcript_message", "2026-01-01T00:00:00+00:00", "t", window_json())
            )
            await store.record_verdict(
                DedupKey("dead"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            return {
                "refresh": [
                    str(row["dedup_key"])
                    for row in await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
                ],
                "plain": [str(row["dedup_key"]) for row in await store.unjudged(role="judge", prompt_version=1)],
            }

    result = asyncio.run(go())
    assert result["refresh"] == []
    assert result["plain"] == []


def test_unjudged_keeps_an_unjudged_event_then_drops_it_once_its_dead_transcript_is_verdicted(
    tmp_path: Path, dead_transcript: None
) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(
                INSERT_EVENT_CONTEXT, ("dead", "transcript_message", "2026-01-01T00:00:00+00:00", "t", window_json())
            )
            before = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            await store.record_verdict(
                DedupKey("dead"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            after = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            return {
                "before": [str(row["dedup_key"]) for row in before],
                "after": [str(row["dedup_key"]) for row in after],
            }

    result = asyncio.run(go())
    assert result["before"] == ["dead"]
    assert result["after"] == []


def test_unjudged_probe_hydration_false_skips_the_transcript_scan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def boom(*_: object, **__: object) -> None:
        raise AssertionError("probe_hydration=False must not scan for transcripts")

    monkeypatch.setattr("cc_transcript.activity.find_transcript", boom)

    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            for i, key in enumerate(("fresh", "summ")):
                store.store.conn.execute(
                    INSERT_EVENT_CONTEXT,
                    (key, "transcript_message", f"2026-01-0{i + 1}T00:00:00+00:00", "t", window_json()),
                )
            await store.record_verdict(
                DedupKey("summ"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            rows = await store.unjudged(
                role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False
            )
            return {
                "keys": [str(row["dedup_key"]) for row in rows],
                "has_verdict_id": any("verdict_id" in row for row in rows),
            }

    result = asyncio.run(go())
    assert result["keys"] == ["fresh", "summ"]
    assert result["has_verdict_id"] is False


def test_unjudged_probe_hydration_false_keeps_a_dead_transcript_row(
    tmp_path: Path, dead_transcript: None
) -> None:
    async def go() -> dict[str, object]:
        with open_generic(tmp_path) as store:
            store.store.conn.execute(
                INSERT_EVENT_CONTEXT, ("dead", "transcript_message", "2026-01-01T00:00:00+00:00", "t", window_json())
            )
            await store.record_verdict(
                DedupKey("dead"), plain(), role="judge", prompt_version=1, model="sonnet", fidelity="summary"
            )
            return {
                "probed": [
                    str(row["dedup_key"])
                    for row in await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
                ],
                "unprobed": [
                    str(row["dedup_key"])
                    for row in await store.unjudged(
                        role="judge", prompt_version=1, refresh_summary=True, probe_hydration=False
                    )
                ],
            }

    result = asyncio.run(go())
    assert result["probed"] == []
    assert result["unprobed"] == ["dead"]


def test_run_verdicts_persists_each_verdict_and_skips_failed_rows() -> None:
    rows = [{"dedup_key": f"k{i}", "text": f"t{i}"} for i in range(4)]
    persisted: list[tuple[str, str]] = []

    async def judge(prompt: str) -> str:
        if prompt == "prompt:t2":
            raise JudgeError("boom")
        return prompt.upper()

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        persisted.append((str(row["dedup_key"]), verdict))

    async def prompt_for(row: Mapping[str, object]) -> str:
        return f"prompt:{row['text']}"

    async def go() -> tuple[int, int]:
        return await run_verdicts(rows, prompt_for, judge, persist, concurrency=2)

    assert asyncio.run(go()) == (3, 1)
    assert sorted(persisted) == [("k0", "PROMPT:T0"), ("k1", "PROMPT:T1"), ("k3", "PROMPT:T3")]


def test_run_verdicts_propagates_programming_errors() -> None:
    rows = [{"dedup_key": f"k{i}", "text": f"t{i}"} for i in range(3)]

    async def judge(prompt: str) -> str:
        if prompt == "prompt:t1":
            raise TypeError("bug in the worker body")
        return prompt.upper()

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        del row, verdict

    async def prompt_for(row: Mapping[str, object]) -> str:
        return f"prompt:{row['text']}"

    async def go() -> tuple[int, int]:
        return await run_verdicts(rows, prompt_for, judge, persist, concurrency=2)

    with pytest.raises(ExceptionGroup) as excinfo:
        asyncio.run(go())
    assert excinfo.group_contains(TypeError, match="bug in the worker body")


def test_structured_judge_wraps_a_non_json_response_as_judge_error(monkeypatch: pytest.MonkeyPatch) -> None:
    pytest.importorskip("spawnllm")

    from pydantic import BaseModel

    from cc_transcript.judge import llm

    class Verdict(BaseModel):
        ok: bool

    async def garbage_extract(*_: object, **__: object) -> Verdict:
        from spawnllm.structured import structured_value

        structured_value("<< not the JSON the judge asked for >>")
        raise AssertionError("spawnllm must reject a non-JSON response before returning")

    monkeypatch.setattr(llm, "default_backend", lambda: object())
    monkeypatch.setattr("spawnllm.extract", garbage_extract)

    async def go() -> None:
        judge = llm.structured_judge(Verdict, tier="medium")
        with pytest.raises(JudgeError):
            await judge("prompt")

    asyncio.run(go())


def test_run_verdicts_respects_concurrency() -> None:
    state = {"active": 0, "peak": 0}

    async def judge(prompt: str) -> str:
        state["active"] += 1
        state["peak"] = max(state["peak"], state["active"])
        await asyncio.sleep(0.01)
        state["active"] -= 1
        return prompt

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        del row, verdict

    async def prompt_for(row: Mapping[str, object]) -> str:
        return str(row["dedup_key"])

    async def go() -> tuple[int, int]:
        rows = [{"dedup_key": str(i)} for i in range(8)]
        return await run_verdicts(rows, prompt_for, judge, persist, concurrency=2)

    assert asyncio.run(go()) == (8, 0)
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


def test_refresh_summary_re_yields_summary_rows_until_a_full_verdict_replaces(
    tmp_path: Path, live_transcript: None
) -> None:
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
        db = FileStateStore.open(
            tmp_path / "feedback.db", extra_schema=FEEDBACK_DDL + GenericStore.verdicts_ddl()
        )
        with GenericStore(db) as store:
            store.store.conn.execute(
                INSERT_EVENT_CONTEXT,
                ("k1", "transcript_message", "2026-01-01T00:00:00+00:00", "text k1", window_json()),
            )
            await record(store, summary_verdict, "summary")
            plain = await store.unjudged(role="judge", prompt_version=1)
            refresh = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            await record(store, full_verdict, "full")
            after_full = await store.unjudged(role="judge", prompt_version=1, refresh_summary=True)
            await record(store, summary_verdict, "summary")
            return {
                "plain": plain,
                "refresh": [row["dedup_key"] for row in refresh],
                "after_full": after_full,
                "judged": store.judged(role="judge", prompt_version=1),
            }

    result = asyncio.run(go())
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
        await asyncio.sleep(0)
        return f"prompt:{row['text']}"

    async def judge(prompt: str) -> str:
        return prompt.upper()

    async def persist(row: Mapping[str, object], verdict: str) -> None:
        persisted.append((str(row["dedup_key"]), verdict))

    async def go() -> tuple[int, int]:
        return await run_verdicts(rows, prompt_for, judge, persist, concurrency=2)

    assert asyncio.run(go()) == (3, 0)
    assert sorted(persisted) == [("k0", "PROMPT:T0"), ("k1", "PROMPT:T1"), ("k2", "PROMPT:T2")]


def test_resolved_model_matches_the_backend_table() -> None:
    pytest.importorskip("spawnllm", reason="[llm] extra not installed")
    from spawnllm import BackendUnavailable

    from cc_transcript.judge.llm import default_backend, resolved_model

    try:
        backend = default_backend()
    except BackendUnavailable:
        pytest.skip("no spawnllm backend available")

    assert resolved_model("medium") == backend.models["medium"]


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


def test_structured_judge_converts_provider_errors_to_judge_error(monkeypatch: pytest.MonkeyPatch) -> None:
    pytest.importorskip("spawnllm", reason="[llm] extra not installed")
    from pydantic import BaseModel
    from spawnllm import BackendCallError

    from cc_transcript.judge.llm import structured_judge

    class Toy(BaseModel):
        value: int

    async def failing_extract(*args: object, **kwargs: object) -> Toy:
        raise BackendCallError("claude exited 1: overloaded")

    monkeypatch.setattr("spawnllm.extract", failing_extract)
    monkeypatch.setattr("cc_transcript.judge.llm.default_backend", lambda: object())
    judge = structured_judge(Toy, tier="medium", timeout=1)
    with pytest.raises(JudgeError, match="overloaded"):
        asyncio.run(judge("prompt"))


@pytest.mark.parametrize(
    ("text", "matches"),
    [
        pytest.param("use-uv", True, id="two-groups"),
        pytest.param("use-uv-not-pip-here-now", True, id="six-groups"),
        pytest.param("word", False, id="one-group-rejected"),
        pytest.param("a-b-c-d-e-f-g", False, id="seven-groups-rejected"),
        pytest.param("a" * 64, False, id="sixty-four-hex-rejected"),
        pytest.param("Use-Uv", False, id="uppercase-rejected"),
        pytest.param("use--uv", False, id="double-hyphen-rejected"),
    ],
)
def test_slug_pattern_edges(text: str, matches: bool) -> None:
    assert bool(SLUG_PATTERN.match(text)) is matches


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        pytest.param("Use UV, not pip!", "use-uv-not-pip", id="punctuation-and-case"),
        pytest.param("  never   force_push  ", "never-force-push", id="whitespace-and-underscore"),
        pytest.param("already-a-slug", "already-a-slug", id="idempotent"),
        pytest.param("!!!", "", id="all-punctuation-empties"),
    ],
)
def test_canonical_slug_normalizes(text: str, expected: str) -> None:
    assert canonical_slug(text) == expected


def test_canonical_slug_output_matches_slug_pattern() -> None:
    slug = canonical_slug("Use UV, not pip!")
    assert SLUG_PATTERN.match(slug)
