from __future__ import annotations

import json
import os
import re
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


def test_exhausted_budget_names_its_flag_once(tmp_path: Path) -> None:
    source(tmp_path / "a.jsonl", ["alpha"])
    source(tmp_path / "b.jsonl", ["beta"])
    result = run_scan(tmp_path, "absent", "--all", "--max-sources", "1")
    assert result.returncode == 3, result.stderr
    assert result.stdout.splitlines()[-1] == "0 files, 0 matches so far (incomplete scan)"
    assert result.stderr == "warning: scan incomplete: source budget exhausted; raise --max-sources above 1\n"
    structured = run_scan(tmp_path, "absent", "--all", "--max-sources", "1", "--scan-json")
    assert json.loads(structured.stdout)["outcome"]["budget"] == "sources"


def test_sources_budget_counts_selected_sources_only(tmp_path: Path) -> None:
    for name in ("a", "b", "c"):
        source(tmp_path / f"{name}.jsonl", [name])
    result = run_scan(tmp_path, "absent", "--limit", "1", "--max-sources", "1", "--scan-json")
    assert result.returncode == 1, result.stderr
    assert json.loads(result.stdout)["outcome"]["complete"] is True


def test_discovery_is_not_charged_to_output(tmp_path: Path) -> None:
    for index in range(40):
        source(tmp_path / f"{index:02d}.jsonl", ["alpha"])
    result = run_scan(tmp_path, "absent", "--all", "--max-output-bytes", "64")
    assert result.returncode == 1, result.stderr
    assert result.stdout == "0 files, 0 matches\n"


def oversized(path: Path) -> Path:
    return source(path, ["needle early", "needle " + "x" * 40000, *(f"needle later {index}" for index in range(4))])


def test_output_cap_cuts_an_oversized_line_and_prints_every_match_after_it(tmp_path: Path) -> None:
    path = oversized(tmp_path / "wide.jsonl")
    result = run_scan(tmp_path, "needle", str(path), "--width", "0", "--max-output-bytes", "20000")
    assert result.returncode == 0, result.stderr
    header, early, cut, *later, summary = result.stdout.splitlines()
    assert summary == "1 files, 6 matches"
    assert [line.split()[0] for line in (early, cut, *later)] == ["0", "1", "2", "3", "4", "5"]
    assert all(line.endswith(f"needle later {index}") for index, line in enumerate(later))
    shown = header.removeprefix("== ")
    tail = re.search(r" \[cut from (\d+) bytes; read it whole with (.+)\]$", cut)
    assert tail, cut
    assert tail.group(2) == f"cc-transcript show {shown} --range 1:2 --width 0"
    assert len(cut.encode()) < 400 < int(tail.group(1))
    whole = sum(len(line.encode()) + 2 for line in (header, early, *later)) + int(tail.group(1)) + 2
    assert result.stderr == (
        f"warning: --max-output-bytes 20000 shortened 1 line at event 1; raise it to {whole} to print every line whole\n"
    )
    assert run_scan(tmp_path, "needle", str(path), "--width", "0", "--max-output-bytes", str(whole)).stderr == ""


def test_output_cap_cuts_an_oversized_json_event_to_a_record_that_parses(tmp_path: Path) -> None:
    path = oversized(tmp_path / "wide.jsonl")
    result = run_scan(tmp_path, "needle", str(path), "--json", "--max-output-bytes", "20000")
    assert result.returncode == 0, result.stderr
    events = [json.loads(record) for record in result.stdout.splitlines()]
    assert [event["i"] for event in events] == list(range(6))
    assert events[1] == {
        "path": str(path),
        "i": 1,
        "kind": "user",
        "cut": {"bytes": events[1]["cut"]["bytes"], "show": f"cc-transcript show {path} --range 1:2 --json"},
    }
    assert events[1]["cut"]["bytes"] > 40000
    assert [event["text"] for event in events[2:]] == [f"needle later {index}" for index in range(4)]
    assert "shortened 1 line at event 1" in result.stderr


def test_output_cap_counts_every_match_once_no_line_fits(tmp_path: Path) -> None:
    path = source(tmp_path / "wide.jsonl", [f"needle {index} " + "x" * 4000 for index in range(40)])
    result = run_scan(tmp_path, "needle", str(path), "--max-matches", "0", "--max-output-bytes", "2000")
    assert result.returncode == 0, result.stderr
    *printed, summary = result.stdout.splitlines()
    hits = len(printed) - 1
    assert 0 < hits < 40
    assert sum(len(line.encode()) + 2 for line in printed) <= 2000
    assert summary == "1 files, 40 matches"
    warning = re.fullmatch(
        rf"warning: --max-output-bytes 2000 left out {40 - hits} lines from event {hits} on;"
        r" raise it to (\d+) to print every line whole\n",
        result.stderr,
    )
    assert warning, result.stderr
    whole = warning.group(1)
    raised = run_scan(tmp_path, "needle", str(path), "--max-matches", "0", "--max-output-bytes", whole)
    assert raised.stderr == ""
    assert len(raised.stdout.splitlines()) == 42
    short = run_scan(tmp_path, "needle", str(path), "--max-matches", "0", "--max-output-bytes", str(int(whole) - 1))
    assert "left out 1 line at event 39" in short.stderr


def test_project_filter_precedes_discovery_budget(tmp_path: Path) -> None:
    for index in range(5):
        other = tmp_path / f"project-other-{index}"
        other.mkdir()
        source(other / "one.jsonl", ["needle"])
    wanted = tmp_path / "project-foo"
    wanted.mkdir()
    source(wanted / "one.jsonl", ["alpha"])
    result = run_scan(tmp_path, "needle", "--project", "foo", "--max-discovery-entries", "3", "--scan-json")
    assert result.returncode == 1, result.stderr
    assert json.loads(result.stdout)["outcome"]["complete"] is True


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


def test_corpus_match_no_bytes_can_hold_is_counted_not_printed(tmp_path: Path) -> None:
    path = tmp_path / "corpus.txt"
    path.write_text("needle " + "x" * 1024 + "\n")
    result = run_scan(tmp_path, "needle", "--corpus", str(path), "--scan-json", "--max-output-bytes", "4097")
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["matches"] == []
    assert payload["counts"] == [1]
    assert payload["outcome"]["complete"] is True
    assert re.fullmatch(
        r"warning: --max-output-bytes 4097 left out 1 line at corpus line 1;"
        r" raise it to \d+ to print every line whole\n",
        result.stderr,
    ), result.stderr


def test_corpus_cuts_an_oversized_window_and_prints_every_match_after_it(tmp_path: Path) -> None:
    path = tmp_path / "corpus.txt"
    path.write_text("".join(f"{line}\n" for line in ["early needle", "needle " + "x" * 40000, "later needle 0", "later needle 1"]))
    result = run_scan(tmp_path, "needle", "--corpus", str(path), "--max-output-bytes", "20000")
    assert result.returncode == 0, result.stderr
    early, cut, *later, summary = result.stdout.splitlines()
    assert (early, later) == ("early needle", ["later needle 0", "later needle 1"])
    assert cut.endswith(" [cut from 40007 bytes; read it whole with sed -n 2p ~/corpus.txt]")
    assert summary.startswith("4 matches in ")
    assert "shortened 1 line at corpus line 2" in result.stderr


def test_literal_grep_scans_raw_lines_without_checkpoints(tmp_path: Path) -> None:
    path = source(tmp_path / "live.jsonl", ["needle", *(f"filler {index} {'x' * 500}" for index in range(200))])
    for _ in range(2):
        result = run_scan(tmp_path, "needle", str(path), "--scan-json", "--max-matches", "0")
        assert result.returncode == 0, result.stderr
        payload = json.loads(result.stdout)
        assert payload["counts"] == [1]
        assert payload["outcome"]["complete"] is True
        progress = payload["outcome"]["progress"]
        assert progress["source_bytes"] == path.stat().st_size
        assert progress["parsed_events"] == 1
        assert progress["validated_bytes"] == progress["cache_hits"] == 0


def test_repeated_grep_on_an_appended_file_reads_only_new_bytes(tmp_path: Path) -> None:
    path = source(tmp_path / "live.jsonl", ["needle", *(f"filler {index} {'x' * 500}" for index in range(200))])
    cold = run_scan(tmp_path, ".?needle.?", str(path), "--scan-json", "--max-matches", "0")
    assert cold.returncode == 0, cold.stderr
    assert json.loads(cold.stdout)["outcome"]["progress"]["source_bytes"] == path.stat().st_size
    hit = len(path.read_bytes().split(b"\n")[0])
    before = path.stat().st_size
    appended = source(tmp_path / "suffix.jsonl", ["needle later", "tail"]).read_bytes()
    with path.open("ab") as handle:
        handle.write(appended)
    warm = run_scan(tmp_path, ".?needle.?", str(path), "--scan-json", "--max-matches", "0")
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
    cold = run_scan(tmp_path, ".?needle.?", str(path), "--scan-json")
    assert cold.returncode == 1, cold.stderr
    before = path.stat().st_size
    path.write_bytes(path.read_bytes().replace(b"filler 5 ", b"needle 5 "))
    with path.open("ab") as handle:
        handle.write(source(tmp_path / "suffix.jsonl", ["tail"]).read_bytes())
    warm = run_scan(tmp_path, ".?needle.?", str(path), "--scan-json")
    fresh_path = tmp_path / "fresh.jsonl"
    fresh_path.write_bytes(path.read_bytes())
    fresh = run_scan(tmp_path, ".?needle.?", str(fresh_path), "--scan-json")
    assert warm.returncode == fresh.returncode == 0, warm.stderr
    payload, expected = json.loads(warm.stdout), json.loads(fresh.stdout)
    assert payload["counts"] == expected["counts"] == [1]
    assert payload["outcome"]["complete"] is expected["outcome"]["complete"] is True
    progress = payload["outcome"]["progress"]
    assert progress["cache_hits"] == 0
    assert progress["cache_invalidations"] == 1
    assert progress["validated_bytes"] == before
