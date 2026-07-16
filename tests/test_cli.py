"""Behavioral tests for the Rust `cc-transcript` CLI, driven through the installed
console script. Output byte-parity lives in ``tests/testdata/cli_golden`` (recorded by
``scripts/record_cli_golden.py``); these tests cover the legs goldens cannot pin —
scratchpad resolution against live tmp roots, exit conventions, SIGPIPE, and argv
validation."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

CLI = Path(sys.executable).parent / "cc-transcript"
SCRATCHPAD_SESSION = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
OTHER_SCRATCHPAD_SESSION = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
MISSING_SCRATCHPAD_SESSION = "cccccccc-cccc-cccc-cccc-cccccccccccc"


def run_cli(
    *args: str,
    env: dict[str, str] | None = None,
    drop: tuple[str, ...] = (),
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    merged = {k: v for k, v in os.environ.items() if k not in drop} | (env or {})
    return subprocess.run([str(CLI), *args], capture_output=True, text=True, env=merged, cwd=cwd)


def test_help_lists_all_commands() -> None:
    result = run_cli("--help")
    assert result.returncode == 0
    for command in (
        "list",
        "show",
        "grep",
        "stats",
        "slice",
        "scratchpad",
        "digest",
        "corrections",
        "tools",
        "commands",
        "permissions",
        "mcp",
        "watch",
    ):
        assert f"\n  {command}" in result.stdout, f"--help is missing {command}"


def test_version_reports_the_package_version() -> None:
    result = run_cli("--version")
    assert result.returncode == 0
    assert result.stdout.startswith("cc-transcript ")


def test_list_survives_a_closed_pipe() -> None:
    with subprocess.Popen(
        [str(CLI), "list", "--root", ".fixtures/corpus"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=Path(__file__).resolve().parent.parent,
    ) as proc:
        assert proc.stdout is not None and proc.stderr is not None
        proc.stdout.readline()
        proc.stdout.close()
        stderr = proc.stderr.read()
        assert proc.wait(timeout=30) == 0
    assert b"panic" not in stderr


def test_slice_unknown_session_exits_one_with_empty_output(tmp_path: Path) -> None:
    result = run_cli(
        "slice",
        "--session", MISSING_SCRATCHPAD_SESSION,
        "--since", "2020-01-01T00:00:00Z",
        "--until", "2030-01-01T00:00:00Z",
        "--root", str(tmp_path),
    )
    assert (result.returncode, result.stdout, result.stderr) == (1, "", "")


@pytest.mark.parametrize(
    ("value", "fragment"),
    [
        pytest.param("2020-01-01T00:00:00", "RFC 3339 requires a UTC offset", id="naive"),
        pytest.param("not-a-time", "expected an RFC 3339 timestamp", id="garbage"),
    ],
)
def test_slice_rejects_bad_timestamps(tmp_path: Path, value: str, fragment: str) -> None:
    result = run_cli(
        "slice",
        "--session", MISSING_SCRATCHPAD_SESSION,
        "--since", value,
        "--until", "2030-01-01T00:00:00Z",
        "--root", str(tmp_path),
    )
    assert result.returncode == 2
    assert fragment in result.stderr


def scratchpad_env(tmp_root: Path) -> dict[str, str]:
    return {"TMPDIR": str(tmp_root)}


def real(path: Path) -> Path:
    return Path(os.path.realpath(path))


def test_scratchpad_glob_hit_precedes_formula_fallback(tmp_path: Path) -> None:
    cwd = tmp_path / "working-copy"
    slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(real(cwd)))
    formula = real(tmp_path) / f"claude-{os.getuid()}" / slug / SCRATCHPAD_SESSION / "scratchpad"
    glob_match = real(tmp_path) / f"claude-{os.getuid()}" / "other-slug" / SCRATCHPAD_SESSION / "scratchpad"
    cwd.mkdir()
    formula.mkdir(parents=True)
    glob_match.mkdir(parents=True)
    os.utime(formula, (1_000_000.0, 1_000_000.0))
    os.utime(glob_match, (2_000_000.0, 2_000_000.0))

    result = run_cli(
        "scratchpad", "--session", SCRATCHPAD_SESSION, env=scratchpad_env(tmp_path), cwd=cwd
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == f"{glob_match}\n"


@pytest.mark.parametrize(
    ("args", "expected_session"),
    [
        pytest.param(["--session", SCRATCHPAD_SESSION], SCRATCHPAD_SESSION, id="flag_wins"),
        pytest.param([], OTHER_SCRATCHPAD_SESSION, id="env_default"),
    ],
)
def test_scratchpad_session_flag_and_env(tmp_path: Path, args: list[str], expected_session: str) -> None:
    base = real(tmp_path) / f"claude-{os.getuid()}" / "cwd-slug"
    expected = base / expected_session / "scratchpad"
    (base / SCRATCHPAD_SESSION / "scratchpad").mkdir(parents=True)
    (base / OTHER_SCRATCHPAD_SESSION / "scratchpad").mkdir(parents=True)

    result = run_cli(
        "scratchpad",
        *args,
        env=scratchpad_env(tmp_path) | {"CLAUDE_CODE_SESSION_ID": OTHER_SCRATCHPAD_SESSION},
    )

    assert result.returncode == 0, result.stderr
    assert result.stdout == f"{expected}\n"


def test_scratchpad_missing_exits_one_with_empty_stdout(tmp_path: Path) -> None:
    result = run_cli(
        "scratchpad", "--session", MISSING_SCRATCHPAD_SESSION, env=scratchpad_env(tmp_path)
    )

    assert result.returncode == 1
    assert result.stdout == ""
    assert "scratchpad not found" in result.stderr


def test_scratchpad_requires_session() -> None:
    result = run_cli("scratchpad", env={"CLAUDE_CODE_SESSION_ID": ""})

    assert result.returncode == 2
    assert result.stdout == ""
    assert "Missing option '--session'" in result.stderr


@pytest.mark.parametrize("session", ["*", "../x"])
def test_scratchpad_rejects_malformed_session(session: str) -> None:
    result = run_cli("scratchpad", "--session", session)

    assert result.returncode == 2
    assert result.stdout == ""
    assert "expected a UUID" in result.stderr
