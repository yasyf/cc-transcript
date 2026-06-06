from __future__ import annotations

import os
from pathlib import Path

from cc_transcript.discovery import TranscriptDiscovery


def write(path: Path, mtime: float) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{}\n")
    os.utime(path, (mtime, mtime))
    return path


def test_find_in_returns_all_sorted_by_path(tmp_path: Path) -> None:
    top = write(tmp_path / "b.jsonl", 100.0)
    nested = write(tmp_path / "sub" / "a.jsonl", 200.0)
    assert TranscriptDiscovery.find_in(tmp_path) == [(top, 100.0), (nested, 200.0)]


def test_find_in_skips_unchanged_and_returns_newer(tmp_path: Path) -> None:
    old = write(tmp_path / "old.jsonl", 100.0)
    new = write(tmp_path / "new.jsonl", 300.0)
    known = {str(old): 100.0, str(new): 200.0}
    assert TranscriptDiscovery.find_in(tmp_path, known_mtimes=known) == [(new, 300.0)]


def test_find_in_name_contains(tmp_path: Path) -> None:
    write(tmp_path / "main.jsonl", 100.0)
    agent = write(tmp_path / "subagents" / "agent-1.jsonl", 200.0)
    assert TranscriptDiscovery.find_in(tmp_path, name_contains="agent") == [(agent, 200.0)]


def test_find_in_limit(tmp_path: Path) -> None:
    write(tmp_path / "a.jsonl", 100.0)
    write(tmp_path / "b.jsonl", 100.0)
    write(tmp_path / "c.jsonl", 100.0)
    assert len(TranscriptDiscovery.find_in(tmp_path, limit=2)) == 2


def test_find_in_missing_directory(tmp_path: Path) -> None:
    assert TranscriptDiscovery.find_in(tmp_path / "nope") == []
