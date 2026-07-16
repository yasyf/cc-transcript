"""The committed CLI goldens are live: re-record the full matrix through the installed
console script into a tmp dir and byte-compare every stdout, stderr, and exit code
against ``tests/testdata/cli_golden``. Drift means either a behavior break or a
deliberate change that must re-record the goldens in the same commit."""

from __future__ import annotations

from typing import TYPE_CHECKING

from scripts.record_cli_golden import GOLDEN_DIR, record

if TYPE_CHECKING:
    from pathlib import Path


def test_golden_matrix_matches_the_committed_recording(tmp_path: Path) -> None:
    record(tmp_path)
    fresh = {p.name: p.read_bytes() for p in tmp_path.iterdir()}
    committed = {p.name: p.read_bytes() for p in GOLDEN_DIR.iterdir()}
    assert sorted(fresh) == sorted(committed)
    for name, body in sorted(committed.items()):
        assert fresh[name] == body, f"golden drift in {name}"
