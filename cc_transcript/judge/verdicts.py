"""Generic LLM verdict passes over mined feedback: storage, fan-out, sampling, eval math.

The mechanism proven in cc-steer's triage pipeline, lifted app-free: a verdict
table layered on :class:`FeedbackStore`'s event ledger, the asyncio fan-out that
runs a judge over rows, the seeded stratified audit sampler, and the mechanical eval
math (golden gate, exact Clopper-Pearson bounds, flip tracking). Apps own the
prompts, the verdict model, and any SQL views over the verdict table.
"""

from __future__ import annotations

import asyncio
import re
from dataclasses import dataclass
from typing import TYPE_CHECKING, Protocol

from cc_transcript import _native

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable, Mapping, Sequence

SLUG_PATTERN = re.compile(r"^[a-z0-9]+(-[a-z0-9]+){1,5}$")
"""Matches a durable rule slug: two to six ``[a-z0-9]`` groups joined by single hyphens.

The two-group floor rejects a bare word, and the six-group ceiling — together
with the hyphen requirement — can never match a 64-character hex digest, so a
slug and a content digest never collide.
"""

class VerdictSchemaError(RuntimeError):
    """The verdict table predates the v9 schema and must be rebuilt by hand.

    Raised at open time by the native store engine when the physical verdict
    table lacks the ``canonical_key`` column or its unique index still covers the
    v8 identity ``(dedup_key, role, prompt_version, model)``. ``CREATE TABLE IF
    NOT EXISTS`` leaves such a table untouched, and the ``judged`` query neither
    selects ``canonical_key`` nor joins on ``model``, so a v8 table would
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
    it — an app whose model names the bits differently (cc-steer's
    ``is_steering``/``what_claude_did``) adapts with two properties.

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
    persister = asyncio.Lock()

    async def worker(row: Mapping[str, object]) -> None:
        async with limiter:
            try:
                verdict = await judge(await prompt_for(row))
            except JudgeError:
                counts["failed"] += 1
                return
        # The store's transaction is exclusive by contract; persists serialize on
        # this lock so concurrent workers never race record_verdict into a
        # TransactionConflictError. Judge concurrency is untouched.
        async with persister:
            await persist(row, verdict)
        counts["judged"] += 1

    async with asyncio.TaskGroup() as tg:
        for row in rows:
            tg.create_task(worker(row))
    return counts["judged"], counts["failed"]


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
        judged_rows: Rows in :meth:`~cc_transcript.mining.store.FeedbackStore.judged` shape for one pass.
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
    rows = list(judged_rows)
    core, oversample = _native.judge_sample_audit(
        [
            (str(row["dedup_key"]), str(row["source_kind"]), float(str(row["confidence"])), bool(row["accepted"]))
            for row in rows
        ],
        accepts,
        rejects,
        str(seed),
        list(quotas.items()),
        remainder_kind,
        oversample_share,
    )
    return AuditSample(
        core=tuple(rows[index] for index in core),
        oversample=tuple(rows[index] for index in oversample),
    )


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
    return _native.judge_exact_upper_bound(hits, n, alpha)


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
        judge_by_key: The pass's verdicts in :meth:`~cc_transcript.mining.store.FeedbackStore.judged`
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
        earlier: The earlier pass's rows in :meth:`~cc_transcript.mining.store.FeedbackStore.judged` shape.
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
