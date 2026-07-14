"""Live transcript tailing: drain each event freshly appended to the projects tree.

A thin sync facade over the native :class:`WatchTailer`, which holds the per-file
byte cursors between polls and does the whole tail — discover changed files, read
only what appended since the last poll, decode complete lines, dedupe compaction
replays, and stamp each event's session id and sidechain flag. :meth:`Watcher.tick`
is one poll step; callers own the poll-forever loop and its cadence.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.ids import SessionId

if TYPE_CHECKING:
    from collections.abc import Sequence

    from cc_transcript.models import TranscriptEvent


@dataclass(frozen=True, slots=True)
class WatchEvent:
    """One transcript event freshly appended to a watched file.

    Attributes:
        path: The transcript file the event was read from.
        session_id: The session the event belongs to — from the event's own
            envelope when it carries one, else the file's last-known session,
            else derived from the transcript path.
        is_sidechain: Whether the file is a subagent sidechain transcript.
        event: The parsed transcript event.
    """

    path: Path
    session_id: SessionId
    is_sidechain: bool
    event: TranscriptEvent


class Watcher:
    """Tails every transcript under ``roots``, one :meth:`tick` per poll.

    Holds the native tailer's cursor between ticks — a file is re-read only
    when its size or mtime changed, a file first seen on the priming pass
    starts at end-of-file unless ``from_start``, a shrunken file (compaction)
    resets its cursor, and the cursor only advances past the last complete
    line. Everything appended after the first tick is drained exactly once,
    with sidechain transcripts flagged on the event.

    Example:
        >>> watcher = Watcher()
        >>> while True:
        ...     for item in watcher.tick():
        ...         handle(item)
        ...     time.sleep(1.0)
    """

    def __init__(self, roots: Sequence[Path] = (CLAUDE_PROJECTS_DIR,), *, from_start: bool = False) -> None:
        self.roots = tuple(roots)
        self.from_start = from_start
        self._tailer = _native.WatchTailer()

    def tick(self) -> list[WatchEvent]:
        """Run one poll step, draining the events appended since the last tick.

        Returns:
            The newly appended events, files in path order, lines in file order.
        """
        return [
            WatchEvent(path=Path(path), session_id=SessionId(session_id), is_sidechain=is_sidechain, event=event)
            for path, session_id, is_sidechain, event in self._tailer.tick(
                [str(root) for root in self.roots], self.from_start
            )
        ]
