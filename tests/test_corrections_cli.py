"""The `cc-transcript corrections` group, driven through the installed console script
against a HOME-redirected ledger."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pathlib

CLI = Path(sys.executable).parent / "cc-transcript"
SESSION = "11111111-1111-1111-1111-111111111111"


def run_cli(home: pathlib.Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(CLI), "corrections", *args],
        capture_output=True,
        text=True,
        env=os.environ | {"HOME": str(home)},
    )


def rows_of(output: str) -> list[dict]:
    return [json.loads(line) for line in output.splitlines() if line.strip()]


def add_review(home: pathlib.Path, **extra: str) -> None:
    args = [
        "add",
        "--session", SESSION,
        "--source", "cc-review",
        "--anchor", "review:r1:7",
        "--origin", "review",
        "--incorrect-file", "/a.py",
        "--incorrect-new", "pip install x",
        "--correction-text", "use uv add",
        "--repo", "repo-a",
        "--ts-ms", "1000",
    ]
    for key, value in extra.items():
        args += [f"--{key.replace('_', '-')}", value]
    result = run_cli(home, *args)
    assert result.returncode == 0, result.stderr


def test_add_then_query_by_session(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path)
    result = run_cli(tmp_path, "query", "--session", SESSION)
    (row,) = rows_of(result.stdout)
    assert row["source"] == "cc-review"
    assert row["correction_text"] == "use uv add"
    assert row["incorrect_digest"] is None
    assert row["correction_origin"] == "review"
    assert row["detail"]["repo"] == "repo-a"


def test_query_by_repo_and_since(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path)
    assert len(rows_of(run_cli(tmp_path, "query", "--repo", "repo-a").stdout)) == 1
    assert rows_of(run_cli(tmp_path, "query", "--repo", "nope").stdout) == []
    assert len(rows_of(run_cli(tmp_path, "query", "--since", "999").stdout)) == 1
    assert rows_of(run_cli(tmp_path, "query", "--since", "1000").stdout) == []


def test_query_requires_a_selector(tmp_path: pathlib.Path) -> None:
    result = run_cli(tmp_path, "query")
    assert result.returncode == 2
    assert "one of --session" in result.stderr


def test_sql_escape_hatch(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path)
    result = run_cli(tmp_path, "sql", "SELECT source, correction_text FROM corrections")
    (row,) = rows_of(result.stdout)
    assert row == {"source": "cc-review", "correction_text": "use uv add"}


def test_python_facade_reads_what_the_cli_wrote(tmp_path: pathlib.Path) -> None:
    # Cross-process engine mixing is the supported mode (doc 3f9e034a): the Rust CLI
    # writes in one process, the Python facade's native engine reads in another.
    add_review(tmp_path)
    probe = (
        "from cc_transcript.corrections import CorrectionLog; "
        f"rows = CorrectionLog.open().for_session('{SESSION}'); "
        "print(len(rows), rows[0].correction_text)"
    )
    result = subprocess.run(
        [sys.executable, "-c", probe],
        capture_output=True,
        text=True,
        env=os.environ | {"HOME": str(tmp_path)},
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "1 use uv add"
