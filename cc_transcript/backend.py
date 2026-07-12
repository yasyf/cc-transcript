from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pathlib import Path

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
