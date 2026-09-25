"""The committed CLI goldens are live: re-record the full matrix through the installed
console script into a tmp dir and byte-compare every stdout, stderr, and exit code
against ``tests/testdata/cli_golden``. Drift means either a behavior break or a
deliberate change that must re-record the goldens in the same commit."""

from __future__ import annotations

import json
from typing import TYPE_CHECKING

from scripts.record_cli_golden import GOLDEN_DIR, Case, record, run

if TYPE_CHECKING:
    from pathlib import Path


def test_golden_matrix_matches_the_committed_recording(tmp_path: Path) -> None:
    record(tmp_path)
    fresh = {p.name: p.read_bytes() for p in tmp_path.iterdir()}
    committed = {p.name: p.read_bytes() for p in GOLDEN_DIR.iterdir()}
    assert sorted(fresh) == sorted(committed)
    for name, body in sorted(committed.items()):
        assert fresh[name] == body, f"golden drift in {name}"


def test_default_grep_budget_reports_incomplete_for_the_full_fixture() -> None:
    stdout, _, code = run(Case(["grep", "zzzz_no_such_pattern_xyzzy", "--root", ".fixtures/corpus", "--scan-json"]))
    payload = json.loads(stdout)
    assert code == 3
    assert payload["counts"] == [0]
    outcome = payload["outcome"]
    assert outcome["complete"] is False
    assert "budget" in outcome["reason"]
    progress = outcome["progress"]
    assert progress["source_bytes"] + progress["projection_bytes"] <= 8 * 1024 * 1024
    assert progress["parsed_events"] + progress["preparation_reserved_events"] + progress["examined_events"] <= 4096
    assert progress["staging_peak_reserved_bytes"] > 0
