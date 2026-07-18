# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""LLM verdict passes over mined feedback.

Structured completions via the headless ``claude`` CLI (behind the ``[llm]``
extra), fidelity-aware verdict persistence layered on
:class:`~cc_transcript.mining.FeedbackStore`, the asyncio fan-out that runs a
judge over rows, and the mechanical eval math (seeded stratified audits, the
golden gate, exact Clopper-Pearson bounds, flip tracking). Apps own the
prompts, the verdict model, and any SQL views over the verdict table.
"""

from __future__ import annotations

from cc_transcript.judge.llm import default_backend, resolved_model, structured_judge
from cc_transcript.judge.similar import (
    Evidence,
    KeyOverlap,
    Suggestion,
    default_embedder,
    embed_evidence,
    near_duplicate_keys,
    record_evidence,
    suggest_canonical_keys,
)
from cc_transcript.judge.verdicts import (
    SLUG_PATTERN,
    AuditEstimate,
    AuditSample,
    Disagreement,
    Flip,
    FlipReport,
    GoldenFailure,
    GoldenResult,
    GoldenRow,
    JudgeError,
    Metrics,
    VerdictLike,
    VerdictSchemaError,
    canonical_slug,
    exact_upper_bound,
    flip_pairs,
    golden_result,
    run_verdicts,
    sample_audit,
)
