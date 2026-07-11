from __future__ import annotations

import os
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import orjson
import pytest
from click.testing import CliRunner

from cc_transcript.cli import cli
from cc_transcript.filterspec import DENIAL_PREFIX, USER_SAID_MARKER, USER_SAID_TRAILER
from cc_transcript.ids import tool_digest
from cc_transcript.parser import TranscriptParser
from cc_transcript.render import human_size

BASE_TS = datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC)
MODEL = "claude-opus-4-7"
WINDOW = ("--since", "2026-01-02T03:04:00Z", "--until", "2026-01-02T03:05:00Z")

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

DIGEST_CASES = [
    {"tool": "Bash", "input": {"command": "ls"}},
    {"tool": "Edit", "input": {"file_path": "/tmp/é.py", "old_string": "x", "new_string": "y", "ratio": 1.5}},
    {"tool": "mcp__github__search", "input": {"nested": [{"empty": {}}, [1, 2.5, "three"]]}},
]

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

USER0_DICT = {
    "i": 0,
    "kind": "user",
    "meta": {
        "uuid": "u0",
        "parent_uuid": None,
        "session_id": "sess-1",
        "timestamp": "2026-01-02T03:04:05+00:00",
        "cwd": "/repo",
        "git_branch": "main",
        "cc_version": "1.2.3",
        "is_sidechain": False,
        "is_meta": False,
        "entrypoint": "cli",
        "is_compact_summary": False,
        "is_visible_in_transcript_only": False,
        "user_type": None,
        "slug": None,
    },
    "text": "hello world",
    "blocks": [],
    "interrupted": False,
    "is_agent_injected": False,
    "prompt_id": None,
    "prompt_source": None,
    "queue_priority": None,
    "image_paste_ids": None,
    "source_tool_use_id": None,
    "source_tool_assistant_uuid": None,
    "mcp_meta": None,
    "permission_mode": None,
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


def denial(said: str) -> str:
    return f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will follow."


def tool_use_entry(n: int, tool_use_id: str, name: str, **input: Any) -> dict[str, Any]:
    return assistant_entry(
        n, [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}], stop_reason="tool_use"
    )


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


def write_transcript(path: Path, entries: list[dict[str, Any]], *, mtime: float | None = None) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\n".join(orjson.dumps(entry) for entry in entries) + b"\n")
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


def list_line(path: Path) -> str:
    stat = path.stat()
    return f"{datetime.fromtimestamp(stat.st_mtime):%Y-%m-%d %H:%M} {human_size(stat.st_size):>8} {path}"


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def python_backend(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    monkeypatch.setattr(TranscriptParser, "backend_instance", None)


@pytest.fixture
def unparseable(tmp_path: Path) -> Path:
    bad = tmp_path / "bad.jsonl"
    bad.write_bytes(orjson.dumps(envelope(0, type="user", message={"role": "user", "content": None})) + b"\n")
    return bad


@pytest.fixture
def transcript(tmp_path: Path) -> Path:
    return write_transcript(tmp_path / "t.jsonl", fixture_entries())


@pytest.fixture
def rich(tmp_path: Path) -> Path:
    return write_transcript(tmp_path / "rich.jsonl", rich_entries())


@pytest.fixture
def root(tmp_path: Path) -> tuple[Path, Path, Path]:
    root = tmp_path / "projects"
    old = write_transcript(root / "-Users-x-proj-a" / "old.jsonl", [user_entry(0, "needle one")], mtime=1_000_000.0)
    new = write_transcript(
        root / "-Users-x-proj-b" / "new.jsonl",
        [user_entry(1, "needle two", sessionId="sess-2")],
        mtime=2_000_000.0,
    )
    return root, old, new


@pytest.fixture
def session_root(tmp_path: Path) -> Path:
    root = tmp_path / "projects"
    write_transcript(root / "-Users-x-proj-a" / "sess-1.jsonl", fixture_entries())
    return root


def test_help_lists_all_commands(runner: CliRunner) -> None:
    result = runner.invoke(cli, ["--help"])
    assert result.exit_code == 0
    assert set(cli.commands) == {
        "list",
        "show",
        "grep",
        "stats",
        "slice",
        "digest",
        "corrections",
        "tools",
        "commands",
        "permissions",
        "mcp",
    }


def test_list_newest_first(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, new = root
    result = runner.invoke(cli, ["list", "--root", str(rootdir)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [list_line(new), list_line(old), f"2 transcripts under {rootdir}"]


def test_list_project_filter(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, _ = root
    result = runner.invoke(cli, ["list", "--root", str(rootdir), "--project", "proj-a"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [list_line(old), f"1 transcripts under {rootdir}"]


def test_list_contains_filter(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, _, new = root
    result = runner.invoke(cli, ["list", "--root", str(rootdir), "--contains", "new"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [list_line(new), f"1 transcripts under {rootdir}"]


def test_list_limit_truncates_and_all_restores(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, new = root
    limited = runner.invoke(cli, ["list", "--root", str(rootdir), "--limit", "1"])
    assert limited.output.splitlines() == [list_line(new), f"1 of 2 transcripts under {rootdir}"]
    full = runner.invoke(cli, ["list", "--root", str(rootdir), "--limit", "1", "--all"])
    assert full.output.splitlines() == [list_line(new), list_line(old), f"2 transcripts under {rootdir}"]


def test_list_json_line_shape(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, new = root
    result = runner.invoke(cli, ["list", "--root", str(rootdir), "--json"])
    assert result.exit_code == 0
    assert [orjson.loads(line) for line in result.output.splitlines()] == [
        {"path": str(new), "mtime": 2_000_000.0, "size": new.stat().st_size},
        {"path": str(old), "mtime": 1_000_000.0, "size": old.stat().st_size},
    ]


def test_list_empty_root(runner: CliRunner, tmp_path: Path) -> None:
    empty = tmp_path / "nothing"
    result = runner.invoke(cli, ["list", "--root", str(empty)])
    assert result.exit_code == 0
    assert result.output == f"0 transcripts under {empty}\n"


def test_show_default_renders_every_event(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript)])
    assert result.exit_code == 0
    assert tuple(result.output.splitlines()) == EXPECTED_SHOW


@pytest.mark.parametrize(
    ("args", "expected"),
    [
        pytest.param(["--head", "2"], EXPECTED_SHOW[:2], id="head"),
        pytest.param(["--tail", "2"], EXPECTED_SHOW[-2:], id="tail"),
        pytest.param(["--range", "3:5"], EXPECTED_SHOW[3:5], id="range-mid"),
        pytest.param(["--range", "10:"], EXPECTED_SHOW[10:], id="range-open-end"),
        pytest.param(["--range", ":2"], EXPECTED_SHOW[:2], id="range-open-start"),
    ],
)
def test_show_slicers(runner: CliRunner, transcript: Path, args: list[str], expected: tuple[str, ...]) -> None:
    result = runner.invoke(cli, ["show", str(transcript), *args])
    assert result.exit_code == 0
    assert tuple(result.output.splitlines()) == expected


def test_show_kind_filter_preserves_raw_indexes(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--kind", "user"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [EXPECTED_SHOW[i] for i in (0, 3, 5, 6, 7, 8, 12)]


def test_show_signal_drops_junk_sidechain_and_empty(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--signal"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [EXPECTED_SHOW[i] for i in (0, 1, 2, 4, 6, 12)]


def test_show_thinking_inline(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--range", "2:3", "--thinking"])
    assert result.output == "    2 asst  03:04:07 [claude-opus-4-7] th(12ch) let me think Read(/x)\n"


def test_show_width_truncates(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--range", "0:1", "--width", "8"])
    assert result.output == "    0 user  03:04:05 hello w…\n"


def test_show_json_fidelity(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--json", "--range", "0:1"])
    assert result.exit_code == 0
    assert orjson.loads(result.output) == USER0_DICT


def test_show_caps_at_200_with_notice(runner: CliRunner, tmp_path: Path) -> None:
    path = write_transcript(tmp_path / "big.jsonl", [user_entry(n, f"msg {n}") for n in range(205)])
    result = runner.invoke(cli, ["show", str(path)])
    lines = result.output.splitlines()
    assert result.exit_code == 0
    assert lines[0] == "… 5 earlier events hidden — use --head/--range/--all"
    assert len(lines) == 201
    assert lines[1] == f"    5 user  {BASE_TS + timedelta(seconds=5):%H:%M:%S} msg 5"
    assert lines[-1] == f"  204 user  {BASE_TS + timedelta(seconds=204):%H:%M:%S} msg 204"


def test_show_json_cap_notice_goes_to_stderr(runner: CliRunner, tmp_path: Path) -> None:
    path = write_transcript(tmp_path / "big.jsonl", [user_entry(n, f"msg {n}") for n in range(205)])
    result = runner.invoke(cli, ["show", str(path), "--json"])
    assert result.exit_code == 0
    assert result.stderr == "… 5 earlier events hidden — use --head/--range/--all\n"
    assert [orjson.loads(line)["i"] for line in result.stdout.splitlines()] == list(range(5, 205))


def test_show_two_slicers_usage_error(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["show", str(transcript), "--head", "1", "--tail", "1"])
    assert result.exit_code == 2
    assert "--head, --tail, and --range are mutually exclusive" in result.stderr


@pytest.mark.parametrize("option", ["--head", "--tail"])
def test_show_negative_slicer_usage_error(runner: CliRunner, transcript: Path, option: str) -> None:
    result = runner.invoke(cli, ["show", str(transcript), option, "-1"])
    assert result.exit_code == 2
    assert "is not in the range" in result.stderr


def test_show_tolerates_null_line(runner: CliRunner, tmp_path: Path, python_backend: None) -> None:
    path = tmp_path / "t.jsonl"
    path.write_bytes(orjson.dumps(user_entry(0, "hello world")) + b"\nnull\n")
    result = runner.invoke(cli, ["show", str(path)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [EXPECTED_SHOW[0]]


def test_grep_match_exits_zero(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "hello", str(transcript)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[0], "1 files, 1 matches"]


def test_grep_no_match_exits_one(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "zebra", str(transcript)])
    assert result.exit_code == 1
    assert result.output == "0 files, 0 matches\n"


def test_grep_ignore_case(runner: CliRunner, transcript: Path) -> None:
    sensitive = runner.invoke(cli, ["grep", "HELLO", str(transcript)])
    assert sensitive.exit_code == 1
    insensitive = runner.invoke(cli, ["grep", "HELLO", str(transcript), "-i"])
    assert insensitive.exit_code == 0
    assert insensitive.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[0], "1 files, 1 matches"]


def test_grep_kind_filter(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "o", str(transcript), "--kind", "system"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[9], "1 files, 1 matches"]


def test_grep_tool_hits_use_and_correlated_result(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", ".", str(transcript), "--tool", "Read"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {transcript}",
        EXPECTED_SHOW[2],
        EXPECTED_SHOW[3],
        "1 files, 2 matches",
    ]


def test_grep_tool_pipe_spec_matches_either(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", ".", str(transcript), "--tool", "Read|Bash"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {transcript}",
        EXPECTED_SHOW[2],
        EXPECTED_SHOW[3],
        EXPECTED_SHOW[4],
        EXPECTED_SHOW[5],
        "1 files, 4 matches",
    ]


def test_grep_tool_alias_matches_canonical_name(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", ".", str(transcript), "--tool", "Execute"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {transcript}",
        EXPECTED_SHOW[4],
        EXPECTED_SHOW[5],
        "1 files, 2 matches",
    ]


def test_grep_where_thinking(runner: CliRunner, transcript: Path) -> None:
    thinking = runner.invoke(cli, ["grep", "think", str(transcript), "--where", "thinking"])
    assert thinking.exit_code == 0
    assert thinking.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[2], "1 files, 1 matches"]
    text_only = runner.invoke(cli, ["grep", "think", str(transcript), "--where", "text"])
    assert text_only.exit_code == 1
    assert text_only.output == "0 files, 0 matches\n"


def test_grep_context_windows_and_separator(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "hello|final", str(transcript), "-C", "1"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {transcript}",
        EXPECTED_SHOW[0],
        EXPECTED_SHOW[1],
        "--",
        EXPECTED_SHOW[11],
        EXPECTED_SHOW[12],
        "1 files, 2 matches",
    ]


def test_grep_max_matches(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "hello|final", str(transcript), "--max-matches", "1"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[0], "1 files, 1 matches"]


@pytest.mark.parametrize("args", [["-C", "-1"], ["--max-matches", "-1"]], ids=["context", "max-matches"])
def test_grep_negative_count_usage_error(runner: CliRunner, transcript: Path, args: list[str]) -> None:
    result = runner.invoke(cli, ["grep", "hello", str(transcript), *args])
    assert result.exit_code == 2
    assert "is not in the range" in result.stderr


def test_grep_system_kind_matches_subtype(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "stop_hook", str(transcript), "--kind", "system"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[9], "1 files, 1 matches"]


def test_grep_mode_kind_matches_channel_value(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "mode=normal", str(transcript), "--kind", "mode"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [f"== {transcript}", EXPECTED_SHOW[10], "1 files, 1 matches"]


def test_grep_bad_sibling_keeps_healthy_matches(
    runner: CliRunner, root: tuple[Path, Path, Path], python_backend: None
) -> None:
    rootdir, old, new = root
    bad = rootdir / "-Users-x-proj-c" / "bad.jsonl"
    bad.parent.mkdir(parents=True)
    bad.write_bytes(orjson.dumps(envelope(0, type="user", message={"role": "user", "content": None})) + b"\n")
    result = runner.invoke(cli, ["grep", "needle", "--root", str(rootdir)])
    assert result.exit_code == 0
    assert result.stderr == f"warning: skipped 1 unparseable transcript(s): {bad}\n"
    assert result.stdout.splitlines() == [
        f"== {new}",
        "    0 user  03:04:06 needle two",
        f"== {old}",
        "    0 user  03:04:05 needle one",
        "2 files, 2 matches",
    ]


def test_grep_discovery_multi_file(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, new = root
    result = runner.invoke(cli, ["grep", "needle", "--root", str(rootdir)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {new}",
        "    0 user  03:04:06 needle two",
        f"== {old}",
        "    0 user  03:04:05 needle one",
        "2 files, 2 matches",
    ]


def test_grep_limit_note_and_all_restores(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, old, new = root
    limited = runner.invoke(cli, ["grep", "needle", "--root", str(rootdir), "--limit", "1"])
    assert limited.exit_code == 0
    assert limited.output.splitlines() == [
        f"== {new}",
        "    0 user  03:04:06 needle two",
        "1 files, 1 matches · searched 1 of 2 transcripts — use --all",
    ]
    full = runner.invoke(cli, ["grep", "needle", "--root", str(rootdir), "--limit", "1", "--all"])
    assert full.output.splitlines() == [
        f"== {new}",
        "    0 user  03:04:06 needle two",
        f"== {old}",
        "    0 user  03:04:05 needle one",
        "2 files, 2 matches",
    ]


def test_grep_zero_matches_still_notes_truncation(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, _, _ = root
    result = runner.invoke(cli, ["grep", "needle one", "--root", str(rootdir), "--limit", "1"])
    assert result.exit_code == 1
    assert result.output == "0 files, 0 matches · searched 1 of 2 transcripts — use --all\n"


def test_grep_json_carries_path_and_event(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "hello", str(transcript), "--json"])
    assert result.exit_code == 0
    assert orjson.loads(result.output) == {"path": str(transcript)} | USER0_DICT


def test_grep_json_context_emits_windows(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["grep", "hello", str(transcript), "--json", "-C", "1"])
    assert result.exit_code == 0
    rows = [orjson.loads(line) for line in result.output.splitlines()]
    assert rows[0] == {"path": str(transcript)} | USER0_DICT
    assert [(row["i"], row.get("context")) for row in rows] == [(0, None), (1, True)]


def test_stats_single_file_exact(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["stats", str(transcript)])
    assert result.exit_code == 0
    assert result.output == EXPECTED_STATS + "\n"


def test_stats_json(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["stats", str(transcript), "--json"])
    assert result.exit_code == 0
    assert orjson.loads(result.output) == {
        "files": 1,
        "events": 13,
        "kinds": {"user": 7, "assistant": 3, "system": 1, "mode": 1, "other": 1},
        "models": {"claude-opus-4-7": 3},
        "tools": {"Read": 1, "Bash": 1},
        "text_chars": 126,
        "thinking_chars": 12,
        "tool_io_chars": 47,
        "sessions": 1,
        "first_timestamp": "2026-01-02T03:04:05+00:00",
        "last_timestamp": "2026-01-02T03:04:17+00:00",
        "interrupts": 1,
        "tool_errors": 1,
        "sidechain": 1,
    }


def test_stats_per_file(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["stats", str(transcript), "--per-file"])
    assert result.exit_code == 0
    assert result.output == f"== {transcript}\n{EXPECTED_STATS}\n\n"


def test_stats_warns_on_unparseable_file(
    runner: CliRunner, transcript: Path, unparseable: Path, python_backend: None
) -> None:
    result = runner.invoke(cli, ["stats", str(transcript), str(unparseable)])
    assert result.exit_code == 0
    assert result.stderr == f"warning: skipped 1 unparseable transcript(s): {unparseable}\n"
    assert result.stdout == EXPECTED_STATS + "\n"


def test_stats_discovery_combines_files(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, _, _ = root
    result = runner.invoke(cli, ["stats", "--root", str(rootdir)])
    assert result.exit_code == 0
    assert result.output == "\n".join(
        (
            "files        2",
            "events       2",
            "kinds        user 2",
            "models       -",
            "tools        -",
            "text         20B",
            "thinking     0B",
            "tool io      0B",
            "sessions     2",
            "span         2026-01-02 03:04:05 → 2026-01-02 03:04:06",
            "interrupts   0",
            "tool errors  0",
            "sidechain    0",
            "",
        )
    )


def test_stats_limit_note_and_all_restores(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, _, _ = root
    limited = runner.invoke(cli, ["stats", "--root", str(rootdir), "--limit", "1"])
    assert limited.exit_code == 0
    lines = limited.output.splitlines()
    assert lines[0] == "files        1"
    assert lines[-1] == "searched 1 of 2 transcripts — use --all"
    full = runner.invoke(cli, ["stats", "--root", str(rootdir), "--limit", "1", "--all"])
    assert full.exit_code == 0
    assert full.output.splitlines()[0] == "files        2"
    assert "searched" not in full.output


def test_stats_json_omits_truncation_note(runner: CliRunner, root: tuple[Path, Path, Path]) -> None:
    rootdir, _, _ = root
    result = runner.invoke(cli, ["stats", "--root", str(rootdir), "--limit", "1", "--json"])
    assert result.exit_code == 0
    assert orjson.loads(result.output)["files"] == 1


def test_slice_emits_one_line_per_tool_call(runner: CliRunner, session_root: Path) -> None:
    result = runner.invoke(cli, ["slice", "--session", "sess-1", *WINDOW, "--root", str(session_root)])
    assert result.exit_code == 0
    rows = [orjson.loads(line) for line in result.output.splitlines()]
    assert rows == [READ_SLICE, BASH_SLICE]
    assert all(type(row["ts_ms"]) is int for row in rows)


@pytest.mark.parametrize(
    ("since", "until", "expected"),
    [
        pytest.param("2026-01-02T03:04:09Z", "2026-01-02T03:05:00Z", [BASH_SLICE], id="since_inclusive"),
        pytest.param("2026-01-02T03:04:00Z", "2026-01-02T03:04:09Z", [READ_SLICE], id="until_exclusive"),
        pytest.param("2026-01-02T03:05:00Z", "2026-01-02T03:06:00Z", [], id="empty_window"),
    ],
)
def test_slice_window_filtering(
    runner: CliRunner, session_root: Path, since: str, until: str, expected: list[dict[str, object]]
) -> None:
    result = runner.invoke(
        cli, ["slice", "--session", "sess-1", "--since", since, "--until", until, "--root", str(session_root)]
    )
    assert result.exit_code == 0
    assert [orjson.loads(line) for line in result.output.splitlines()] == expected


def test_slice_missing_transcript_exits_one_with_empty_stdout(runner: CliRunner, session_root: Path) -> None:
    result = runner.invoke(cli, ["slice", "--session", "sess-gone", *WINDOW, "--root", str(session_root)])
    assert result.exit_code == 1
    assert result.stdout == ""


@pytest.mark.parametrize(
    ("since", "until"),
    [
        pytest.param("not-a-time", "2026-01-02T03:05:00Z", id="unparseable_since"),
        pytest.param("2026-01-02T03:04:00Z", "2026-01-02T03:05:00", id="naive_until"),
    ],
)
def test_slice_bad_timestamp_usage_error(runner: CliRunner, session_root: Path, since: str, until: str) -> None:
    result = runner.invoke(
        cli, ["slice", "--session", "sess-1", "--since", since, "--until", until, "--root", str(session_root)]
    )
    assert result.exit_code == 2
    assert "RFC 3339" in result.stderr


def test_slice_unparseable_transcript_exits_two_with_empty_stdout(
    runner: CliRunner, tmp_path: Path, python_backend: None
) -> None:
    root = tmp_path / "projects"
    bad = root / "-Users-x-proj-a" / "sess-9.jsonl"
    bad.parent.mkdir(parents=True)
    bad.write_bytes(orjson.dumps(envelope(0, type="user", message={"role": "user", "content": None})) + b"\n")
    result = runner.invoke(cli, ["slice", "--session", "sess-9", *WINDOW, "--root", str(root)])
    assert result.exit_code == 2
    assert result.stdout == ""


def test_digest_generates_fixture_rows(runner: CliRunner) -> None:
    result = runner.invoke(cli, ["digest"], input=orjson.dumps(DIGEST_CASES).decode())
    assert result.exit_code == 0
    assert orjson.loads(result.output) == [
        case | {"digest": tool_digest(case["tool"], case["input"])} for case in DIGEST_CASES
    ]


def test_digest_check_verifies_and_catches_corruption(runner: CliRunner, tmp_path: Path) -> None:
    fixture = tmp_path / "digest_fixtures.json"
    fixture.write_text(runner.invoke(cli, ["digest"], input=orjson.dumps(DIGEST_CASES).decode()).output)
    ok = runner.invoke(cli, ["digest", "--check", str(fixture)])
    assert ok.exit_code == 0
    assert ok.output == ""

    rows = orjson.loads(fixture.read_bytes())
    rows[1]["digest"] = "0" * 64
    fixture.write_bytes(orjson.dumps(rows))
    corrupted = runner.invoke(cli, ["digest", "--check", str(fixture)])
    assert corrupted.exit_code == 1
    assert corrupted.stderr == (
        f"mismatch: Edit expected {'0' * 64}, computed {tool_digest('Edit', DIGEST_CASES[1]['input'])}\n"
    )


def test_digest_invalid_stdin_usage_error(runner: CliRunner) -> None:
    result = runner.invoke(cli, ["digest"], input="not json")
    assert result.exit_code == 2
    assert "invalid JSON on stdin" in result.stderr


def test_tools_human_lists_one_line_per_call(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["tools", str(transcript)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        "2026-01-02 03:04:07 sess-1 Read",
        "2026-01-02 03:04:09 sess-1 Bash ls [err]",
    ]


def test_tools_human_mcp_and_denied_markers(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["tools", str(rich)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        "2026-01-02 03:04:06 sess-1 Bash rm [denied]",
        "2026-01-02 03:04:08 sess-1 semble/search",
        "2026-01-02 03:04:10 sess-1 semble/find_related",
        "2026-01-02 03:04:12 sess-1 railway/deploy",
    ]


def test_tools_json_row_shape(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["tools", str(transcript), "--json"])
    assert result.exit_code == 0
    rows = [orjson.loads(line) for line in result.output.splitlines()]
    assert rows[0] == {
        "ts": "2026-01-02T03:04:07+00:00",
        "session_id": "sess-1",
        "path": str(transcript),
        "tool_use_id": "toolu_read",
        "tool": "Read",
        "command_prefixes": [],
        "command": None,
        "mcp_server": None,
        "mcp_tool": None,
        "mcp_access": None,
        "file_path": "/x",
        "is_error": False,
        "denied": False,
        "user_said": None,
        "duration_ms": 1000,
    }
    assert rows[1]["tool"] == "Bash"
    assert rows[1]["command_prefixes"] == ["ls"]
    assert rows[1]["is_error"] is True


def test_tools_tool_filter_keeps_only_matches(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["tools", str(rich), "--tool", "search", "--json"])
    assert result.exit_code == 0
    rows = [orjson.loads(line) for line in result.output.splitlines()]
    assert [row["tool"] for row in rows] == ["mcp__semble__search"]


def test_commands_human_counts(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["commands", str(rich)])
    assert result.exit_code == 0
    assert result.output.splitlines() == ["  1  rm"]


def test_commands_json_rows_sorted_desc(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["commands", str(transcript), "--json"])
    assert result.exit_code == 0
    assert [orjson.loads(line) for line in result.output.splitlines()] == [{"prefix": "ls", "count": 1}]


def test_permissions_human_line(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["permissions", str(rich)])
    assert result.exit_code == 0
    assert result.output.splitlines() == ["Bash rm -rf /tmp/x → do not delete that"]


def test_permissions_json_row_shape(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["permissions", str(rich), "--json"])
    assert result.exit_code == 0
    assert [orjson.loads(line) for line in result.output.splitlines()] == [
        {
            "ts": "2026-01-02T03:04:06+00:00",
            "session": "sess-1",
            "path": str(rich),
            "tool": "Bash",
            "command": "rm -rf /tmp/x",
            "file_path": None,
            "user_said": "do not delete that",
        }
    ]


def test_permissions_empty_without_denials(runner: CliRunner, transcript: Path) -> None:
    result = runner.invoke(cli, ["permissions", str(transcript)])
    assert result.exit_code == 0
    assert result.output == ""


def test_permissions_excludes_plan_and_question_rejections(runner: CliRunner, tmp_path: Path) -> None:
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
    result = runner.invoke(cli, ["permissions", str(path), "--json"])
    assert result.exit_code == 0
    assert [orjson.loads(line)["tool"] for line in result.output.splitlines()] == ["Bash"]


def test_mcp_human_table_ordered_by_total(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["mcp", str(rich)])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        "semble   read 2 · write 0 · total 2  search 1 · find_related 1",
        "railway  read 0 · write 1 · total 1  deploy 1",
    ]


def test_mcp_json_rows(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["mcp", str(rich), "--json"])
    assert result.exit_code == 0
    assert [orjson.loads(line) for line in result.output.splitlines()] == [
        {"server": "semble", "read": 2, "write": 0, "total": 2, "tools": {"search": 1, "find_related": 1}},
        {"server": "railway", "read": 0, "write": 1, "total": 1, "tools": {"deploy": 1}},
    ]


def test_grep_with_result_json_adds_results_sibling(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["grep", "rm|query", str(rich), "--json", "--with-result"])
    assert result.exit_code == 0
    by_i = {row["i"]: row for line in result.output.splitlines() if (row := orjson.loads(line))}
    assert by_i[1]["results"] == {"toolu_rm": {"is_error": True, "denied": True, "duration_ms": 1000}}
    assert by_i[3]["results"] == {"toolu_srch": {"is_error": False, "denied": False, "duration_ms": 1000}}


def test_grep_without_with_result_omits_sibling(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["grep", "rm|query", str(rich), "--json"])
    assert result.exit_code == 0
    rows = [orjson.loads(line) for line in result.output.splitlines()]
    assert rows and all("results" not in row for row in rows)


def test_grep_with_result_builds_facts_only_for_transcripts_with_hits(
    runner: CliRunner, rich: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from cc_transcript.facts import tool_facts

    quiet = write_transcript(tmp_path / "quiet.jsonl", [user_entry(0, "nothing to see")])
    joined: list[Any] = []

    def counting(transcripts: Any) -> Any:
        joined.append(transcripts)
        return tool_facts(transcripts)

    monkeypatch.setattr("cc_transcript.cli.tool_facts", counting)
    result = runner.invoke(cli, ["grep", "rm|query", str(rich), str(quiet), "--json", "--with-result"])
    assert result.exit_code == 0
    assert len(joined) == 1


def test_grep_with_result_human_appends_markers(runner: CliRunner, rich: Path) -> None:
    result = runner.invoke(cli, ["grep", "rm|query", str(rich), "--with-result"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        f"== {rich}",
        "    1 asst  03:04:06 [claude-opus-4-7] rm -rf /tmp/x [denied] (1000ms)",
        "    3 asst  03:04:08 [claude-opus-4-7] mcp__semble__search(x) (1000ms)",
        "1 files, 2 matches",
    ]
