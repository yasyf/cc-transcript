from __future__ import annotations

from cc_transcript.models import TranscriptEvent

class ParseStream:
    """A streaming handle over a rayon-parsed batch of transcript files."""

    def recv(self) -> tuple[str, float, list[TranscriptEvent]] | None:
        """Blocks for the next parsed file, or returns None when drained."""

    def recv_many(self, max: int, /) -> list[tuple[str, float, list[TranscriptEvent]]]:
        """Blocks for at least one parsed file, then drains up to ``max``."""

def stream_parse(
    paths: list[tuple[str, float]], prefetch: int, spec_json: str | None = ..., /
) -> ParseStream:
    """Spawns a rayon pool parsing ``paths``, buffering ``prefetch`` results.

    When ``spec_json`` is the JSON of a portable filter spec, events failing it
    are dropped during parsing, before any Python object is built.
    """
