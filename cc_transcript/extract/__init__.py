"""The LLM-grounded correction extractor domain.

Layers on :mod:`cc_transcript.evidence` (the deterministic harvest) and
:mod:`cc_transcript.judge` (the LLM runner) to pick the one edit a piece of
feedback faults and append it to the shared ledger. Re-exports lazily so
``import cc_transcript.extract`` stays free of the ``[llm]`` extra; the symbols
pull in pydantic only when first accessed.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from cc_transcript.extract.correct import CorrectionPick, extract_correction, usable_backend

    __all__ = ["CorrectionPick", "extract_correction", "usable_backend"]


def __getattr__(name: str) -> Any:
    import importlib

    return getattr(importlib.import_module(f"{__name__}.correct"), name)
