"""Generic LLM verdict passes over mined feedback: storage, fan-out, sampling, eval math.

The mechanism proven in cc-pushback's triage pipeline, lifted app-free: a verdict
table layered on :class:`FeedbackStore`'s event ledger, the asyncio fan-out that
runs a judge over rows, the seeded stratified audit sampler, and the mechanical eval
math (golden gate, exact Clopper-Pearson bounds, flip tracking). Apps own the
prompts, the verdict model, and any SQL views over the verdict table.
"""

from __future__ import annotations

import asyncio
import re
from dataclasses import dataclass
from math import comb
from random import Random
from typing import TYPE_CHECKING, ClassVar, Protocol
from weakref import WeakKeyDictionary

from cc_transcript.mining.store import now

if TYPE_CHECKING:
    import sqlite3
    from collections.abc import Awaitable, Callable, Mapping, Sequence

    from cc_transcript.context import Fidelity
    from cc_transcript.mining.candidates import DedupKey
    from cc_transcript.store import FileStateStore

EVENT_COLUMNS = (
    "e.id, e.dedup_key, e.source_kind, e.occurred_at, e.text, "
    "e.payload_json, e.context_json, e.session_id, e.event_uuid"
)

VERDICT_DDL_TEMPLATE = """
CREATE TABLE IF NOT EXISTS {table} (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  dedup_key TEXT NOT NULL REFERENCES feedback_events(dedup_key),
  role TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  category TEXT NOT NULL,
  {accepted} INTEGER NOT NULL,
  {summary} TEXT NOT NULL,
  confidence REAL NOT NULL,
  rationale TEXT NOT NULL,
  canonical_key TEXT,
  fidelity TEXT NOT NULL CHECK(fidelity IN ('full','summary')),
  judged_at TEXT NOT NULL,
  UNIQUE(dedup_key, role, prompt_version)
);
CREATE INDEX IF NOT EXISTS idx_{table}_dedup ON {table}(dedup_key);
"""

SLUG_PATTERN = re.compile(r"^[a-z0-9]+(-[a-z0-9]+){1,5}$")
"""Matches a durable rule slug: two to six ``[a-z0-9]`` groups joined by single hyphens.

The two-group floor rejects a bare word, and the six-group ceiling — together
with the hyphen requirement — can never match a 64-character hex digest, so a
slug and a content digest never collide.
"""

VALIDATED_VERDICT_TABLES: WeakKeyDictionary[sqlite3.Connection, set[str]] = WeakKeyDictionary()
"""Per-connection sets of verdict table names that passed :meth:`VerdictStoreMixin.ensure_verdict_schema`.

A table's schema is fixed for a connection's lifetime, so the v8/v9 check runs
once per table per connection; membership short-circuits the repeat calls from
every :meth:`~VerdictStoreMixin.judged`, :meth:`~VerdictStoreMixin.unjudged`, and
:meth:`~VerdictStoreMixin.record_verdict`. Scoped by table, not connection alone,
because the mixin is explicitly multi-table — two subclasses with different
``VERDICT_TABLE`` may share one connection, and validating one must not vouch for
the other.
"""


class VerdictSchemaError(RuntimeError):
    """The verdict table predates the v9 schema and must be rebuilt by hand.

    Raised by :class:`VerdictStoreMixin`'s read and write paths when the physical
    table lacks the ``canonical_key`` column or its unique index still covers the
    v8 identity ``(dedup_key, role, prompt_version, model)``. ``CREATE TABLE IF
    NOT EXISTS`` leaves such a table untouched, and :meth:`~VerdictStoreMixin.judged`
    neither selects ``canonical_key`` nor joins on ``model``, so a v8 table would
    otherwise read without error while silently returning per-model duplicates.
    """


class JudgeError(RuntimeError):
    """A provider or transport failure from one judge call.

    The boundary that talks to the LLM provider (the callable returned by
    :func:`~cc_transcript.judge.llm.structured_judge`) converts backend, timeout,
    and response-validation failures into this type. :func:`run_verdicts` catches
    exactly this: the row is counted as failed and left unpersisted, so the next
    pass retries it. Anything else raised inside a judge call is a programming
    error and propagates.
    """


class VerdictLike(Protocol):
    """The structural shape a judge's verdict must expose to be persisted.

    Read-only by design, so both plain attributes and property aliases satisfy
    it — an app whose model names the bits differently (cc-pushback's
    ``is_pushback``/``what_claude_did``) adapts with two properties.

    Attributes:
        category: The single best-fitting category label.
        summary: One neutral sentence summarizing the judged action.
        confidence: The judge's probability that its accept-vs-reject call is right.
        rationale: One short clause explaining the call.
        accepted: Whether the verdict accepts the row.
        canonical_key: The canonical, normalized key of the durable rule the
            verdict names, or ``None`` when the verdict names no durable rule.
    """

    @property
    def category(self) -> str: ...
    @property
    def summary(self) -> str: ...
    @property
    def confidence(self) -> float: ...
    @property
    def rationale(self) -> str: ...
    @property
    def accepted(self) -> bool: ...
    @property
    def canonical_key(self) -> str | None: ...


class VerdictStoreMixin:
    """Verdict persistence layered on :class:`FeedbackStore`'s event table.

    Mix into a :class:`FeedbackStore` subclass and include :meth:`verdicts_ddl`
    in the schema passed to ``FileStateStore.open``. The class-level params name
    the physical table and columns; :meth:`judged` aliases them back to the
    generic ``accepted``/``summary``, so the sampling and eval math read one row
    shape. An app with an existing verdict table (cc-pushback's ``triage``)
    adopts the mixin by overriding the params — zero migration.

    Verdict identity is ``(dedup_key, role, prompt_version)``: one verdict per
    event per role per prompt version. ``model`` is provenance only — recorded
    and reported, never filtered on — so a judge-backend flip cannot mass-re-judge
    the corpus and :meth:`judged`'s consumers never double-count per-model
    duplicates.

    Example:
        >>> class Store(VerdictStoreMixin, FeedbackStore):
        ...     VERDICT_TABLE = "triage"
        ...     ACCEPTED_COLUMN = "is_pushback"
        ...     SUMMARY_COLUMN = "what_claude_did"
    """

    VERDICT_TABLE: ClassVar[str] = "verdicts"
    ACCEPTED_COLUMN: ClassVar[str] = "accepted"
    SUMMARY_COLUMN: ClassVar[str] = "summary"
    store: FileStateStore

    @classmethod
    def verdicts_ddl(cls) -> str:
        """Returns the verdict table and index DDL under the class's name params."""
        return VERDICT_DDL_TEMPLATE.format(
            table=cls.VERDICT_TABLE, accepted=cls.ACCEPTED_COLUMN, summary=cls.SUMMARY_COLUMN
        )

    def ensure_verdict_schema(self) -> None:
        """Validates the verdict table matches the v9 schema, once per table per connection.

        ``CREATE TABLE IF NOT EXISTS`` leaves an existing v8 table (no
        ``canonical_key`` column, unique index over ``(dedup_key, role,
        prompt_version, model)``) in place, and neither :meth:`judged` nor
        :meth:`unjudged` selects ``canonical_key`` or joins on ``model`` — so a v8
        table reads without error while returning per-model duplicates. The read
        and write paths call this first so the mismatch fails loud instead.

        Raises:
            VerdictSchemaError: When the table lacks a ``canonical_key`` column or
                no unique index covers exactly ``(dedup_key, role, prompt_version)``.
        """
        if self.VERDICT_TABLE in VALIDATED_VERDICT_TABLES.setdefault(self.store.conn, set()):
            return
        columns = {row["name"] for row in self.store.conn.execute(f"PRAGMA table_info({self.VERDICT_TABLE})")}
        indexes = [dict(row) for row in self.store.conn.execute(f"PRAGMA index_list({self.VERDICT_TABLE})")]
        unique_columns = [
            tuple(col["name"] for col in self.store.conn.execute(f"PRAGMA index_info({index['name']})"))
            for index in indexes
            if index["unique"]
        ]
        if "canonical_key" not in columns or ("dedup_key", "role", "prompt_version") not in unique_columns:
            raise VerdictSchemaError(
                f"verdict table {self.VERDICT_TABLE!r} predates the v9 schema: it needs a canonical_key column and a "
                f"UNIQUE(dedup_key, role, prompt_version) index. Rebuild it with the manual v8-to-v9 migration "
                f"(recreate {self.VERDICT_TABLE!r} from verdicts_ddl() and copy the rows over) before reading "
                f"or writing."
            )
        VALIDATED_VERDICT_TABLES[self.store.conn].add(self.VERDICT_TABLE)

    async def record_verdict(
        self, key: DedupKey, verdict: VerdictLike, *, role: str, prompt_version: int, model: str, fidelity: Fidelity
    ) -> None:
        """Records one verdict, idempotently, keyed by ``(dedup_key, role, prompt_version)``.

        One verdict per event per role per prompt version — ``model`` is pure
        provenance, recorded and reported but never part of the identity, so a
        judge-backend flip never mass-re-judges the corpus. Re-recording is a
        no-op, with one exception: a ``'full'``-fidelity verdict replaces a
        ``'summary'`` one at the same key (any model), carrying the new model,
        content, and ``canonical_key`` across — the re-judge path
        :meth:`unjudged` opens with ``refresh_summary=True``. A different-model
        ``'full'``-to-``'full'`` re-record is a deliberate silent no-op:
        first-full-wins.

        Args:
            key: The judged event's dedup key.
            verdict: The structured verdict to persist.
            role: Who produced it, e.g. ``judge`` or ``auditor``.
            prompt_version: The prompt version that produced it.
            model: The resolved model name that produced it, kept as provenance.
            fidelity: Whether the judged window rendered at ``'full'`` fidelity
                or from ``'summary'`` previews.
        """
        from cc_transcript.judge.similar import (
            clear_evidence,
            embed_evidence,
            prepare_evidence_removal,
            record_evidence,
        )

        self.ensure_verdict_schema()
        evidence = (
            await embed_evidence(
                self.store, dedup_key=key, canonical_key=verdict.canonical_key, summary=verdict.summary
            )
            if verdict.canonical_key is not None
            else None
        )
        removable = evidence is None and prepare_evidence_removal(self.store)
        with self.store.transaction() as conn:
            cursor = conn.execute(
                f"INSERT INTO {self.VERDICT_TABLE} ("
                f"dedup_key, role, prompt_version, model, category, {self.ACCEPTED_COLUMN}, "
                f"{self.SUMMARY_COLUMN}, confidence, rationale, canonical_key, fidelity, judged_at"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) "
                "ON CONFLICT(dedup_key, role, prompt_version) DO UPDATE SET "
                f"model = excluded.model, category = excluded.category, "
                f"{self.ACCEPTED_COLUMN} = excluded.{self.ACCEPTED_COLUMN}, "
                f"{self.SUMMARY_COLUMN} = excluded.{self.SUMMARY_COLUMN}, confidence = excluded.confidence, "
                "rationale = excluded.rationale, canonical_key = excluded.canonical_key, "
                "fidelity = excluded.fidelity, judged_at = excluded.judged_at "
                "WHERE fidelity = 'summary' AND excluded.fidelity = 'full'",
                (
                    key,
                    role,
                    prompt_version,
                    model,
                    verdict.category,
                    verdict.accepted,
                    verdict.summary,
                    verdict.confidence,
                    verdict.rationale,
                    verdict.canonical_key,
                    fidelity,
                    now(),
                ),
            )
            if cursor.rowcount > 0:
                if evidence is not None:
                    record_evidence(conn, dedup_key=key, role=role, prompt_version=prompt_version, evidence=evidence)
                elif removable:
                    clear_evidence(conn, dedup_key=key, role=role, prompt_version=prompt_version)

    def unjudged(
        self,
        *,
        role: str,
        prompt_version: int,
        limit: int | None = None,
        refresh_summary: bool = False,
        probe_hydration: bool = True,
    ) -> list[dict[str, object]]:
        """Returns events lacking a verdict for ``(role, prompt_version)``, unjudged first.

        Truly-unjudged events (no verdict at all) sort ahead of summary-refresh
        rows, then by event id, so a backlog of summary rows never starves fresh
        events under ``limit``.

        Args:
            role: The verdict role to check.
            prompt_version: The prompt version the verdict must carry.
            limit: When set, the maximum number of rows to return.
            refresh_summary: When True, also re-yields events whose verdict was
                recorded at ``fidelity='summary'``, so they can be re-judged at
                full fidelity once their windows hydrate again. A summary row
                whose context window no longer hydrates — its transcript expired
                or a ref was compacted away — is dropped, so a dead transcript
                stops burning the cap (unless ``probe_hydration`` is False).
            probe_hydration: When True (default), each summary-refresh row is
                probed with a transcript-discovery hydration check and dropped
                when its window no longer hydrates. When False, that per-row
                probe — and the recursive transcript scan behind it — is skipped
                entirely: every summary-refresh row is returned unprobed, so some
                may name a transcript that is no longer discoverable. Only the
                ``refresh_summary`` path probes, so this flag is a no-op
                otherwise. The default preserves the probing behavior exactly.

        Returns:
            One dict per event with the columns needed to build its prompt.
        """
        self.ensure_verdict_schema()
        if not refresh_summary:
            sql = (
                f"SELECT {EVENT_COLUMNS} FROM feedback_events e "
                f"LEFT JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
                "AND t.role = ? AND t.prompt_version = ? "
                "WHERE t.id IS NULL ORDER BY e.id"
            )
            params: tuple[object, ...] = (role, prompt_version)
            if limit is not None:
                sql += " LIMIT ?"
                params = (*params, limit)
            return [dict(row) for row in self.store.conn.execute(sql, params)]
        candidates = [
            dict(raw)
            for raw in self.store.conn.execute(
                f"SELECT {EVENT_COLUMNS}, t.id AS verdict_id FROM feedback_events e "
                f"LEFT JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
                "AND t.role = ? AND t.prompt_version = ? "
                "WHERE (t.id IS NULL OR t.fidelity = 'summary') ORDER BY (t.id IS NOT NULL), e.id",
                (role, prompt_version),
            )
        ]
        kept: list[dict[str, object]] = []
        for row in candidates:
            fresh = row.pop("verdict_id") is None
            if not probe_hydration or fresh or hydratable(str(row["context_json"])):
                kept.append(row)
                if limit is not None and len(kept) >= limit:
                    break
        return kept

    def judged(self, *, role: str, prompt_version: int) -> list[dict[str, object]]:
        """Returns events joined with their ``(role, prompt_version)`` verdicts, oldest first.

        The physical accepted/summary columns are aliased to the generic
        ``accepted`` and ``summary`` keys, whatever the class params name them.

        Args:
            role: The verdict role to join.
            prompt_version: The prompt version to join.

        Returns:
            One dict per verdict-bearing event: the event columns plus the
            verdict's ``category``, ``accepted``, ``confidence``, ``summary``,
            ``rationale``, and ``model``.
        """
        self.ensure_verdict_schema()
        cur = self.store.conn.execute(
            f"SELECT {EVENT_COLUMNS}, t.category, t.{self.ACCEPTED_COLUMN} AS accepted, t.confidence, "
            f"t.{self.SUMMARY_COLUMN} AS summary, t.rationale, t.model "
            f"FROM feedback_events e JOIN {self.VERDICT_TABLE} t ON t.dedup_key = e.dedup_key "
            "WHERE t.role = ? AND t.prompt_version = ? ORDER BY e.id",
            (role, prompt_version),
        )
        return [dict(row) for row in cur]

    def dedup_keys(self) -> set[str]:
        """Returns every stored event's dedup key."""
        return {str(row["dedup_key"]) for row in self.store.conn.execute("SELECT dedup_key FROM feedback_events")}


@dataclass(frozen=True, slots=True)
class AuditSample:
    """The seeded audit draw over one prompt version's judged rows.

    Attributes:
        core: Uniform draws — the only rows entering headline precision metrics.
        oversample: Lowest-judge-confidence draws — diagnosis fuel only.
    """

    core: tuple[Mapping[str, object], ...]
    oversample: tuple[Mapping[str, object], ...]


@dataclass(frozen=True, slots=True)
class AuditEstimate:
    """One binomial estimate from audited rows.

    Attributes:
        audited: How many rows of the population carry an auditor verdict.
        hits: How many of those the auditor accepted.
    """

    audited: int
    hits: int

    @property
    def rate(self) -> float | None:
        """``hits / audited``, or ``None`` when nothing is audited."""
        return self.hits / self.audited if self.audited else None


@dataclass(frozen=True, slots=True)
class GoldenRow:
    """One frozen, hand-labeled row of the golden regression set.

    Attributes:
        dedup_key: The content-derived key joining the row to ``feedback_events``.
        source_kind: The detector that produced the row.
        text: The verbatim message, kept for human review of the fixture.
        expected: The frozen accepted-vs-rejected label.
        note: One clause recording why the label holds.
    """

    dedup_key: str
    source_kind: str
    text: str
    expected: bool
    note: str


@dataclass(frozen=True, slots=True)
class GoldenFailure:
    """One golden row the judge got wrong (or has not judged).

    Attributes:
        dedup_key: The failing row's key.
        expected: The frozen label.
        category: The judge's category, or ``None`` when the row is unjudged.
        rationale: The judge's stated reason, or ``None`` when the row is unjudged.
        text: The verbatim message.
    """

    dedup_key: str
    expected: bool
    category: str | None
    rationale: str | None
    text: str


@dataclass(frozen=True, slots=True)
class GoldenResult:
    """The golden-set gate outcome.

    Attributes:
        total: The fixture's row count.
        passed: How many rows the judge labeled to match the fixture.
        sha256: The fixture file's digest, printed so any edit is visible.
        failures: Every mismatched or unjudged row.
    """

    total: int
    passed: int
    sha256: str
    failures: tuple[GoldenFailure, ...]


@dataclass(frozen=True, slots=True)
class Disagreement:
    """One audited row where the auditor's side differs from the judge's.

    Attributes:
        dedup_key: The row's key.
        source_kind: The detector that produced the row.
        text: The verbatim message.
        judge_category: The judge's category.
        auditor_category: The auditor's category.
        judge_rationale: The judge's stated reason.
        auditor_rationale: The auditor's stated reason.
    """

    dedup_key: str
    source_kind: str
    text: str
    judge_category: str
    auditor_category: str
    judge_rationale: str
    auditor_rationale: str


@dataclass(frozen=True, slots=True)
class Metrics:
    """The full mechanical evaluation of one prompt version.

    Attributes:
        prompt_version: The judge prompt version evaluated.
        total: The corpus row count.
        judged: How many rows carry a judge verdict at this version.
        accepted: How many of those are accepted.
        golden: The golden-set gate outcome.
        core_accepts: Audited precision numerator/denominator over the uniform core.
        core_rejects: Audited contamination numerator/denominator over the uniform core.
        pool_accepts: The same estimate over every audited accept (cumulative pool).
        pool_rejects: The same estimate over every audited reject (cumulative pool).
        by_kind: ``(judged, accepted)`` counts per source kind, descriptive only.
        disagreements: Every audited row where auditor and judge disagree.
    """

    prompt_version: int
    total: int
    judged: int
    accepted: int
    golden: GoldenResult
    core_accepts: AuditEstimate
    core_rejects: AuditEstimate
    pool_accepts: AuditEstimate
    pool_rejects: AuditEstimate
    by_kind: Mapping[str, tuple[int, int]]
    disagreements: tuple[Disagreement, ...]

    @property
    def precision(self) -> float | None:
        """Audited precision over the uniform core's accepts."""
        return self.core_accepts.rate

    @property
    def contamination(self) -> float | None:
        """Audited genuine-accept rate over the uniform core's rejects."""
        return self.core_rejects.rate

    @property
    def contamination_upper(self) -> float | None:
        """The exact one-sided 95% upper bound on contamination."""
        est = self.core_rejects
        return exact_upper_bound(est.hits, est.audited) if est.audited else None

    @property
    def recall_hat(self) -> float | None:
        """The derived estimate of the fraction of genuine accepts accepted."""
        match (self.precision, self.contamination):
            case (None, _) | (_, None):
                return None
            case (p, c):
                rejected = self.judged - self.accepted
                genuine = self.accepted * p + rejected * c
                return self.accepted * p / genuine if genuine else None


@dataclass(frozen=True, slots=True)
class Flip:
    """One row whose accepted-vs-rejected side changed between verdict passes.

    Attributes:
        dedup_key: The row's key.
        text: The verbatim message.
        from_category: The category at the earlier pass.
        to_category: The category at the later pass.
    """

    dedup_key: str
    text: str
    from_category: str
    to_category: str


@dataclass(frozen=True, slots=True)
class FlipReport:
    """The verdict churn between two passes.

    Attributes:
        common: How many rows carry verdicts at both passes.
        flips: Every row whose side changed.
    """

    common: int
    flips: tuple[Flip, ...]

    @property
    def rate(self) -> float | None:
        """``len(flips) / common``, or ``None`` when no rows overlap."""
        return len(self.flips) / self.common if self.common else None


def canonical_slug(text: str) -> str:
    """Normalizes free text into a hyphenated slug candidate.

    Lowercases ``text`` and reduces every run of non-alphanumeric characters to a
    single hyphen, trimming leading and trailing hyphens — so a judge can turn a
    durable-rule phrase into a stable :data:`SLUG_PATTERN` ``canonical_key``.

    Example:
        >>> canonical_slug("Use UV, not pip!")
        'use-uv-not-pip'
    """
    return re.sub(r"[^a-z0-9]+", "-", text.strip().lower()).strip("-")


def hydratable(context_json: str) -> bool:
    from cc_transcript.context import ContextWindow

    return ContextWindow.from_json(context_json).hydrate() is not None


async def run_verdicts[V](
    rows: Sequence[Mapping[str, object]],
    prompt_for: Callable[[Mapping[str, object]], Awaitable[str]],
    judge: Callable[[str], Awaitable[V]],
    persist: Callable[[Mapping[str, object], V], Awaitable[None]],
    *,
    concurrency: int,
) -> tuple[int, int]:
    """Runs ``judge`` over every row's prompt and persists each verdict as it lands.

    Incremental by construction: a row whose judge call raises :class:`JudgeError`
    is counted as failed and left unpersisted, so the next pass retries it; every
    other row's verdict persists as soon as its call completes. Any other
    exception is a programming error and propagates, cancelling the pass. Generic
    over the verdict payload, so the same fan-out serves triage- and
    refinement-shaped passes.

    Args:
        rows: The rows to judge.
        prompt_for: Builds one row's prompt; async, so prompts may hydrate their
            context window first.
        judge: Turns one prompt into a verdict payload, e.g. ``structured_judge(...)``.
        persist: Persists one row's verdict, e.g. ``store.record_verdict`` applied.
        concurrency: The maximum number of concurrent judge calls.

    Returns:
        The pass's ``(judged, failed)`` counts.
    """
    counts = {"judged": 0, "failed": 0}
    limiter = asyncio.Semaphore(concurrency)

    async def worker(row: Mapping[str, object]) -> None:
        async with limiter:
            try:
                verdict = await judge(await prompt_for(row))
            except JudgeError:
                counts["failed"] += 1
                return
        await persist(row, verdict)
        counts["judged"] += 1

    async with asyncio.TaskGroup() as tg:
        for row in rows:
            tg.create_task(worker(row))
    return counts["judged"], counts["failed"]


def draw(
    group: Sequence[Mapping[str, object]], k: int, rng: Random, oversample_share: float
) -> tuple[list[Mapping[str, object]], list[Mapping[str, object]]]:
    if k >= len(group):
        return list(group), []
    n_over = round(k * oversample_share)
    over = sorted(group, key=lambda r: (float(str(r["confidence"])), str(r["dedup_key"])))[:n_over]
    over_keys = {str(r["dedup_key"]) for r in over}
    pool = [r for r in group if str(r["dedup_key"]) not in over_keys]
    return rng.sample(pool, k - n_over), over


def stratified(
    rows: Sequence[Mapping[str, object]],
    n: int,
    rng: Random,
    quotas: Mapping[str, int | None],
    remainder_kind: str,
    oversample_share: float,
) -> tuple[list[Mapping[str, object]], list[Mapping[str, object]]]:
    by_kind: dict[str, list[Mapping[str, object]]] = {}
    for row in sorted(rows, key=lambda r: str(r["dedup_key"])):
        by_kind.setdefault(str(row["source_kind"]), []).append(row)
    core: list[Mapping[str, object]] = []
    oversample: list[Mapping[str, object]] = []
    spent = 0
    for kind, quota in quotas.items():
        group = by_kind.get(kind, [])
        take = len(group) if quota is None else min(quota, len(group))
        kind_core, kind_over = draw(group, take, rng, oversample_share)
        core.extend(kind_core)
        oversample.extend(kind_over)
        spent += take
    remainder = by_kind.get(remainder_kind, [])
    rest_core, rest_over = draw(remainder, min(max(n - spent, 0), len(remainder)), rng, oversample_share)
    return core + rest_core, oversample + rest_over


def sample_audit(
    judged_rows: Sequence[Mapping[str, object]],
    *,
    accepts: int,
    rejects: int,
    seed: int,
    quotas: Mapping[str, int | None],
    remainder_kind: str,
    oversample_share: float = 0.3,
) -> AuditSample:
    """Draws the deterministic stratified audit sample over judged rows.

    The draw is seeded and pure, so an evaluator can reproduce the exact core
    set by calling it with the same inputs. Per side (accepted/rejected): every
    kind in ``quotas`` gets its quota (``None`` means exhaustive), the remainder
    budget goes to ``remainder_kind``, and within each subsampled kind
    ``oversample_share`` of the draw oversamples the judge's lowest-confidence
    verdicts.

    Args:
        judged_rows: Rows in :meth:`VerdictStoreMixin.judged` shape for one pass.
        accepts: The audit budget for accepted rows.
        rejects: The audit budget for rejected rows.
        seed: The iteration's deterministic sampling seed.
        quotas: Per-source-kind audit quotas; ``None`` audits the kind exhaustively.
        remainder_kind: The source kind the leftover budget draws from.
        oversample_share: The fraction of each subsampled draw spent on the
            judge's lowest-confidence verdicts.

    Returns:
        The sampled rows, split into the uniform core and the oversample.
    """
    rng = Random(seed)
    accepted = [row for row in judged_rows if row["accepted"]]
    rejected = [row for row in judged_rows if not row["accepted"]]
    accept_core, accept_over = stratified(accepted, accepts, rng, quotas, remainder_kind, oversample_share)
    reject_core, reject_over = stratified(rejected, rejects, rng, quotas, remainder_kind, oversample_share)
    return AuditSample(core=(*accept_core, *reject_core), oversample=(*accept_over, *reject_over))


def exact_upper_bound(hits: int, n: int, alpha: float = 0.05) -> float:
    """Returns the exact (Clopper-Pearson) one-sided upper confidence bound.

    The smallest rate ``p`` such that observing ``hits`` or fewer successes in
    ``n`` trials has probability at most ``alpha`` — the rule of three's exact
    generalization.

    Args:
        hits: The observed success count.
        n: The trial count.
        alpha: The one-sided significance level.

    Returns:
        The upper bound on the true rate.
    """
    if hits >= n:
        return 1.0
    lo, hi = hits / n, 1.0
    for _ in range(60):
        mid = (lo + hi) / 2
        if sum(comb(n, k) * mid**k * (1 - mid) ** (n - k) for k in range(hits + 1)) > alpha:
            lo = mid
        else:
            hi = mid
    return hi


def golden_failure(row: GoldenRow, verdict: Mapping[str, object] | None) -> GoldenFailure | None:
    match verdict:
        case None:
            return GoldenFailure(
                dedup_key=row.dedup_key, expected=row.expected, category=None, rationale=None, text=row.text
            )
        case v if bool(v["accepted"]) is not row.expected:
            return GoldenFailure(
                dedup_key=row.dedup_key,
                expected=row.expected,
                category=str(v["category"]),
                rationale=str(v["rationale"]),
                text=row.text,
            )
        case _:
            return None


def golden_result(
    golden: Sequence[GoldenRow], corpus_keys: set[str], judge_by_key: Mapping[str, Mapping[str, object]], sha256: str
) -> GoldenResult:
    """Gates one pass's verdicts against the frozen golden fixture.

    Args:
        golden: The fixture's rows.
        corpus_keys: Every stored event's dedup key.
        judge_by_key: The pass's verdicts in :meth:`VerdictStoreMixin.judged`
            shape, keyed by dedup key.
        sha256: The fixture file's digest, carried into the result.

    Returns:
        The gate outcome with every mismatched or unjudged row.

    Raises:
        LookupError: If any golden row is missing from the corpus (drift).
    """
    if missing := [row.dedup_key for row in golden if row.dedup_key not in corpus_keys]:
        raise LookupError(f"golden rows missing from the corpus (drift): {missing}")
    failures = tuple(
        failure for row in golden if (failure := golden_failure(row, judge_by_key.get(row.dedup_key))) is not None
    )
    return GoldenResult(total=len(golden), passed=len(golden) - len(failures), sha256=sha256, failures=failures)


def flip_pairs(earlier: Sequence[Mapping[str, object]], later: Sequence[Mapping[str, object]]) -> FlipReport:
    """Compares two verdict passes row by row.

    Args:
        earlier: The earlier pass's rows in :meth:`VerdictStoreMixin.judged` shape.
        later: The later pass's rows in the same shape.

    Returns:
        The overlap size and every side-changing row, ordered by dedup key.
    """
    earlier_by_key = {str(row["dedup_key"]): row for row in earlier}
    later_by_key = {str(row["dedup_key"]): row for row in later}
    common = earlier_by_key.keys() & later_by_key.keys()
    return FlipReport(
        common=len(common),
        flips=tuple(
            Flip(
                dedup_key=key,
                text=str(later_by_key[key]["text"]),
                from_category=str(earlier_by_key[key]["category"]),
                to_category=str(later_by_key[key]["category"]),
            )
            for key in sorted(common)
            if bool(earlier_by_key[key]["accepted"]) is not bool(later_by_key[key]["accepted"])
        ),
    )
