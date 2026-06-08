from __future__ import annotations

from cc_transcript.models import TranscriptEvent

class ParseStream:
    """A streaming handle over a rayon-parsed batch of transcript files."""

    def recv(self) -> tuple[str, float, list[TranscriptEvent]] | None:
        """Blocks for the next parsed file, or returns None when drained."""

    def recv_many(self, max: int, /) -> list[tuple[str, float, list[TranscriptEvent]]]:
        """Blocks for at least one parsed file, then drains up to ``max``."""

def stream_parse(paths: list[tuple[str, float]], prefetch: int, spec_json: str | None = ..., /) -> ParseStream:
    """Spawns a rayon pool parsing ``paths``, buffering ``prefetch`` results.

    When ``spec_json`` is the JSON of a portable filter spec, events failing it
    are dropped during parsing, before any Python object is built.
    """

def lexicon_available() -> bool:
    """Whether the UDPipe English model loaded (downloading + caching on first call)."""

def lexicon_polarity(lemma: str, /) -> int:
    """Polarity of a single lemma: domain override, else AFINN floored at MIN_MAGNITUDE."""

def lexicon_has_hit(text: str, floor: int, want_negative: bool, /) -> bool:
    """Whether any token's lemma polarity crosses ``floor`` (``<= -floor`` when ``want_negative``)."""

def lexicon_overrides() -> list[tuple[str, int]]:
    """The embedded domain-override entries (for the single-source drift guard)."""

def score_short_circuit(spec_json: str, buckets: list[list[str]], /) -> list[int | None]:
    """Per bucket, the first short-circuit stage's score over its user texts, else None."""

def score_post_process(spec_json: str, buckets: list[list[str]], raw: list[int], /) -> list[int]:
    """Folds each bucket's raw score through the post-process stages in order."""
