from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.command import command_prefixes, load_rust_prefixes
from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.filterspec import session_id_of
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.tools import BashCall

if TYPE_CHECKING:
    from pathlib import Path

REAL_CORPUS_SAMPLE = 25
requires_rust = pytest.mark.skipif(load_rust_prefixes() is None, reason="_parser_rs.command_prefixes is not built")

# Every command string pinned in tests/test_command.py, plus quoting, heredoc,
# subshell, redirect, mixed-operator, background, and multibyte cases. Parity is
# asserted against the Python reference, so no expected values are hardcoded.
FIXTURE_COMMANDS = [
    # --- test_command.py pins ---
    "cat file.py",
    "ENV=val uv run pytest",
    "ENV_VAR=val OTHER=x uv run pytest tests/",
    'jj commit -m "some message"',
    "uv run mtest run tests/",
    "uv run pytest tests/",
    "python -m module arg",
    "python3 -m module arg",
    "jj commit",
    "",
    "# just a comment",
    "jj commit -m x",
    "jj log",
    "uv run mtest run tests/ --last-failed",
    "uv run mtest run tests/ -k test_name",
    "uv run mtest run tests/test_foo.py",
    "jj commit -m msg",
    "ENV=val uv run mtest",
    "cat",
    "echo hello >> out.txt",
    "cmd 2>&1",
    "echo hello >> out.txt 2>&1",
    "sudo git push",
    "env -i FOO=bar make test",
    "timeout 30 git push",
    "nice -n 10 cargo build",
    "sudo env FOO=1 timeout 5 ls -la",
    "sudo",
    "ls -la",
    "VAR=1 sudo git push > log.txt",
    "git commit -m x",
    "git --version",
    "docker compose up -d",
    "sudo docker compose up",
    "sudo git push -f",
    "cmd1; cmd2 && cmd3",
    "cmd1; cmd2",
    "cmd1 || cmd2",
    "cd /dir && ./setup.sh",
    'eval "$(direnv export bash)" && uv run mtest run tests/',
    "cat file.py | grep pattern",
    "cmd1 && cmd2 && cmd3",
    "a && b",
    'eval "$(direnv)" && uv run',
    "cmd",
    "cat <<EOF\ngit push --force\nEOF",
    'eval "$(direnv export bash)"',
    "sudo git push -f && echo hi",
    "> out.txt",
    "cd /x && git push -f",
    "cd /x && sudo git push",
    "git push",
    "# comment",
    "git push origin main",
    "ls -la && git commit -m x",
    "cat f | grep x",
    "echo hi > f.txt",
    "git push origin",
    'echo "unterminated',
    "uv run mtest run tests/test_foo.py::TestClass::test_method",
    'eval "$(direnv export bash)" && ENV=prod uv run mtest run tests/test_foo.py -k test_name 2>&1 | head -50',
    "VAR=1 sudo docker compose up -d",
    "git add . && git commit -m 'x; y'",
    "cat a && grep b",
    "for f in *.py; do python $f; done",
    "while true; do sleep 1; done",
    "if grep -q x f; then echo y; else echo z; fi",
    # --- quoting ---
    'git commit -m "multi word message"',
    "echo 'single quoted'",
    'grep -r "pattern with spaces" .',
    '"echo" hello',
    "git commit -m 'has; semicolon'",
    # --- heredoc / herestring ---
    "cat <<'EOF'\nrm -rf /\nEOF",
    'sort <<< "some string"',
    # --- subshell / command substitution ---
    "(cd /tmp && ls)",
    "echo $(git rev-parse HEAD)",
    "x=$(pwd) && echo $x",
    "{ git status; git diff; }",
    # --- redirect ---
    "git log > /dev/null 2>&1",
    "sort < input.txt",
    "cmd 2> err.log",
    # --- mixed operators / background ---
    "a && b || c ; d | e",
    "git add . && git commit -m x || echo failed",
    "false || sudo git push & echo done",
    "sleep 1 &",
    "long-task & echo started",
    "set -o pipefail && cat f | grep x | head -5",
    # --- more wrappers / loops / conditionals ---
    "command -v git",
    "env FOO=bar BAZ=qux python script.py",
    "time cargo build --release",
    "doas systemctl restart nginx",
    "xargs -n1 git status",
    "until false; do echo x; done",
    "case $x in a) echo a;; esac",
    # --- multibyte ---
    "echo 漢字",
    'git commit -m "日本語メッセージ"',
    'echo "emoji 🤖 test"',
    "grep café file.txt",
    "timeout ٣ git push",
    # --- empty argv tokens / one-layer dequoting ---
    "sudo ''",
    "git '' status",
    "echo \"'hello'\"",
    "git \"'commit'\"",
    "echo '",
    'echo "a',
    # --- process substitution / arithmetic / backticks ---
    "diff <(sort a.txt) <(sort b.txt)",
    "cat <(git log) | head -3",
    "echo $((1 + 2))",
    "x=$((COUNT + 1)) make build",
    "echo `git rev-parse HEAD`",
    # --- function definitions / multiline / newline-separated ---
    "foo() { git status; }; foo",
    'git commit -m "line1\nline2"',
    "git add .\ngit commit -m x",
]


def rust_prefixes(commands: list[str]) -> list[list[str]]:
    from cc_transcript import _parser_rs

    return _parser_rs.command_prefixes(commands)


def python_prefixes(commands: list[str]) -> list[list[str]]:
    return [list(command_prefixes(command)) for command in commands]


def assert_batch_parity(commands: list[str]) -> None:
    actual = rust_prefixes(commands)
    expected = python_prefixes(commands)
    assert len(actual) == len(expected)
    for command, rust, python in zip(commands, actual, expected, strict=True):
        assert rust == python, f"prefix parity diverged for {command!r}\n  python={python!r}\n  rust={rust!r}"


def real_corpus() -> list[Path]:
    if not CLAUDE_PROJECTS_DIR.exists():
        return []
    # Largest transcripts first: Bash activity concentrates in substantive sessions,
    # so this samples real command variety rather than empty agent/journal files.
    return sorted(CLAUDE_PROJECTS_DIR.rglob("*.jsonl"), key=lambda p: p.stat().st_size, reverse=True)[
        :REAL_CORPUS_SAMPLE
    ]


def bash_commands(path: Path) -> list[str]:
    events = parse_events_from_bytes(path.read_bytes())
    session_id = session_id_of(events)
    if session_id is None:
        return []
    activity = SessionActivity.from_events(session_id, events)
    return [
        use.call.command
        for turn in activity.turns
        for use in turn.tool_uses
        if isinstance(use.call, BashCall)
    ]


@requires_rust
def test_fixture_battery_parity() -> None:
    assert_batch_parity(FIXTURE_COMMANDS)


@requires_rust
def test_real_corpus_command_parity() -> None:
    commands = [command for path in real_corpus() for command in bash_commands(path)]
    if not commands:
        pytest.skip(f"no Bash commands under {CLAUDE_PROJECTS_DIR}")
    assert_batch_parity(commands)
