from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


def source(path: Path, texts: list[str]) -> Path:
    path.write_text(
        "".join(
            json.dumps(
                {
                    "type": "user",
                    "uuid": f"event-{index}",
                    "sessionId": "bounded-scan-fixture",
                    "timestamp": "2026-01-01T00:00:00Z",
                    "message": {"role": "user", "content": text},
                }
            )
            + "\n"
            for index, text in enumerate(texts)
        )
    )
    return path


def run_scan(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(Path(sys.executable).parent / "cc-transcript"), "grep", *args],
        capture_output=True,
        text=True,
        env=os.environ | {"HOME": str(root)},
        cwd=root,
        timeout=30,
    )


def test_batch_loads_source_once_and_tracks_each_pattern(tmp_path: Path) -> None:
    path = source(tmp_path / "one.jsonl", ["alpha beta", "alpha", "beta"])
    result = run_scan(
        tmp_path, "alpha", str(path), "--pattern", "beta", "--scan-json", "--max-matches", "1"
    )
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["counts"] == [1, 1]
    assert payload["matches"][0]["pattern_ids"] == [0, 1]
    assert payload["matches"][0]["path"] == str(path)
    assert payload["matches"][0]["generation"]
    assert payload["outcome"]["progress"]["source_opens"] == 1
    assert payload["outcome"]["complete"] is False
    assert payload["outcome"]["reason"] == "result_limit"


def test_quota_does_not_open_later_missing_source(tmp_path: Path) -> None:
    path = source(tmp_path / "first.jsonl", ["needle"])
    result = run_scan(
        tmp_path, "needle", str(path), str(tmp_path / "absent.jsonl"), "--scan-json", "--max-matches", "1"
    )
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["counts"] == [1]
    assert payload["outcome"]["progress"]["sources"] == 1


def test_zero_matches_require_complete_scan(tmp_path: Path) -> None:
    path = source(tmp_path / "one.jsonl", ["alpha"])
    complete = run_scan(tmp_path, "absent", str(path), "--scan-json")
    assert complete.returncode == 1, complete.stderr
    assert json.loads(complete.stdout)["outcome"]["complete"] is True
    partial = run_scan(tmp_path, "absent", str(path), "--scan-json", "--max-read-bytes", "16")
    assert partial.returncode == 3, partial.stderr
    assert json.loads(partial.stdout)["outcome"]["complete"] is False


def test_all_retains_discovery_budget(tmp_path: Path) -> None:
    source(tmp_path / "a.jsonl", ["alpha"])
    source(tmp_path / "b.jsonl", ["beta"])
    result = run_scan(
        tmp_path, "absent", "--root", str(tmp_path), "--all", "--scan-json", "--max-discovery-entries", "1"
    )
    assert result.returncode == 3, result.stderr
    payload = json.loads(result.stdout)
    assert payload["outcome"]["complete"] is False
    assert payload["outcome"]["progress"]["source_opens"] == 0


def test_corpus_long_line_does_not_become_zero_matches(tmp_path: Path) -> None:
    path = tmp_path / "corpus.txt"
    path.write_text("abcdefghijk\n")
    result = run_scan(tmp_path, "absent", "--corpus", str(path), "--scan-json", "--max-read-bytes", "4")
    assert result.returncode == 3, result.stderr
    assert json.loads(result.stdout)["outcome"]["complete"] is False


def test_batch_requires_structured_output(tmp_path: Path) -> None:
    result = run_scan(tmp_path, "alpha", "--pattern", "beta")
    assert result.returncode == 2
    assert "--scan-json" in result.stderr


def test_eighteen_patterns_share_one_preparation(tmp_path: Path) -> None:
    patterns = [f"pattern-{index:02d}" for index in range(18)]
    path = source(tmp_path / "batch.jsonl", [" ".join(patterns)])
    flags = [part for pattern in patterns[1:] for part in ("--pattern", pattern)]
    result = run_scan(tmp_path, patterns[0], str(path), *flags, "--scan-json", "--max-matches", "1")
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["counts"] == [1] * 18
    assert payload["matches"][0]["pattern_ids"] == list(range(18))
    assert payload["outcome"]["progress"]["source_opens"] == 1
