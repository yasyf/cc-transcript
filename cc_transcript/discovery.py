from __future__ import annotations

from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING

import anyio
import anyio.to_thread

if TYPE_CHECKING:
    from cc_transcript.ids import SessionId

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"


class TranscriptExpiredError(RuntimeError):
    """A session's transcript file is gone from disk.

    Raised for the file-gone miss mode only — Claude Code prunes transcripts
    after roughly thirty days. A reference compacted away inside a
    still-living transcript is a separate miss mode, modeled by lookups such
    as ``turn_of()`` and ``hydrate()`` returning ``None``.

    Attributes:
        session_id: The session whose transcript has expired.
    """

    session_id: SessionId

    def __init__(self, session_id: SessionId) -> None:
        super().__init__(f"transcript expired for session {session_id}")
        self.session_id = session_id


class TranscriptDiscovery:
    """Locates Claude Code transcript files on disk.

    Transcripts live as ``*.jsonl`` files under
    :data:`CLAUDE_PROJECTS_DIR` (``~/.claude/projects``), one directory per
    project plus ``subagents/`` sidechain files.
    """

    @staticmethod
    async def find_transcripts() -> list[Path]:
        """Returns every transcript under the projects directory, sorted."""
        root = anyio.Path(CLAUDE_PROJECTS_DIR)
        if not await root.exists():
            return []
        return sorted([Path(p) async for p in root.rglob("*.jsonl")])

    @staticmethod
    async def stat_mtime(path: Path) -> float | None:
        try:
            return (await anyio.Path(path).stat()).st_mtime
        except OSError:
            return None

    @staticmethod
    async def transcript_mtime(path: Path) -> float:
        """Returns ``path``'s modification time, raising if it cannot be read."""
        return (await anyio.Path(path).stat()).st_mtime

    @staticmethod
    async def find_in(
        directory: Path,
        *,
        name_contains: str | None = None,
        limit: int | None = None,
        known_mtimes: dict[str, float] | None = None,
    ) -> list[tuple[Path, float]]:
        """Finds transcripts under ``directory`` newer than known mtimes.

        Args:
            directory: The directory to search recursively.
            name_contains: When set, keep only files whose name contains it.
            limit: When set, return at most this many paths.
            known_mtimes: Map of path string to last-seen mtime; a file is
                kept only when absent from the map or modified since.

        Returns:
            Pairs of ``(path, mtime)`` sorted by path.
        """
        root = anyio.Path(directory)
        if not await root.exists():
            return []
        found: list[tuple[Path, float]] = []
        async for entry in root.rglob("*.jsonl"):
            if name_contains and name_contains not in entry.name:
                continue
            path = Path(entry)
            if (mtime := await TranscriptDiscovery.stat_mtime(path)) is None:
                continue
            if known_mtimes is not None and (prev := known_mtimes.get(str(path))) is not None and prev >= mtime:
                continue
            found.append((path, mtime))
        found.sort(key=lambda e: e[0])
        return found[:limit] if limit is not None else found


def find_transcript_sync(session_id: SessionId, *, root: Path | None = None) -> Path | None:
    """Locates ``session_id``'s transcript on disk, synchronously.

    Globs ``<root>/**/<session_id>.jsonl`` under ``root`` (defaulting to
    :data:`CLAUDE_PROJECTS_DIR`), resolving symlinks — cc-pool gives one
    transcript several path spellings — and deduping by resolved real path.

    Returns:
        The newest-mtime real path, or None when no transcript exists.
    """
    base = root or CLAUDE_PROJECTS_DIR
    if not base.exists():
        return None
    candidates: dict[Path, float] = {}
    for entry in base.rglob(f"{session_id}.jsonl"):
        if (real := entry.resolve()) in candidates:
            continue
        try:
            candidates[real] = real.stat().st_mtime
        except OSError:
            continue
    return max(candidates, key=candidates.__getitem__, default=None)


async def find_transcript(session_id: SessionId, *, root: Path | None = None) -> Path | None:
    """Locates ``session_id``'s transcript on disk.

    The async counterpart of :func:`find_transcript_sync`, scanning off the
    event loop in a worker thread.
    """
    return await anyio.to_thread.run_sync(partial(find_transcript_sync, session_id, root=root))


def subagent_paths(path: Path) -> tuple[Path, ...]:
    """Sidechain transcript files spawned by the session transcript at ``path``.

    Claude Code writes subagent transcripts as
    ``<parent>/<stem>/subagents/agent-<tool_use_id>.jsonl`` next to the main
    transcript; macOS resource-fork artifacts (``._*``) are skipped.

    Returns:
        The sidechain files sorted by path; ``()`` when none exist.
    """
    if not (directory := path.parent / path.stem / "subagents").is_dir():
        return ()
    return tuple(sorted(entry for entry in directory.glob("*.jsonl") if not entry.name.startswith("._")))
