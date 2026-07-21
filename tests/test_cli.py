"""Behavioral tests for the Rust `cc-transcript` CLI, driven through the installed
console script. Output byte-parity lives in ``tests/testdata/cli_golden`` (recorded by
``scripts/record_cli_golden.py``); these tests cover the legs goldens cannot pin —
scratchpad resolution against live tmp roots, exit conventions, SIGPIPE, and argv
validation."""

from __future__ import annotations

import json
import math
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.filterspec import DENIAL_PREFIX, USER_SAID_MARKER, USER_SAID_TRAILER
from cc_transcript.ids import tool_digest

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
        "blame",
        "attribute",
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


BASE_TS = datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)
MODEL = "claude-opus-4-7"
WINDOW = ("--since", "2026-01-02T03:04:00Z", "--until", "2026-01-02T03:05:00Z")

EXPECTED_SHOW = (
    "    0 user  03:04:05 hello world",
    '    1 asst  03:04:06 [claude-opus-4-7] "hi there"',
    "    2 asst  03:04:07 [claude-opus-4-7] th(12ch) Read(/x)",
    "    3 user  03:04:08 <-Read (9ch) ok output",
    "    4 asst  03:04:09 [claude-opus-4-7] ls",
    "    5 user  03:04:10 <-Bash[err] (4ch) boom",
    "    6 user  03:04:11 [int] [Request interrupted by user]",
    "    7 user  03:04:12 <system-reminder>do not respond</system-reminder>",
    "    8 user* 03:04:13 subagent prompt",
    "    9 sys   03:04:14 stop_hook_summary: hook ran",
    "   10 mode           mode=normal",
    "   11 other          summary",
    "   12 user  03:04:17 final question",
)

EXPECTED_STATS = "\n".join(
    (
        "files        1",
        "events       13",
        "kinds        user 7 · assistant 3 · system 1 · mode 1 · other 1",
        "models       claude-opus-4-7 3",
        "tools        Read 1 · Bash 1",
        "text         126B",
        "thinking     12B",
        "tool io      47B",
        "sessions     1",
        "span         2026-01-02 03:04:05 → 2026-01-02 03:04:17",
        "interrupts   1",
        "tool errors  1",
        "sidechain    1",
    )
)

READ_SLICE = {
    "schema": "cc-transcript.slice/1",
    "event_uuid": "u2",
    "tool_use_id": "toolu_read",
    "ts_ms": int((BASE_TS + timedelta(seconds=2)).timestamp() * 1000),
    "tool_name": "Read",
    "tool_digest": tool_digest("Read", {"file_path": "/x"}),
    "file_path": "/x",
    "summary": "Read(/x)",
}

BASH_SLICE = {
    "schema": "cc-transcript.slice/1",
    "event_uuid": "u4",
    "tool_use_id": "toolu_bash",
    "ts_ms": int((BASE_TS + timedelta(seconds=4)).timestamp() * 1000),
    "tool_name": "Bash",
    "tool_digest": tool_digest("Bash", {"command": "ls"}),
    "file_path": None,
    "summary": "ls",
}


def envelope(n: int, **overrides: Any) -> dict[str, Any]:
    return {
        "uuid": f"u{n}",
        "parentUuid": None,
        "sessionId": "sess-1",
        "timestamp": (BASE_TS + timedelta(seconds=n)).isoformat(),
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "1.2.3",
        "isSidechain": False,
        "entrypoint": "cli",
    } | overrides


def user_entry(n: int, text: str, **overrides: Any) -> dict[str, Any]:
    return envelope(n, type="user", message={"role": "user", "content": text}, **overrides)


def assistant_entry(n: int, content: list[dict[str, Any]], *, stop_reason: str = "end_turn") -> dict[str, Any]:
    return envelope(
        n,
        type="assistant",
        message={"role": "assistant", "model": MODEL, "stop_reason": stop_reason, "content": content},
    )


def tool_result_entry(n: int, tool_use_id: str, content: str, *, is_error: bool) -> dict[str, Any]:
    return envelope(
        n,
        type="user",
        message={
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}],
        },
    )


def tool_use_entry(n: int, tool_use_id: str, name: str, **input: Any) -> dict[str, Any]:
    return assistant_entry(
        n, [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}], stop_reason="tool_use"
    )


def denial(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def fixture_entries() -> list[dict[str, Any]]:
    return [
        user_entry(0, "hello world"),
        assistant_entry(1, [{"type": "text", "text": "hi there"}]),
        assistant_entry(
            2,
            [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "/x"}},
            ],
            stop_reason="tool_use",
        ),
        tool_result_entry(3, "toolu_read", "ok output", is_error=False),
        assistant_entry(
            4,
            [{"type": "tool_use", "id": "toolu_bash", "name": "Bash", "input": {"command": "ls"}}],
            stop_reason="tool_use",
        ),
        tool_result_entry(5, "toolu_bash", "boom", is_error=True),
        user_entry(6, "[Request interrupted by user]"),
        user_entry(7, "<system-reminder>do not respond</system-reminder>"),
        user_entry(8, "subagent prompt", isSidechain=True),
        envelope(9, type="system", subtype="stop_hook_summary", content="hook ran"),
        {"type": "mode", "mode": "normal", "sessionId": "sess-1"},
        {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x"},
        user_entry(12, "final question"),
    ]


def rich_entries() -> list[dict[str, Any]]:
    return [
        user_entry(0, "do stuff"),
        tool_use_entry(1, "toolu_rm", "Bash", command="rm -rf /tmp/x"),
        tool_result_entry(2, "toolu_rm", denial("do not delete that"), is_error=True),
        tool_use_entry(3, "toolu_srch", "mcp__semble__search", query="x"),
        tool_result_entry(4, "toolu_srch", "results", is_error=False),
        tool_use_entry(5, "toolu_rel", "mcp__semble__find_related", ref="y"),
        tool_result_entry(6, "toolu_rel", "related", is_error=False),
        tool_use_entry(7, "toolu_dep", "mcp__railway__deploy"),
        tool_result_entry(8, "toolu_dep", "deployed", is_error=False),
    ]


def write_transcript(path: Path, entries: list[dict[str, Any]]) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\n".join(orjson.dumps(entry) for entry in entries) + b"\n")
    return path


@pytest.fixture
def transcript(tmp_path: Path) -> Path:
    return write_transcript(tmp_path / "t.jsonl", fixture_entries())


@pytest.fixture
def rich(tmp_path: Path) -> Path:
    return write_transcript(tmp_path / "rich.jsonl", rich_entries())


@pytest.fixture
def unparseable(tmp_path: Path) -> Path:
    bad = tmp_path / "bad.jsonl"
    bad.write_bytes(orjson.dumps(envelope(0, type="user", message={"role": "user", "content": None})) + b"\n")
    return bad


@pytest.fixture
def session_root(tmp_path: Path) -> Path:
    root = tmp_path / "projects"
    write_transcript(root / "-Users-x-proj-a" / "sess-1.jsonl", fixture_entries())
    return root


def test_show_renders_each_event_kind(transcript: Path) -> None:
    result = run_cli("show", str(transcript))
    assert result.returncode == 0, result.stderr
    assert tuple(result.stdout.splitlines()) == EXPECTED_SHOW


def test_show_caps_at_200_with_notice(tmp_path: Path) -> None:
    path = write_transcript(tmp_path / "big.jsonl", [user_entry(n, f"msg {n}") for n in range(205)])
    result = run_cli("show", str(path))
    lines = result.stdout.splitlines()
    assert result.returncode == 0
    assert lines[0] == "… 5 earlier events hidden — use --head/--range/--all"
    assert len(lines) == 201
    assert lines[1] == f"    5 user  {BASE_TS + timedelta(seconds=5):%H:%M:%S} msg 5"
    assert lines[-1] == f"  204 user  {BASE_TS + timedelta(seconds=204):%H:%M:%S} msg 204"
    assert len(run_cli("show", str(path), "--all").stdout.splitlines()) == 205


def test_show_head_tail_range_are_mutually_exclusive(transcript: Path) -> None:
    result = run_cli("show", str(transcript), "--head", "1", "--tail", "1")
    assert result.returncode == 2
    assert "--head, --tail, and --range are mutually exclusive" in result.stderr


def test_grep_kind_filter(transcript: Path) -> None:
    result = run_cli("grep", "o", str(transcript), "--kind", "system")
    assert result.returncode == 0
    assert result.stdout.splitlines() == [f"== {transcript}", EXPECTED_SHOW[9], "1 files, 1 matches"]


def test_grep_context_windows_and_separator(transcript: Path) -> None:
    result = run_cli("grep", "hello|final", str(transcript), "-C", "1")
    assert result.returncode == 0
    assert result.stdout.splitlines() == [
        f"== {transcript}",
        EXPECTED_SHOW[0],
        EXPECTED_SHOW[1],
        "--",
        EXPECTED_SHOW[11],
        EXPECTED_SHOW[12],
        "1 files, 2 matches",
    ]


def test_grep_with_result_human_appends_markers(rich: Path) -> None:
    result = run_cli("grep", "rm|query", str(rich), "--with-result")
    assert result.returncode == 0
    assert result.stdout.splitlines() == [
        f"== {rich}",
        "    1 asst  03:04:06 [claude-opus-4-7] rm -rf /tmp/x [denied] (1000ms)",
        "    3 asst  03:04:08 [claude-opus-4-7] mcp__semble__search(x) (1000ms)",
        "1 files, 2 matches",
    ]


def test_grep_with_result_json_adds_results_sibling(rich: Path) -> None:
    result = run_cli("grep", "rm|query", str(rich), "--json", "--with-result")
    assert result.returncode == 0
    by_i = {row["i"]: row for line in result.stdout.splitlines() if (row := json.loads(line))}
    assert by_i[1]["results"] == {"toolu_rm": {"is_error": True, "denied": True, "duration_ms": 1000}}
    assert by_i[3]["results"] == {"toolu_srch": {"is_error": False, "denied": False, "duration_ms": 1000}}


def test_stats_warns_on_unparseable_file(transcript: Path, unparseable: Path) -> None:
    result = run_cli("stats", str(transcript), str(unparseable))
    assert result.returncode == 0
    assert result.stderr == f"warning: skipped 1 unparseable transcript(s): {unparseable}\n"
    assert result.stdout == EXPECTED_STATS + "\n"


def test_slice_emits_one_line_per_tool_call(session_root: Path) -> None:
    result = run_cli("slice", "--session", "sess-1", *WINDOW, "--root", str(session_root))
    assert result.returncode == 0
    rows = [json.loads(line) for line in result.stdout.splitlines()]
    assert rows == [READ_SLICE, BASH_SLICE]


@pytest.mark.parametrize(
    ("since", "until", "expected_uuids"),
    [
        pytest.param("2026-01-02T03:04:07Z", "2026-01-02T03:04:09Z", ["u2"], id="until_exclusive"),
        pytest.param("2026-01-02T03:04:09Z", "2026-01-02T03:05:00Z", ["u4"], id="since_inclusive"),
    ],
)
def test_slice_window_boundaries(session_root: Path, since: str, until: str, expected_uuids: list[str]) -> None:
    result = run_cli("slice", "--session", "sess-1", "--since", since, "--until", until, "--root", str(session_root))
    assert result.returncode == 0
    assert [json.loads(line)["event_uuid"] for line in result.stdout.splitlines()] == expected_uuids


def test_slice_unparseable_transcript_exits_two_with_empty_stdout(tmp_path: Path) -> None:
    root = tmp_path / "projects"
    bad = root / "-Users-x-proj-a" / "sess-9.jsonl"
    bad.parent.mkdir(parents=True)
    bad.write_bytes(orjson.dumps(envelope(0, type="user", message={"role": "user", "content": None})) + b"\n")
    result = run_cli("slice", "--session", "sess-9", *WINDOW, "--root", str(root))
    assert result.returncode == 2
    assert result.stdout == ""


def test_digest_rejects_invalid_stdin() -> None:
    result = subprocess.run(
        [str(CLI), "digest"], capture_output=True, text=True, input="not json", env=dict(os.environ)
    )
    assert result.returncode == 2
    assert "invalid JSON on stdin" in result.stderr


def test_digest_check_missing_file_exits_two(tmp_path: Path) -> None:
    result = run_cli("digest", "--check", str(tmp_path / "nope.json"))
    assert result.returncode == 2
    assert "does not exist" in result.stderr


def test_tools_lists_every_call_with_outcomes(rich: Path) -> None:
    result = run_cli("tools", str(rich))
    assert result.returncode == 0
    assert result.stdout.splitlines() == [
        "2026-01-02 03:04:06 sess-1 Bash rm [denied]",
        "2026-01-02 03:04:08 sess-1 semble/search",
        "2026-01-02 03:04:10 sess-1 semble/find_related",
        "2026-01-02 03:04:12 sess-1 railway/deploy",
    ]


def test_tools_json_rows_carry_cwd_and_file_paths(rich: Path) -> None:
    result = run_cli("tools", str(rich), "--json")
    assert result.returncode == 0
    row = json.loads(result.stdout.splitlines()[0])
    assert row == {
        "ts": "2026-01-02T03:04:06+00:00",
        "session_id": "sess-1",
        "path": str(rich),
        "cwd": "/repo",
        "tool_use_id": "toolu_rm",
        "tool": "Bash",
        "command_prefixes": ["rm"],
        "command": "rm -rf /tmp/x",
        "mcp_server": None,
        "mcp_tool": None,
        "mcp_access": None,
        "file_path": None,
        "file_paths": [],
        "is_error": True,
        "denied": True,
        "denial_kind": "user-rejected",
        "user_said": "do not delete that",
        "duration_ms": 1000,
    }


def test_tools_file_glob_matches_any_path_of_a_multifile_call(tmp_path: Path) -> None:
    envelope = (
        "*** Begin Patch\n"
        "*** Update File: src/a.py\n"
        "@@\n"
        "-x\n"
        "+y\n"
        "*** Add File: src/b.py\n"
        "+created\n"
        "*** End Patch\n"
    )
    entries = [
        user_entry(0, "patch it"),
        assistant_entry(
            1,
            [{"type": "tool_use", "id": "toolu_patch", "name": "apply_patch", "input": envelope}],
            stop_reason="tool_use",
        ),
        tool_result_entry(2, "toolu_patch", "done", is_error=False),
    ]
    path = write_transcript(tmp_path / "patch.jsonl", entries)

    result = run_cli("tools", str(path), "--file", "src/b.py", "--json")

    assert result.returncode == 0
    rows = [json.loads(line) for line in result.stdout.splitlines()]
    assert len(rows) == 1
    assert rows[0]["tool"] == "apply_patch"
    assert rows[0]["file_paths"] == ["src/a.py", "src/b.py"]


def test_tools_since_until_window_bounds(tmp_path: Path) -> None:
    entries = [
        user_entry(0, "go"),
        tool_use_entry(1, "toolu_a", "Read", file_path="/a"),
        tool_result_entry(2, "toolu_a", "ok", is_error=False),
        tool_use_entry(3, "toolu_b", "Read", file_path="/b"),
        tool_result_entry(4, "toolu_b", "ok", is_error=False),
    ]
    path = write_transcript(tmp_path / "window.jsonl", entries)
    later = (BASE_TS + timedelta(seconds=3)).isoformat()

    since_result = run_cli("tools", str(path), "--since", later, "--json")
    assert since_result.returncode == 0
    since_ids = [json.loads(line)["tool_use_id"] for line in since_result.stdout.splitlines()]
    assert since_ids == ["toolu_b"]

    until_result = run_cli("tools", str(path), "--until", later, "--json")
    assert until_result.returncode == 0
    until_ids = [json.loads(line)["tool_use_id"] for line in until_result.stdout.splitlines()]
    assert until_ids == ["toolu_a"]


def test_tools_since_accepts_bare_date_and_duration(rich: Path) -> None:
    date_result = run_cli("tools", str(rich), "--since", "2020-01-01")
    assert date_result.returncode == 0
    assert len(date_result.stdout.splitlines()) == 4

    duration_result = run_cli("tools", str(rich), "--since", "1s")
    assert duration_result.returncode == 0
    assert duration_result.stdout == ""

    bogus_result = run_cli("tools", str(rich), "--since", "bogus")
    assert bogus_result.returncode == 2
    assert (
        "expected an RFC 3339 timestamp, a YYYY-MM-DD date, or a relative duration like 2d"
        in bogus_result.stderr
    )


def test_commands_human_counts(rich: Path) -> None:
    result = run_cli("commands", str(rich))
    assert result.returncode == 0
    assert result.stdout.splitlines() == ["  1  rm"]


def test_commands_json_rows(transcript: Path) -> None:
    result = run_cli("commands", str(transcript), "--json")
    assert result.returncode == 0
    assert [json.loads(line) for line in result.stdout.splitlines()] == [{"prefix": "ls", "count": 1}]


def test_permissions_human_line(rich: Path) -> None:
    result = run_cli("permissions", str(rich))
    assert result.returncode == 0
    assert result.stdout.splitlines() == ["Bash rm -rf /tmp/x → do not delete that"]


def test_permissions_json_row_shape(rich: Path) -> None:
    result = run_cli("permissions", str(rich), "--json")
    assert result.returncode == 0
    assert [json.loads(line) for line in result.stdout.splitlines()] == [
        {
            "ts": "2026-01-02T03:04:06+00:00",
            "session": "sess-1",
            "path": str(rich),
            "tool": "Bash",
            "command": "rm -rf /tmp/x",
            "file_path": None,
            "denial_kind": "user-rejected",
            "user_said": "do not delete that",
        }
    ]


def test_permissions_excludes_plan_and_question_rejections(tmp_path: Path) -> None:
    path = write_transcript(
        tmp_path / "plan.jsonl",
        [
            user_entry(0, "go"),
            tool_use_entry(1, "toolu_plan", "ExitPlanMode", plan="do it"),
            tool_result_entry(2, "toolu_plan", denial("not yet"), is_error=True),
            tool_use_entry(3, "toolu_ask", "AskUserQuestion", questions=[]),
            tool_result_entry(4, "toolu_ask", denial("skip"), is_error=True),
            tool_use_entry(5, "toolu_rm", "Bash", command="rm -rf /x"),
            tool_result_entry(6, "toolu_rm", denial("stop"), is_error=True),
        ],
    )
    result = run_cli("permissions", str(path), "--json")
    assert result.returncode == 0
    assert [json.loads(line)["tool"] for line in result.stdout.splitlines()] == ["Bash"]


def test_mcp_human_table_ordered_by_total(rich: Path) -> None:
    result = run_cli("mcp", str(rich))
    assert result.returncode == 0
    assert result.stdout.splitlines() == [
        "semble   read 2 · write 0 · total 2  search 1 · find_related 1",
        "railway  read 0 · write 1 · total 1  deploy 1",
    ]


def test_mcp_json_rows(rich: Path) -> None:
    result = run_cli("mcp", str(rich), "--json")
    assert result.returncode == 0
    assert [json.loads(line) for line in result.stdout.splitlines()] == [
        {"server": "semble", "read": 2, "write": 0, "total": 2, "tools": {"search": 1, "find_related": 1}},
        {"server": "railway", "read": 0, "write": 1, "total": 1, "tools": {"deploy": 1}},
    ]


def test_root_option_rejects_a_file(tmp_path: Path) -> None:
    file_root = tmp_path / "not-a-dir"
    file_root.write_text("x")
    for args in (["list"], ["watch", "--poll", "0.05"], ["slice", "--session", "s", *WINDOW]):
        result = run_cli(*args, "--root", str(file_root))
        assert result.returncode == 2, args
        assert "is a file." in result.stderr, args


def test_bare_invocation_prints_help_to_stderr_and_exits_two() -> None:
    result = run_cli()
    assert result.returncode == 2
    assert result.stdout == ""
    assert "Usage: cc-transcript" in result.stderr


def test_show_json_floats_match_orjson_layout(tmp_path: Path) -> None:
    """The ids shortest-repr fuzz extended to the CLI writer: 10k seeded bit-random
    f64s plus the exponent edges must survive an orjson re-serialization byte-for-byte
    (orjson is the reference the deleted Python CLI emitted through)."""
    rng = random.Random(0xC0FFEE)
    values = [698957826421429.2, 0.0001, 0.00001, 0.0000999, -0.00001, 1e-6, 1e-7, 1e15, 1e16, 5e-324, -0.0]
    while len(values) < 10_000:
        candidate = struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0]
        if math.isfinite(candidate):
            values.append(candidate)
    path = write_transcript(tmp_path / "floats.jsonl", [tool_use_entry(0, "toolu_f", "Bash", xs=values)])
    result = run_cli("show", str(path), "--json")
    assert result.returncode == 0, result.stderr
    (line,) = result.stdout.splitlines()
    assert orjson.dumps(orjson.loads(line)).decode() == line
    assert json.loads(line)["blocks"][0]["input"]["xs"] == values


def test_show_json_nonfinite_lexemes_project_null(tmp_path: Path) -> None:
    entry = tool_use_entry(0, "toolu_n", "Bash", big="BIG", neg="NEG", nan_x="NAN")
    raw = orjson.dumps(entry).replace(b'"BIG"', b"1e400").replace(b'"NEG"', b"-1e999").replace(b'"NAN"', b"2e308")
    path = tmp_path / "nonfinite.jsonl"
    path.write_bytes(raw + b"\n")
    result = run_cli("show", str(path), "--json")
    assert result.returncode == 0, result.stderr
    (line,) = result.stdout.splitlines()
    assert json.loads(line)["blocks"][0]["input"] == {"big": None, "neg": None, "nan_x": None}


def test_home_unset_falls_back_to_the_pwd_home(tmp_path: Path) -> None:
    decoy = tmp_path / ".claude" / "projects" / "-x-proj"
    write_transcript(decoy / "decoy-session.jsonl", [user_entry(0, "decoy")])
    result = run_cli("list", "--limit", "5", drop=("HOME",), cwd=tmp_path)
    assert result.returncode == 0
    assert "decoy-session" not in result.stdout


BLAME_MODEL = "claude-opus-4-8"


def encode_dir(cwd: str) -> str:
    return cwd.replace("/", "-").replace(".", "-")


def blame_repo(tmp_path: Path) -> tuple[Path, Path]:
    tree = tmp_path / "tree"
    (tree / ".git").mkdir(parents=True)
    (tree / "src").mkdir(parents=True)
    (tree / "build").mkdir(parents=True)
    (tree / "src" / "app.py").write_text("app\n")
    (tree / "build" / "out.txt").write_text("built\n")
    (tree / "README.md").write_text("# readme\n")
    projects = tmp_path / "projects"
    projects.mkdir()
    return tree, projects


def prompt_entry(session_id: str, cwd: str, ts: str, text: str, uid: str) -> dict[str, Any]:
    return envelope(
        0, uuid=uid, type="user", sessionId=session_id, cwd=cwd, timestamp=ts,
        message={"role": "user", "content": text},
    )


def call_entry(session_id: str, cwd: str, ts: str, block: dict[str, Any], uid: str, *, stop: str = "tool_use") -> dict[str, Any]:
    return envelope(
        0, uuid=uid, type="assistant", sessionId=session_id, cwd=cwd, timestamp=ts,
        message={"role": "assistant", "model": BLAME_MODEL, "stop_reason": stop, "content": [block]},
    )


def edit_block(tool_use_id: str, file_path: str) -> dict[str, Any]:
    return {"type": "tool_use", "id": tool_use_id, "name": "Edit",
            "input": {"file_path": file_path, "old_string": "app", "new_string": "app2"}}


def patch_block(tool_use_id: str, files: list[str]) -> dict[str, Any]:
    body = "".join(f"*** Update File: {f}\n@@\n-a\n+b\n" for f in files)
    return {"type": "tool_use", "id": tool_use_id, "name": "apply_patch",
            "input": f"*** Begin Patch\n{body}*** End Patch\n"}


def bash_block(tool_use_id: str, command: str) -> dict[str, Any]:
    return {"type": "tool_use", "id": tool_use_id, "name": "Bash", "input": {"command": command}}


def blame_session(projects: Path, session_id: str, cwd: str, entries: list[dict[str, Any]]) -> Path:
    return write_transcript(projects / encode_dir(cwd) / f"{session_id}.jsonl", entries)


def test_blame_orders_newest_first_with_worktree_labels(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    worktree = tree / ".claude" / "worktrees" / "wt1"
    (worktree / "src").mkdir(parents=True)
    (worktree / ".git").write_text("gitdir: x\n")
    (worktree / "src" / "app.py").write_text("app\n")
    tree_abs = str(tree.resolve())
    wt_abs = str(worktree.resolve())
    main = blame_session(projects, "s-main", tree_abs, [
        prompt_entry("s-main", tree_abs, "2026-05-01T09:00:00Z", "add the blame verb", "m0"),
        call_entry("s-main", tree_abs, "2026-05-01T09:05:00Z", edit_block("te", f"{tree_abs}/src/app.py"), "m1"),
    ])
    wt = blame_session(projects, "s-wt", wt_abs, [
        prompt_entry("s-wt", wt_abs, "2026-05-02T14:00:00Z", "wire it in", "w0"),
        call_entry("s-wt", wt_abs, "2026-05-02T14:10:00Z", patch_block("tp", ["src/other.py", "src/app.py"]), "w1"),
    ])

    result = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects))
    assert result.returncode == 0, result.stderr
    assert result.stdout.splitlines() == [
        "2026-05-02 14:10:00 s-wt worktree:wt1 1w apply_patch — wire it in",
        "2026-05-01 09:05:00 s-main main 1w Edit — add the blame verb",
    ]

    js = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects), "--json")
    assert js.returncode == 0, js.stderr
    assert [json.loads(line) for line in js.stdout.splitlines()] == [
        {
            "session_id": "s-wt", "path": str(wt), "tree": "worktree:wt1",
            "first_write_ts": "2026-05-02T14:10:00+00:00", "last_write_ts": "2026-05-02T14:10:00+00:00",
            "writes": 1, "tools": ["apply_patch"], "first_prompt": "wire it in",
        },
        {
            "session_id": "s-main", "path": str(main), "tree": "main",
            "first_write_ts": "2026-05-01T09:05:00+00:00", "last_write_ts": "2026-05-01T09:05:00+00:00",
            "writes": 1, "tools": ["Edit"], "first_prompt": "add the blame verb",
        },
    ]


def test_blame_counts_applypatch_secondary_files(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    blame_session(projects, "s1", tree_abs, [
        prompt_entry("s1", tree_abs, "2026-05-01T09:00:00Z", "patch", "p0"),
        call_entry("s1", tree_abs, "2026-05-01T09:05:00Z", patch_block("tp", ["src/other.py", "src/app.py"]), "p1"),
    ])
    result = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects), "--json")
    assert result.returncode == 0, result.stderr
    row = json.loads(result.stdout)
    assert (row["writes"], row["tools"], row["tree"]) == (1, ["apply_patch"], "main")


def test_blame_excludes_prefix_collided_sibling_repo(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    sibling = f"{tree_abs}-rust"
    blame_session(projects, "s-real", tree_abs, [
        prompt_entry("s-real", tree_abs, "2026-05-01T09:00:00Z", "real", "r0"),
        call_entry("s-real", tree_abs, "2026-05-01T09:05:00Z", edit_block("te", f"{tree_abs}/src/app.py"), "r1"),
    ])
    blame_session(projects, "s-rust", sibling, [
        prompt_entry("s-rust", sibling, "2026-05-02T09:00:00Z", "sibling", "x0"),
        call_entry("s-rust", sibling, "2026-05-02T09:05:00Z", edit_block("xe", "src/app.py"), "x1"),
    ])
    result = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects), "--json")
    assert result.returncode == 0, result.stderr
    assert [json.loads(line)["session_id"] for line in result.stdout.splitlines()] == ["s-real"]


def test_blame_all_projects_finds_misfiled_transcripts(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    write_transcript(projects / "-unrelated-project" / "s-misfiled.jsonl", [
        prompt_entry("s-misfiled", tree_abs, "2026-05-01T09:00:00Z", "misfiled", "u0"),
        call_entry("s-misfiled", tree_abs, "2026-05-01T09:05:00Z", edit_block("te", f"{tree_abs}/src/app.py"), "u1"),
    ])
    without = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects))
    assert without.returncode == 1
    assert "no sessions wrote src/app.py" in without.stderr

    with_all = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects), "--all-projects", "--json")
    assert with_all.returncode == 0, with_all.stderr
    assert [json.loads(line)["session_id"] for line in with_all.stdout.splitlines()] == ["s-misfiled"]


def test_blame_missing_file_exits_two(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    result = run_cli("blame", f"{tree_abs}/src/nope.py", "--root", str(projects))
    assert result.returncode == 2
    assert "does not exist" in result.stderr


def test_blame_outside_repo_exits_one(tmp_path: Path) -> None:
    outside = Path(tempfile.mkdtemp())
    try:
        target = outside / "loose.py"
        target.write_text("x\n")
        result = run_cli("blame", str(target), "--root", str(tmp_path))
        assert result.returncode == 1
        assert "not inside a repository" in result.stderr
    finally:
        shutil.rmtree(outside, ignore_errors=True)


def test_blame_since_excluding_everything_exits_one(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    blame_session(projects, "s1", tree_abs, [
        prompt_entry("s1", tree_abs, "2026-05-01T09:00:00Z", "go", "g0"),
        call_entry("s1", tree_abs, "2026-05-01T09:05:00Z", edit_block("te", f"{tree_abs}/src/app.py"), "g1"),
    ])
    result = run_cli("blame", f"{tree_abs}/src/app.py", "--root", str(projects), "--since", "2099-01-01T00:00:00Z")
    assert result.returncode == 1
    assert "no sessions wrote src/app.py" in result.stderr


def test_attribute_claude_json_shape(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    mtime = datetime(2026, 5, 1, 9, 5, 0, tzinfo=UTC)
    os.utime(tree / "src" / "app.py", (mtime.timestamp(), mtime.timestamp()))
    transcript = blame_session(projects, "s-main", tree_abs, [
        prompt_entry("s-main", tree_abs, "2026-05-01T09:00:00Z", "add it", "m0"),
        call_entry("s-main", tree_abs, "2026-05-01T09:05:00Z", edit_block("te", f"{tree_abs}/src/app.py"), "m1"),
    ])
    result = run_cli("attribute", f"{tree_abs}/src/app.py", "--root", str(projects), "--json")
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {
        "file": "src/app.py",
        "mtime": mtime.isoformat(),
        "verdict": "claude",
        "session_id": "s-main",
        "evidence": {
            "ts": "2026-05-01T09:05:00+00:00",
            "tool": "Edit",
            "tool_use_id": "te",
            "tree": "main",
            "path": str(transcript),
        },
    }


def test_attribute_generated_window_and_bash_suspects(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    mtime = datetime(2026, 5, 3, 10, 20, 0, tzinfo=UTC)
    os.utime(tree / "build" / "out.txt", (mtime.timestamp(), mtime.timestamp()))
    commands = [f"cmd {i}" for i in range(6)]
    entries = [prompt_entry("s-bash", tree_abs, "2026-05-03T10:00:00Z", "build", "b0")]
    entries += [
        call_entry("s-bash", tree_abs, f"2026-05-03T10:0{i}:00Z", bash_block(f"tb{i}", command), f"bb{i}")
        for i, command in enumerate(commands)
    ]
    entries.append(call_entry("s-bash", tree_abs, "2026-05-03T10:30:00Z", {"type": "text", "text": "done"}, "bz", stop="end_turn"))
    blame_session(projects, "s-bash", tree_abs, entries)

    result = run_cli("attribute", f"{tree_abs}/build/out.txt", "--root", str(projects), "--json")
    assert result.returncode == 0, result.stderr
    row = json.loads(result.stdout)
    assert row["verdict"] == "generated"
    assert len(row["candidates"]) == 1
    suspects = row["candidates"][0]["bash"]
    stamps = [entry["ts"] for entry in suspects]
    assert stamps == sorted(stamps)
    assert [entry["command"] for entry in suspects] == commands[1:]


def test_attribute_external_outside_all_windows(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    os.utime(tree / "README.md", (datetime(2020, 1, 1, tzinfo=UTC).timestamp(),) * 2)
    blame_session(projects, "s-bash", tree_abs, [
        prompt_entry("s-bash", tree_abs, "2026-05-03T10:00:00Z", "build", "b0"),
        call_entry("s-bash", tree_abs, "2026-05-03T10:05:00Z", bash_block("tb", "make"), "b1"),
    ])
    result = run_cli("attribute", f"{tree_abs}/README.md", "--root", str(projects))
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "external"


def test_attribute_missing_file_exits_two(tmp_path: Path) -> None:
    tree, projects = blame_repo(tmp_path)
    tree_abs = str(tree.resolve())
    result = run_cli("attribute", f"{tree_abs}/src/nope.py", "--root", str(projects))
    assert result.returncode == 2
    assert "does not exist" in result.stderr
