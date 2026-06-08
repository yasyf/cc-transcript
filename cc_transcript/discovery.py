from __future__ import annotations

from pathlib import Path

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"


class TranscriptDiscovery:
    """Locates Claude Code transcript files on disk.

    Transcripts live as ``*.jsonl`` files under
    :data:`CLAUDE_PROJECTS_DIR` (``~/.claude/projects``), one directory per
    project plus ``subagents/`` sidechain files.
    """

    @staticmethod
    def find_transcripts() -> list[Path]:
        """Returns every transcript under the projects directory, sorted."""
        if not CLAUDE_PROJECTS_DIR.exists():
            return []
        return sorted(CLAUDE_PROJECTS_DIR.rglob("*.jsonl"))

    @staticmethod
    def stat_mtime(path: Path) -> float | None:
        try:
            return path.stat().st_mtime
        except OSError:
            return None

    @staticmethod
    def transcript_mtime(path: Path) -> float:
        """Returns ``path``'s modification time, raising if it cannot be read."""
        return path.stat().st_mtime

    @staticmethod
    def find_in(
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
        if not directory.exists():
            return []
        found = [
            (p, mtime)
            for p in directory.rglob("*.jsonl")
            if not name_contains or name_contains in p.name
            if (mtime := TranscriptDiscovery.stat_mtime(p)) is not None
            if known_mtimes is None or (prev := known_mtimes.get(str(p))) is None or prev < mtime
        ]
        found.sort(key=lambda e: e[0])
        return found[:limit] if limit is not None else found
