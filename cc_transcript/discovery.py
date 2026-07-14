"""Locating transcript files under ``~/.claude/projects``.

A sync facade over the native discovery functions. The projects-root default
and the positive-hit memo are facade concerns: the native side scans exactly
the root it is handed.
"""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript.ids import ToolUseId

if TYPE_CHECKING:
    from cc_transcript.ids import SessionId

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"

TRANSCRIPT_MEMO: dict[tuple[SessionId, Path], Path] = {}
"""Positive-hit memo for :func:`resolve`, keyed by ``(session_id, resolved root)``.

Only successful lookups are cached: a miss may become a hit once Claude Code
writes the file, so ``None`` is never stored. A cached hit is revalidated with
``Path.exists`` before it is reused — a transcript file can be pruned from disk —
and a stale entry falls through to a fresh scan.
"""


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


def discover(root: Path | None = None) -> list[Path]:
    """Every transcript under ``root``, sorted by path.

    Transcripts live as ``*.jsonl`` files under ``root`` (defaulting to
    :data:`CLAUDE_PROJECTS_DIR`), one directory per project plus
    ``subagents/`` sidechain files.

    Returns:
        The transcript paths; ``[]`` when the root does not exist.

    Example:
        >>> for transcript in stream(discover()):
        ...     print(transcript.path)
    """
    return [Path(p) for p in _native.discovery_find_transcripts(str(root or CLAUDE_PROJECTS_DIR))]


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
    return [
        (Path(p), mtime)
        for p, mtime in _native.discovery_find_in(str(directory), name_contains, limit, known_mtimes)
    ]


def resolve(session_id: SessionId, *, root: Path | None = None) -> Path | None:
    """Locates ``session_id``'s transcript on disk.

    Scans ``root`` (defaulting to :data:`CLAUDE_PROJECTS_DIR`) for
    ``<session_id>.jsonl``, resolving symlinks — cc-pool gives one transcript
    several path spellings — and deduping by resolved real path.

    A successful lookup is memoized in ``TRANSCRIPT_MEMO`` under
    ``(session_id, resolved root)`` and revalidated on the next hit, so a
    repeated probe skips the recursive scan while a pruned transcript still
    forces a fresh one.

    Returns:
        The newest-mtime real path, or None when no transcript exists.
    """
    base = root or CLAUDE_PROJECTS_DIR
    key = (session_id, base.resolve())
    if (cached := TRANSCRIPT_MEMO.get(key)) is not None and cached.exists():
        return cached
    if (hit := _native.discovery_find_transcript(str(base), session_id)) is None:
        return None
    TRANSCRIPT_MEMO[key] = (path := Path(hit))
    return path


def is_subagent_path(path: Path) -> bool:
    """Whether ``path`` names a subagent sidechain transcript.

    Matches the ``agent-<tool_use_id>.jsonl`` naming convention that
    :func:`subagent_paths` discovers.
    """
    return _native.discovery_is_subagent_path(str(path))


def subagent_paths(path: Path) -> tuple[Path, ...]:
    """Sidechain transcript files spawned by the session transcript at ``path``.

    Claude Code writes subagent transcripts as
    ``<parent>/<stem>/subagents/agent-<tool_use_id>.jsonl`` next to the main
    transcript; macOS resource-fork artifacts (``._*``) are skipped.

    Returns:
        The sidechain files sorted by path; ``()`` when none exist.
    """
    return tuple(Path(p) for p in _native.discovery_subagent_paths(str(path)))


def subagent_transcripts(path: Path) -> dict[ToolUseId, Path]:
    """Sidechain transcripts keyed by the tool-use id that spawned each one.

    Parses the ``agent-<tool_use_id>`` stem of every file
    :func:`subagent_paths` finds, inheriting its skip of macOS resource-fork
    artifacts (``._*``).

    Returns:
        A mapping from tool-use id to sidechain file; ``{}`` when none exist.
    """
    return {
        ToolUseId(tool_use_id): Path(p)
        for tool_use_id, p in _native.discovery_subagent_transcripts(str(path)).items()
    }
