"""Live transcript tailing: yield each event freshly appended to the projects tree.

A thin async facade over the native :class:`WatchTailer`, which holds the per-file
byte cursors between polls and does the whole tail — discover changed files, read
only what appended since the last poll, decode complete lines, dedupe compaction
replays, and stamp each event's session id and sidechain flag. :func:`tick` is one
poll step (directly drivable by tests and embedders) and :func:`watch` is the thin
poll-forever loop over it.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

import anyio
import anyio.to_thread

from cc_transcript import _parser_rs
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.ids import SessionId

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Sequence

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


async def watch(
    roots: Sequence[Path] = (CLAUDE_PROJECTS_DIR,),
    *,
    poll: float = 1.0,
    from_start: bool = False,
) -> AsyncIterator[WatchEvent]:
    """Tail every transcript under ``roots`` forever, yielding appended events.

    An async generator that never returns on its own: each iteration drains one
    :func:`tick` and sleeps ``poll`` seconds. Content predating the first tick is
    skipped unless ``from_start``; everything appended afterwards is yielded
    exactly once, with sidechain transcripts flagged on the event.
    """
    tailer = _parser_rs.WatchTailer()
    while True:
        for event in await tick(tailer, roots, from_start=from_start):
            yield event
        await anyio.sleep(poll)


async def tick(
    tailer: _parser_rs.WatchTailer, roots: Sequence[Path], *, from_start: bool = False
) -> list[WatchEvent]:
    """Run one poll step over ``roots`` against ``tailer``, draining appended events.

    Delegates to the native tailer's cursor — a file is re-read only when its size
    or mtime changed, a file first seen on the priming pass starts at end-of-file
    unless ``from_start``, a shrunken file (compaction) resets its cursor, and the
    cursor only advances past the last complete line. Returns the newly appended
    events, files in path order, lines in file order.
    """
    rows = await anyio.to_thread.run_sync(tailer.tick, [str(root) for root in roots], from_start)
    return [
        WatchEvent(path=Path(path), session_id=SessionId(session_id), is_sidechain=is_sidechain, event=event)
        for path, session_id, is_sidechain, event in rows
    ]
