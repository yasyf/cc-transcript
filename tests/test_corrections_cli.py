from __future__ import annotations

import json
from typing import TYPE_CHECKING

from click.testing import CliRunner

from cc_transcript.corrections_cli import corrections

if TYPE_CHECKING:
    import pathlib

    import pytest

SESSION = "11111111-1111-1111-1111-111111111111"


def rows_of(output: str) -> list[dict]:
    return [json.loads(line) for line in output.splitlines() if line.strip()]


def add_review(runner: CliRunner, **extra: str) -> None:
    args = [
        "add",
        "--session",
        SESSION,
        "--source",
        "cc-review",
        "--anchor",
        "review:r1:7",
        "--origin",
        "review",
        "--incorrect-file",
        "/a.py",
        "--incorrect-new",
        "pip install x",
        "--correction-text",
        "use uv add",
        "--repo",
        "repo-a",
        "--ts-ms",
        "1000",
    ]
    for key, value in extra.items():
        args += [f"--{key.replace('_', '-')}", value]
    result = runner.invoke(corrections, args, catch_exceptions=False)
    assert result.exit_code == 0, result.output


def test_add_then_query_by_session(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    runner = CliRunner()
    add_review(runner)
    result = runner.invoke(corrections, ["query", "--session", SESSION], catch_exceptions=False)
    (row,) = rows_of(result.output)
    assert row["source"] == "cc-review"
    assert row["correction_text"] == "use uv add"
    assert row["incorrect_digest"] is None
    assert row["correction_origin"] == "review"
    assert row["detail"]["repo"] == "repo-a"


def test_query_by_repo_and_since(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    runner = CliRunner()
    add_review(runner)
    by_repo = runner.invoke(corrections, ["query", "--repo", "repo-a"], catch_exceptions=False)
    assert len(rows_of(by_repo.output)) == 1
    assert rows_of(runner.invoke(corrections, ["query", "--repo", "nope"], catch_exceptions=False).output) == []
    since = runner.invoke(corrections, ["query", "--since", "999"], catch_exceptions=False)
    assert len(rows_of(since.output)) == 1
    assert rows_of(runner.invoke(corrections, ["query", "--since", "1000"], catch_exceptions=False).output) == []


def test_query_requires_a_selector(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    result = CliRunner().invoke(corrections, ["query"])
    assert result.exit_code != 0
    assert "one of --session" in result.output


def test_sql_escape_hatch(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    runner = CliRunner()
    add_review(runner)
    result = runner.invoke(
        corrections, ["sql", "SELECT source, correction_text FROM corrections"], catch_exceptions=False
    )
    (row,) = rows_of(result.output)
    assert row == {"source": "cc-review", "correction_text": "use uv add"}
