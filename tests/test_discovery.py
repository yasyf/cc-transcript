from __future__ import annotations

import os
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from cc_transcript.discovery import (
    TRANSCRIPT_MEMO,
    TranscriptExpiredError,
    discover,
    find_in,
    is_subagent_path,
    resolve,
    subagent_transcripts,
)
from cc_transcript.ids import SessionId, ToolUseId

if TYPE_CHECKING:
    from collections.abc import Iterator

SESSION = SessionId("0c8e6f54-aaaa-bbbb-cccc-d1d2d3d4d5d6")


@pytest.fixture(autouse=True)
def clear_transcript_memo() -> Iterator[None]:
    TRANSCRIPT_MEMO.clear()
    yield
    TRANSCRIPT_MEMO.clear()


def write(path: Path, mtime: float) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{}\n")
    os.utime(path, (mtime, mtime))
    return path


def test_discover_returns_all_sorted_by_path(tmp_path: Path) -> None:
    top = write(tmp_path / "b.jsonl", 100.0)
    nested = write(tmp_path / "sub" / "a.jsonl", 200.0)
    assert discover(tmp_path) == [top, nested]


def test_discover_missing_root(tmp_path: Path) -> None:
    assert discover(tmp_path / "nope") == []


def test_find_in_returns_all_sorted_by_path(tmp_path: Path) -> None:
    top = write(tmp_path / "b.jsonl", 100.0)
    nested = write(tmp_path / "sub" / "a.jsonl", 200.0)
    assert find_in(tmp_path) == [(top, 100.0), (nested, 200.0)]


def test_find_in_skips_unchanged_and_returns_newer(tmp_path: Path) -> None:
    old = write(tmp_path / "old.jsonl", 100.0)
    new = write(tmp_path / "new.jsonl", 300.0)
    known = {str(old): 100.0, str(new): 200.0}
    assert find_in(tmp_path, known_mtimes=known) == [(new, 300.0)]


def test_find_in_name_contains(tmp_path: Path) -> None:
    write(tmp_path / "main.jsonl", 100.0)
    agent = write(tmp_path / "subagents" / "agent-1.jsonl", 200.0)
    assert find_in(tmp_path, name_contains="agent") == [(agent, 200.0)]


def test_find_in_limit(tmp_path: Path) -> None:
    write(tmp_path / "a.jsonl", 100.0)
    write(tmp_path / "b.jsonl", 100.0)
    write(tmp_path / "c.jsonl", 100.0)
    assert len(find_in(tmp_path, limit=2)) == 2


def test_find_in_missing_directory(tmp_path: Path) -> None:
    assert find_in(tmp_path / "nope") == []


def test_resolve_dedupes_symlink_spellings(tmp_path: Path) -> None:
    real = write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    (tmp_path / "proj-b").mkdir()
    (tmp_path / "proj-b" / f"{SESSION}.jsonl").symlink_to(real)
    assert resolve(SESSION, root=tmp_path) == real.resolve()


def test_resolve_newest_mtime_wins(tmp_path: Path) -> None:
    write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    newer = write(tmp_path / "proj-b" / f"{SESSION}.jsonl", 200.0)
    assert resolve(SESSION, root=tmp_path) == newer.resolve()


def test_resolve_memoizes_a_hit_and_skips_the_rescan(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    real = write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    assert resolve(SESSION, root=tmp_path) == real.resolve()
    assert TRANSCRIPT_MEMO[(SESSION, tmp_path.resolve())] == real.resolve()

    def boom(*_: object) -> None:
        raise AssertionError("a memo hit must not rescan the projects tree")

    monkeypatch.setattr("cc_transcript.discovery._native.discovery_find_transcript", boom)
    assert resolve(SESSION, root=tmp_path) == real.resolve()


def test_resolve_never_memoizes_a_miss(tmp_path: Path) -> None:
    assert resolve(SESSION, root=tmp_path) is None
    assert (SESSION, tmp_path.resolve()) not in TRANSCRIPT_MEMO
    later = write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    assert resolve(SESSION, root=tmp_path) == later.resolve()


def test_resolve_revalidates_a_deleted_cached_path(tmp_path: Path) -> None:
    stale = write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    assert resolve(SESSION, root=tmp_path) == stale.resolve()
    stale.unlink()
    fresh = write(tmp_path / "proj-b" / f"{SESSION}.jsonl", 200.0)
    assert resolve(SESSION, root=tmp_path) == fresh.resolve()
    assert TRANSCRIPT_MEMO[(SESSION, tmp_path.resolve())] == fresh.resolve()


def test_resolve_missing_session_returns_none(tmp_path: Path) -> None:
    write(tmp_path / "proj-a" / "other-session.jsonl", 100.0)
    assert resolve(SESSION, root=tmp_path) is None


def test_resolve_missing_root_returns_none(tmp_path: Path) -> None:
    assert resolve(SESSION, root=tmp_path / "nope") is None


def test_resolve_skips_dangling_symlink(tmp_path: Path) -> None:
    (tmp_path / "proj-a").mkdir()
    (tmp_path / "proj-a" / f"{SESSION}.jsonl").symlink_to(tmp_path / "gone.jsonl")
    assert resolve(SESSION, root=tmp_path) is None


def test_transcript_expired_error_carries_session_id() -> None:
    error = TranscriptExpiredError(SESSION)
    assert error.session_id == SESSION
    assert isinstance(error, RuntimeError)
    assert str(SESSION) in str(error)


def test_subagent_transcripts_keys_by_tool_use_id_and_skips_resource_forks(tmp_path: Path) -> None:
    main = write(tmp_path / f"{SESSION}.jsonl", 100.0)
    t9 = write(tmp_path / SESSION / "subagents" / "agent-t9.jsonl", 200.0)
    t10 = write(tmp_path / SESSION / "subagents" / "agent-t10.jsonl", 200.0)
    write(tmp_path / SESSION / "subagents" / "._agent-t9.jsonl", 200.0)
    assert subagent_transcripts(main) == {ToolUseId("t9"): t9, ToolUseId("t10"): t10}


def test_subagent_transcripts_empty_without_directory(tmp_path: Path) -> None:
    assert subagent_transcripts(write(tmp_path / f"{SESSION}.jsonl", 100.0)) == {}


def test_find_in_negative_limit_is_a_pinned_overflow(tmp_path: Path) -> None:
    """v14 divergence pin: the v13 all-but-last slice for limit=-1 was accidental.

    The native boundary takes an unsigned limit, so a negative one raises at
    conversion instead of silently slicing; callers pass None for "no limit".
    """
    with pytest.raises(OverflowError):
        find_in(tmp_path, limit=-1)


def test_surrogate_escaped_paths_cross_the_boundary(tmp_path: Path) -> None:
    """FIX E (discovery-only): non-UTF-8 path INPUT no longer raises.

    A surrogate-escaped Path used to die with UnicodeEncodeError at the &str
    boundary; PathBuf converts via the OS codec. The output half (non-UTF-8
    names read from disk, formerly corrupted by to_string_lossy) cannot be
    exercised here — APFS rejects non-UTF-8 filenames — and rides the same
    PathBuf conversion.
    """
    surrogate = tmp_path / "missing-\udcff"
    assert discover(surrogate) == []
    assert find_in(surrogate) == []
    assert is_subagent_path(surrogate) is False
    assert subagent_transcripts(surrogate) == {}


def test_discovery_returns_pathlib_paths(tmp_path: Path) -> None:
    (tmp_path / "proj").mkdir()
    (tmp_path / "proj" / "s.jsonl").write_text("")
    assert discover(tmp_path) == [tmp_path / "proj" / "s.jsonl"]
    assert all(isinstance(p, Path) for p in discover(tmp_path))
