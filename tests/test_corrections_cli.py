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


def test_detail_round_trips_python_json_bytes(tmp_path: pathlib.Path) -> None:
    detail = {"n": float("nan"), "i": float("inf"), "ni": float("-inf"), "k": "éé 🤖"}
    add_review(tmp_path, detail='{"n": NaN, "i": Infinity, "ni": -Infinity, "k": "éé 🤖"}')
    stored = rows_of(run_cli(tmp_path, "sql", "SELECT detail_json FROM corrections").stdout)
    assert stored == [{"detail_json": json.dumps(detail | {"repo": "repo-a"})}]
    result = run_cli(tmp_path, "query", "--session", SESSION)
    assert result.returncode == 0, result.stderr
    (row,) = rows_of(result.stdout)
    assert row["detail"] == {"n": None, "i": None, "ni": None, "k": "éé 🤖", "repo": "repo-a"}


def test_empty_options_keep_python_truthiness(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path, detail="", incorrect_digest="")
    result = run_cli(tmp_path, "sql", "SELECT detail_json, incorrect_digest FROM corrections")
    (row,) = rows_of(result.stdout)
    assert row == {"detail_json": json.dumps({"repo": "repo-a"}), "incorrect_digest": None}


def test_empty_repo_is_omitted_from_detail(tmp_path: pathlib.Path) -> None:
    result = run_cli(
        tmp_path,
        "add",
        "--session", SESSION,
        "--source", "cc-review",
        "--anchor", "review:r1:8",
        "--incorrect-file", "/a.py",
        "--ts-ms", "1000",
        "--repo", "",
        "--detail", "",
    )
    assert result.returncode == 0, result.stderr
    stored = rows_of(run_cli(tmp_path, "sql", "SELECT detail_json FROM corrections").stdout)
    assert stored == [{"detail_json": "{}"}]


def test_sql_duplicate_columns_keep_the_first(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path)
    result = run_cli(tmp_path, "sql", "SELECT 1 AS x, 2 AS x")
    assert result.returncode == 0
    assert rows_of(result.stdout) == [{"x": 1}]


def test_sql_blob_column_fails_loud(tmp_path: pathlib.Path) -> None:
    add_review(tmp_path)
    result = run_cli(tmp_path, "sql", "SELECT X'ff' AS x")
    assert result.returncode == 1
    assert result.stdout == ""
    assert "cannot serialize BLOB column" in result.stderr


def test_python_facade_reads_what_the_cli_wrote(tmp_path: pathlib.Path) -> None:
    # Cross-process engine mixing is the supported mode (doc 3f9e034a): the Rust CLI
    # writes in one process, the Python facade's native engine reads in another.
    add_review(tmp_path)
    probe = (
        "import asyncio\n"
        "from cc_transcript.corrections import CorrectionLog\n"
        "async def main():\n"
        f"    log = await CorrectionLog.open()\n"
        f"    rows = await log.for_session('{SESSION}')\n"
        "    print(len(rows), rows[0].correction_text)\n"
        "asyncio.run(main())\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", probe],
        capture_output=True,
        text=True,
        env=os.environ | {"HOME": str(tmp_path)},
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "1 use uv add"
