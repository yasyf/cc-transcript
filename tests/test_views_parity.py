from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path

import pytest

from cc_transcript import _native
from cc_transcript.parser import parse
from tests import viewgolden

REPO_ROOT = Path(__file__).resolve().parent.parent
GOLDEN = json.loads((REPO_ROOT / "tests" / "testdata" / "views_golden.json").read_text())


def ensure_fixture(path: Path) -> Path:
    if not path.exists() and ".fixtures" in path.parts:
        subprocess.run(
            [sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")],
            check=True,
            cwd=REPO_ROOT,
            capture_output=True,
        )
    return path


def test_golden_version() -> None:
    assert GOLDEN["version"] == viewgolden.GOLDEN_VERSION
    assert GOLDEN["str_cap"] == viewgolden.STR_CAP


@pytest.mark.parametrize("record", GOLDEN["files"], ids=[r["file"] for r in GOLDEN["files"]])
def test_views_reproduce_golden(record: dict) -> None:
    path = ensure_fixture(REPO_ROOT / record["file"])
    assert hashlib.sha256(path.read_bytes()).hexdigest() == record["sha256"], f"{record['file']} drifted"
    viewgolden.replay_file(record, list(parse(path).events))


@pytest.mark.parametrize("record", GOLDEN["print_results"], ids=[r["file"] for r in GOLDEN["print_results"]])
def test_print_result_views_reproduce_golden(record: dict) -> None:
    raw = (REPO_ROOT / record["file"]).read_bytes()
    assert hashlib.sha256(raw).hexdigest() == record["sha256"], f"{record['file']} drifted"
    viewgolden.compare(record["proj"], _native.parse_print_result(raw))


def test_split_invariance() -> None:
    # captain-hook's transcache contract: parse(prefix) + parse(suffix) == parse(prefix + suffix).
    raw = (REPO_ROOT / "tests" / "testdata" / "views_edge" / "edge_core.jsonl").read_bytes()
    lines = raw.splitlines(keepends=True)
    whole = list(parse(REPO_ROOT / "tests" / "testdata" / "views_edge" / "edge_core.jsonl").events)
    for cut_line in (1, len(lines) // 2, len(lines) - 1):
        prefix = b"".join(lines[:cut_line])
        suffix = b"".join(lines[cut_line:])
        stitched = parse_bytes_via_stream(prefix) + parse_bytes_via_stream(suffix)
        assert len(stitched) == len(whole), f"cut at line {cut_line}"
        for a, b in zip(stitched, whole, strict=True):
            assert type(a) is type(b), f"cut at line {cut_line}"
            assert a == b, f"cut at line {cut_line}: {a!r} != {b!r}"


def parse_bytes_via_stream(raw: bytes) -> list:
    import tempfile

    with tempfile.NamedTemporaryFile(suffix=".jsonl", delete=False) as f:
        f.write(raw)
        path = Path(f.name)
    try:
        return list(parse(path).events)
    finally:
        path.unlink()
