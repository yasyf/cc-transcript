from __future__ import annotations

import os
from pathlib import Path

import anyio

from cc_transcript.discovery import (
    TranscriptDiscovery,
    TranscriptExpiredError,
    find_transcript,
    find_transcript_sync,
)
from cc_transcript.ids import SessionId

SESSION = SessionId("0c8e6f54-aaaa-bbbb-cccc-d1d2d3d4d5d6")


def write(path: Path, mtime: float) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{}\n")
    os.utime(path, (mtime, mtime))
    return path


def test_find_in_returns_all_sorted_by_path(tmp_path: Path) -> None:
    top = write(tmp_path / "b.jsonl", 100.0)
    nested = write(tmp_path / "sub" / "a.jsonl", 200.0)
    assert anyio.run(TranscriptDiscovery.find_in, tmp_path) == [(top, 100.0), (nested, 200.0)]


def test_find_in_skips_unchanged_and_returns_newer(tmp_path: Path) -> None:
    old = write(tmp_path / "old.jsonl", 100.0)
    new = write(tmp_path / "new.jsonl", 300.0)
    known = {str(old): 100.0, str(new): 200.0}
    assert anyio.run(lambda: TranscriptDiscovery.find_in(tmp_path, known_mtimes=known)) == [(new, 300.0)]


def test_find_in_name_contains(tmp_path: Path) -> None:
    write(tmp_path / "main.jsonl", 100.0)
    agent = write(tmp_path / "subagents" / "agent-1.jsonl", 200.0)
    assert anyio.run(lambda: TranscriptDiscovery.find_in(tmp_path, name_contains="agent")) == [(agent, 200.0)]


def test_find_in_limit(tmp_path: Path) -> None:
    write(tmp_path / "a.jsonl", 100.0)
    write(tmp_path / "b.jsonl", 100.0)
    write(tmp_path / "c.jsonl", 100.0)
    assert len(anyio.run(lambda: TranscriptDiscovery.find_in(tmp_path, limit=2))) == 2


def test_find_in_missing_directory(tmp_path: Path) -> None:
    assert anyio.run(lambda: TranscriptDiscovery.find_in(tmp_path / "nope")) == []


def test_find_transcript_dedupes_symlink_spellings(tmp_path: Path) -> None:
    real = write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    (tmp_path / "proj-b").mkdir()
    (tmp_path / "proj-b" / f"{SESSION}.jsonl").symlink_to(real)
    assert anyio.run(lambda: find_transcript(SESSION, root=tmp_path)) == real.resolve()


def test_find_transcript_sync_newest_mtime_wins(tmp_path: Path) -> None:
    write(tmp_path / "proj-a" / f"{SESSION}.jsonl", 100.0)
    newer = write(tmp_path / "proj-b" / f"{SESSION}.jsonl", 200.0)
    assert find_transcript_sync(SESSION, root=tmp_path) == newer.resolve()


def test_find_transcript_missing_session_returns_none(tmp_path: Path) -> None:
    write(tmp_path / "proj-a" / "other-session.jsonl", 100.0)
    assert find_transcript_sync(SESSION, root=tmp_path) is None
    assert anyio.run(lambda: find_transcript(SESSION, root=tmp_path)) is None


def test_find_transcript_sync_missing_root_returns_none(tmp_path: Path) -> None:
    assert find_transcript_sync(SESSION, root=tmp_path / "nope") is None


def test_find_transcript_sync_skips_dangling_symlink(tmp_path: Path) -> None:
    (tmp_path / "proj-a").mkdir()
    (tmp_path / "proj-a" / f"{SESSION}.jsonl").symlink_to(tmp_path / "gone.jsonl")
    assert find_transcript_sync(SESSION, root=tmp_path) is None


def test_transcript_expired_error_carries_session_id() -> None:
    error = TranscriptExpiredError(SESSION)
    assert error.session_id == SESSION
    assert isinstance(error, RuntimeError)
    assert str(SESSION) in str(error)
