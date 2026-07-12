"""Live transcript tailing: poll the projects tree, yield each appended event once.

A byte-offset tailer over the ``*.jsonl`` transcripts: every file gets a
:class:`TailCursor`, each poll reads only what appended since the last one
(open-read-close, never holding a descriptor), complete lines decode through
the parser's per-line decode, and a bounded per-file uuid set keeps compaction
rewrites and replays from double-firing. All progression lives in
:func:`tick` — one deterministic step over a :class:`TailState`, directly
drivable by tests and embedders — and :func:`watch` is the thin poll-forever
loop over it.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

import anyio
import anyio.to_thread

from cc_transcript.discovery import CLAUDE_PROJECTS_DIR, is_subagent_path
from cc_transcript.filterspec import event_meta
from cc_transcript.ids import SessionId
from cc_transcript.models import ModeEvent
from cc_transcript.parser import decode_line

if TYPE_CHECKING:
    import os
    from collections.abc import AsyncIterator, Sequence

    from cc_transcript.ids import EventUuid
    from cc_transcript.models import TranscriptEvent

SEEN_LIMIT = 4096
"""How many yielded event uuids each file's dedupe set retains."""


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


@dataclass(slots=True)
class TailCursor:
    """One watched file's tail progress.

    Attributes:
        offset: Bytes consumed so far — always the end of the last complete
            line, so a partial trailing line stays unconsumed until its
            newline arrives.
        size: The file size at the last processed stat.
        mtime: The file mtime at the last processed stat.
        session_id: The session id cached from the first decoded event that
            carried one.
        seen: Event uuids already yielded, insertion-ordered and bounded at
            :data:`SEEN_LIMIT`.
    """

    offset: int
    size: int
    mtime: float
    session_id: SessionId | None = None
    seen: dict[EventUuid, None] = field(default_factory=dict)


@dataclass(slots=True)
class TailState:
    """The tailer's whole mutable state: one cursor per discovered file.

    Attributes:
        cursors: Tail progress per transcript file.
        primed: Whether the initial discovery pass has run — files first seen
            on that pass are history, files appearing later are new content.
    """

    cursors: dict[Path, TailCursor] = field(default_factory=dict)
    primed: bool = False


async def watch(
    roots: Sequence[Path] = (CLAUDE_PROJECTS_DIR,),
    *,
    poll: float = 1.0,
    from_start: bool = False,
) -> AsyncIterator[WatchEvent]:
    """Tail every transcript under ``roots`` forever, yielding appended events.

    An async generator that never returns on its own: each iteration drains
    one :func:`tick` and sleeps ``poll`` seconds. Content predating the first
    tick is skipped unless ``from_start``; everything appended afterwards is
    yielded exactly once, with sidechain transcripts flagged on the event.
    """
    state = TailState()
    while True:
        for event in await tick(state, roots, from_start=from_start):
            yield event
        await anyio.sleep(poll)


async def tick(state: TailState, roots: Sequence[Path], *, from_start: bool = False) -> list[WatchEvent]:
    """Run one poll step: discover changes under ``roots`` and drain them.

    A file is re-read only when its size or mtime changed — both are compared,
    because mtime granularity can hide rapid appends. A file first seen on the
    priming pass starts at end-of-file unless ``from_start``, so daemon starts
    never replay history; a file appearing on a later pass starts at byte 0,
    since its whole content is new. A file whose size fell below the cursor
    was rewritten (compaction): the cursor resets to 0 and its dedupe set
    clears. The cursor only ever advances past the last complete line — a
    partial trailing line waits, and lines the decoder rejects are skipped.

    Returns:
        The newly appended events, files in path order, lines in file order.
    """
    stats = await scan(roots)
    priming = not state.primed
    state.primed = True
    events: list[WatchEvent] = []
    for path in sorted(stats):
        stat = stats[path]
        if (cursor := state.cursors.get(path)) is None:
            skip_history = priming and not from_start
            cursor = state.cursors[path] = TailCursor(
                offset=stat.st_size if skip_history else 0,
                size=stat.st_size if skip_history else -1,
                mtime=stat.st_mtime if skip_history else -1.0,
            )
        if stat.st_size < cursor.offset:
            cursor.offset = 0
            cursor.seen.clear()
        elif stat.st_size == cursor.size and stat.st_mtime == cursor.mtime:
            continue
        try:
            chunk = await anyio.to_thread.run_sync(read_from, path, cursor.offset)
        except OSError:
            continue
        cursor.size, cursor.mtime = stat.st_size, stat.st_mtime
        complete, _, partial = chunk.rpartition(b"\n")
        cursor.offset += len(chunk) - len(partial)
        for line in complete.split(b"\n"):
            if (event := decode(line)) is None:
                continue
            if (meta := event_meta(event)) is not None:
                if meta.uuid in cursor.seen:
                    continue
                remember(cursor, meta.uuid)
            events.append(
                WatchEvent(
                    path=path,
                    session_id=session_of(cursor, path, event),
                    is_sidechain=is_subagent_path(path),
                    event=event,
                )
            )
    return events


async def scan(roots: Sequence[Path]) -> dict[Path, os.stat_result]:
    """Stat every transcript under ``roots``, skipping macOS resource forks."""
    found: dict[Path, os.stat_result] = {}
    for root in roots:
        base = anyio.Path(root)
        if not await base.exists():
            continue
        async for entry in base.rglob("*.jsonl"):
            if entry.name.startswith("._"):
                continue
            try:
                found[Path(entry)] = await entry.stat()
            except OSError:
                continue
    return found


def read_from(path: Path, offset: int) -> bytes:
    with path.open("rb") as handle:
        handle.seek(offset)
        return handle.read()


def decode(line: bytes) -> TranscriptEvent | None:
    """Decode one complete line, treating any malformed payload as garbage."""
    if not line.strip():
        return None
    try:
        return decode_line(line)
    except (KeyError, ValueError, TypeError):
        return None


def remember(cursor: TailCursor, uuid: EventUuid) -> None:
    cursor.seen[uuid] = None
    while len(cursor.seen) > SEEN_LIMIT:
        del cursor.seen[next(iter(cursor.seen))]


def session_of(cursor: TailCursor, path: Path, event: TranscriptEvent) -> SessionId:
    """The session ``event`` belongs to, cached on the file's cursor.

    Prefers the event's own envelope — meta-bearing events and
    :class:`~cc_transcript.models.ModeEvent` carry the session id — then the
    cursor's cached value, then the path convention: the
    ``<session_id>.jsonl`` stem, or the ``<session_id>/subagents/`` parent
    directory for sidechain files.
    """
    session = event_session(event) or cursor.session_id or path_session_id(path)
    cursor.session_id = session
    return session


def event_session(event: TranscriptEvent) -> SessionId | None:
    match event:
        case ModeEvent(session_id=session_id):
            return session_id
        case _:
            return meta.session_id if (meta := event_meta(event)) is not None else None


def path_session_id(path: Path) -> SessionId:
    return SessionId(path.parent.parent.name if is_subagent_path(path) else path.stem)
