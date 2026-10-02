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
    scope = [] if "--corpus" in args or "--root" in args else ["--root", str(root)]
    return subprocess.run(
        [str(Path(sys.executable).parent / "cc-transcript"), "grep", *scope, *args],
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


def test_relative_root_keeps_project_filter_and_provenance(tmp_path: Path) -> None:
    project = tmp_path / "project-foo"
    project.mkdir()
    source(project / "one.jsonl", ["needle"])
    result = run_scan(tmp_path, "needle", "--root", ".", "--project", "foo", "--scan-json")
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["counts"] == [1]
    assert payload["matches"][0]["path"] == "./project-foo/one.jsonl"
    assert payload["outcome"]["complete"] is True


def test_discovery_does_not_follow_directory_symlinks(tmp_path: Path) -> None:
    root = tmp_path / "root"
    root.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    source(outside / "secret.jsonl", ["needle"])
    (root / "linked").symlink_to(outside, target_is_directory=True)
    result = run_scan(tmp_path, "needle", "--root", str(root), "--scan-json")
    assert result.returncode == 1, result.stderr
    payload = json.loads(result.stdout)
    assert payload["outcome"]["complete"] is True
    assert payload["outcome"]["progress"]["source_opens"] == 0


def test_corpus_match_checks_output_before_rendering(tmp_path: Path) -> None:
    path = tmp_path / "corpus.txt"
    path.write_text("needle " + "x" * 1024 + "\n")
    result = run_scan(tmp_path, "needle", "--corpus", str(path), "--scan-json", "--max-output-bytes", "4097")
    assert result.returncode == 3, result.stderr
    payload = json.loads(result.stdout)
    assert payload["matches"] == []
    assert "output" in payload["outcome"]["reason"]


def test_repeated_grep_on_an_appended_file_reads_only_new_bytes(tmp_path: Path) -> None:
    path = source(tmp_path / "live.jsonl", ["needle", *(f"filler {index} {'x' * 500}" for index in range(200))])
    cold = run_scan(tmp_path, "needle", str(path), "--scan-json", "--max-matches", "0")
    assert cold.returncode == 0, cold.stderr
    assert json.loads(cold.stdout)["outcome"]["progress"]["source_bytes"] == path.stat().st_size
    hit = len(path.read_bytes().split(b"\n")[0])
    before = path.stat().st_size
    appended = source(tmp_path / "suffix.jsonl", ["needle later", "tail"]).read_bytes()
    with path.open("ab") as handle:
        handle.write(appended)
    warm = run_scan(tmp_path, "needle", str(path), "--scan-json", "--max-matches", "0")
    assert warm.returncode == 0, warm.stderr
    payload = json.loads(warm.stdout)
    assert payload["counts"] == [2]
    assert payload["outcome"]["complete"] is True
    progress = payload["outcome"]["progress"]
    assert progress["cache_hits"] == 1
    assert progress["source_bytes"] == 64 + hit + len(appended)
    assert progress["validated_bytes"] == before
    assert path.stat().st_size == before + len(appended)


def test_growth_after_a_prefix_rewrite_matches_a_fresh_scan(tmp_path: Path) -> None:
    path = source(tmp_path / "live.jsonl", [f"filler {index} {'x' * 500}" for index in range(200)])
    cold = run_scan(tmp_path, "needle", str(path), "--scan-json")
    assert cold.returncode == 1, cold.stderr
    before = path.stat().st_size
    path.write_bytes(path.read_bytes().replace(b"filler 5 ", b"needle 5 "))
    with path.open("ab") as handle:
        handle.write(source(tmp_path / "suffix.jsonl", ["tail"]).read_bytes())
    warm = run_scan(tmp_path, "needle", str(path), "--scan-json")
    fresh_path = tmp_path / "fresh.jsonl"
    fresh_path.write_bytes(path.read_bytes())
    fresh = run_scan(tmp_path, "needle", str(fresh_path), "--scan-json")
    assert warm.returncode == fresh.returncode == 0, warm.stderr
    payload, expected = json.loads(warm.stdout), json.loads(fresh.stdout)
    assert payload["counts"] == expected["counts"] == [1]
    assert payload["outcome"]["complete"] is expected["outcome"]["complete"] is True
    progress = payload["outcome"]["progress"]
    assert progress["cache_hits"] == 0
    assert progress["cache_invalidations"] == 1
    assert progress["validated_bytes"] == before
