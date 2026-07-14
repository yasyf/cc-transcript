from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Protocol

from cc_transcript import _native
from cc_transcript.models import UserEvent
from cc_transcript.sentiment.buckets import ConversationBucket, SentimentScore
from cc_transcript.sentiment.scorespec import ScoreSpec, score_spec_to_json

if TYPE_CHECKING:
    from collections.abc import Callable


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


@dataclass(frozen=True)
class FilteredEngine:
    """Wraps an :class:`InferenceEngine` with a :class:`ScoreSpec`: short-circuit
    stages pre-empt inference, post-process stages adjust the model score. Every
    deterministic stage runs in Rust; only inference stays Python-side."""

    inner: InferenceEngine
    spec: ScoreSpec

    async def score(
        self,
        buckets: list[ConversationBucket],
        *,
        on_progress: Callable[[int], None] = NOOP_PROGRESS,
    ) -> list[SentimentScore]:
        texts = [[e.text for e in bucket.events if isinstance(e, UserEvent)] for bucket in buckets]
        spec_json = score_spec_to_json(self.spec)

        prefilled: list[SentimentScore | None] = [
            None if s is None else SentimentScore(s) for s in _native.score_short_circuit(spec_json, texts)
        ]
        infer_idx = [i for i, p in enumerate(prefilled) if p is None]

        if pre := len(buckets) - len(infer_idx):
            on_progress(pre)

        inferred = await self.inner.score([buckets[i] for i in infer_idx], on_progress=on_progress) if infer_idx else []
        filled = dict(zip(infer_idx, inferred, strict=True))
        scored = [filled[i] if p is None else p for i, p in enumerate(prefilled)]

        return [SentimentScore(s) for s in _native.score_post_process(spec_json, texts, [int(s) for s in scored])]

    def peak_memory_gb(self) -> float:
        return self.inner.peak_memory_gb()

    async def close(self) -> None:
        await self.inner.close()
