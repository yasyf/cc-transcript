from __future__ import annotations

from pathlib import Path

import anyio

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"


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
