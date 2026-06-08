from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, ClassVar, Literal, Protocol

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Sequence
    from pathlib import Path

    from cc_transcript.filterspec import FilterSpec
    from cc_transcript.models import TranscriptEvent


@dataclass(frozen=True, slots=True)
class ParsedTranscript:
    """The parsed events of a single transcript file.

    Attributes:
        path: The transcript's path on disk.
        mtime: The transcript's modification time when parsed.
        events: The parsed events, in file order.
    """

    path: Path
    mtime: float
    events: tuple[TranscriptEvent, ...]


class Backend(Protocol):
    """A transcript-parsing backend.

    Implementations parse a batch of transcript paths into
    :class:`ParsedTranscript` objects, streaming results as they finish.
    """

    name: ClassVar[Literal["rust", "python"]]

    def parse_batch(
        self,
        paths: Sequence[tuple[Path, float]],
        *,
        prefetch: int,
        spec: FilterSpec | None = None,
    ) -> AsyncIterator[ParsedTranscript]:
        """Parses ``paths`` concurrently, yielding results as they complete.

        Args:
            paths: Pairs of ``(path, mtime)`` to parse.
            prefetch: The number of files to keep in flight at once.
            spec: When given, events failing the spec are dropped during
                parsing; portable specs run in the Rust backend, others fall
                back to the Python interpreter.

        Yields:
            One :class:`ParsedTranscript` per input path.
        """
        ...


__all__ = ["Backend", "ParsedTranscript"]
