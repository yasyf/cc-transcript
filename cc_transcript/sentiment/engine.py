from __future__ import annotations

import os
from dataclasses import dataclass
from typing import TYPE_CHECKING, Protocol

import anyio
import anyio.to_thread

from cc_transcript.models import UserEvent
from cc_transcript.sentiment.buckets import ConversationBucket, SentimentScore
from cc_transcript.sentiment.lexicon import NLP, Lexicon, rust_lexicon
from cc_transcript.sentiment.scorespec import (
    ScoreSpec,
    has_lexicon_stage,
    py_post_process,
    py_short_circuit,
    score_spec_is_portable,
    score_spec_to_json,
)

if TYPE_CHECKING:
    from collections.abc import Callable
    from types import ModuleType


def NOOP_PROGRESS(_: int) -> None:
    return None


class InferenceEngine(Protocol):
    async def score(
        self,
        buckets: list[ConversationBucket],
        *,
        on_progress: Callable[[int], None] = NOOP_PROGRESS,
    ) -> list[SentimentScore]: ...
    def peak_memory_gb(self) -> float: ...
    async def close(self) -> None: ...


def rust_score_backend(spec: ScoreSpec) -> ModuleType | None:
    """The Rust score executor when built, the spec is portable, and (for lexicon
    stages) the udpipe model is available; otherwise None → the Python interpreter."""
    if os.environ.get("CC_TRANSCRIPT_DISABLE_RUST"):
        return None
    try:
        from cc_transcript import _parser_rs
    except ImportError:
        return None
    if not hasattr(_parser_rs, "score_short_circuit") or not score_spec_is_portable(spec):
        return None
    if has_lexicon_stage(spec) and rust_lexicon() is None:
        return None
    return _parser_rs


@dataclass(frozen=True)
class FilteredEngine:
    """Wraps an :class:`InferenceEngine` with a :class:`ScoreSpec`: short-circuit
    stages pre-empt inference, post-process stages adjust the model score. The
    deterministic stages run in Rust when available, Python at parity otherwise."""

    inner: InferenceEngine
    spec: ScoreSpec

    async def score(
        self,
        buckets: list[ConversationBucket],
        *,
        on_progress: Callable[[int], None] = NOOP_PROGRESS,
    ) -> list[SentimentScore]:
        if has_lexicon_stage(self.spec):
            await self.prepare_lexicon()

        texts = [[e.text for e in bucket.events if isinstance(e, UserEvent)] for bucket in buckets]
        rust = rust_score_backend(self.spec)
        spec_json = score_spec_to_json(self.spec) if rust is not None else ""

        prefilled: list[SentimentScore | None] = (
            [None if s is None else SentimentScore(s) for s in rust.score_short_circuit(spec_json, texts)]
            if rust is not None
            else py_short_circuit(self.spec, texts)
        )
        infer_idx = [i for i, p in enumerate(prefilled) if p is None]

        if pre := len(buckets) - len(infer_idx):
            on_progress(pre)

        inferred = await self.inner.score([buckets[i] for i in infer_idx], on_progress=on_progress) if infer_idx else []
        filled = dict(zip(infer_idx, inferred, strict=True))
        scored = [filled[i] if p is None else p for i, p in enumerate(prefilled)]

        if rust is not None:
            return [SentimentScore(s) for s in rust.score_post_process(spec_json, texts, [int(s) for s in scored])]
        return py_post_process(self.spec, texts, scored)

    async def prepare_lexicon(self) -> None:
        if await anyio.to_thread.run_sync(rust_lexicon) is not None:
            return
        await NLP.ensure_ready()
        await Lexicon.ensure_ready()

    def peak_memory_gb(self) -> float:
        return self.inner.peak_memory_gb()

    async def close(self) -> None:
        await self.inner.close()
