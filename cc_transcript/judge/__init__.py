# Re-exports establish the package's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""LLM verdict passes over mined feedback.

Structured completions via the headless ``claude`` CLI (behind the ``[llm]``
extra), fidelity-aware verdict persistence layered on
:class:`~cc_transcript.mining.FeedbackStore`, the anyio fan-out that runs a
judge over rows, and the mechanical eval math (seeded stratified audits, the
golden gate, exact Clopper-Pearson bounds, flip tracking). Apps own the
prompts, the verdict model, and any SQL views over the verdict table.
"""

from __future__ import annotations

from cc_transcript.judge.llm import resolved_model, run_structured, structured_judge
from cc_transcript.judge.verdicts import (
    AuditEstimate,
    AuditSample,
    Disagreement,
    Flip,
    FlipReport,
    GoldenFailure,
    GoldenResult,
    GoldenRow,
    Metrics,
    VerdictLike,
    VerdictStoreMixin,
    exact_upper_bound,
    flip_pairs,
    golden_result,
    run_verdicts,
    sample_audit,
)
