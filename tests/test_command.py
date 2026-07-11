from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript.command import (
    Command,
    CommandLine,
    Redirect,
    bulk_command_prefixes,
    command_prefixes,
    parse_command_line,
)


def parse(raw: str) -> Command:
    cmd = Command.parse(raw)
    assert cmd is not None
    return cmd


class TestCommand:
    def test_simple_command(self) -> None:
        cmd = parse("cat file.py")
        assert cmd.executable == "cat"
        assert cmd.args == ("file.py",)

    def test_env_vars(self) -> None:
        cmd = parse("ENV=val uv run pytest")
        assert cmd.executable == "uv"
        assert cmd.env_dict == {"ENV": "val"}

    def test_multiple_env_vars(self) -> None:
        cmd = parse("ENV_VAR=val OTHER=x uv run pytest tests/")
        assert cmd.executable == "uv"
        assert cmd.env == (("ENV_VAR", "val"), ("OTHER", "x"))
        assert cmd.env_dict == {"ENV_VAR": "val", "OTHER": "x"}

    def test_vcs_command(self) -> None:
        cmd = parse('jj commit -m "some message"')
        assert cmd.executable == "jj"
        assert cmd.args[0] == "commit"

    @pytest.mark.parametrize(
        ("raw", "program"),
        [
            pytest.param("uv run mtest run tests/", "mtest", id="uv_run"),
            pytest.param("uv run pytest tests/", "pytest", id="uv_run_pytest"),
            pytest.param("python -m module arg", "module", id="python_m"),
            pytest.param("python3 -m module arg", "module", id="python3_m"),
            pytest.param("jj commit", "jj", id="plain_executable"),
        ],
    )
    def test_program(self, raw: str, program: str) -> None:
        assert parse(raw).program == program

    def test_argv_includes_executable_and_args(self) -> None:
        assert parse("cat file.py").argv == ("cat", "file.py")

    @pytest.mark.parametrize(
        ("raw",),
        [
            pytest.param("", id="blank"),
            pytest.param("# just a comment", id="comment_only"),
        ],
    )
    def test_parse_nothing_is_none(self, raw: str) -> None:
        assert Command.parse(raw) is None

    @pytest.mark.parametrize(
        ("raw", "pattern", "expected"),
        [
            pytest.param("jj commit -m x", r"jj\s+(commit|split)", True, id="matches_regex"),
            pytest.param("jj log", r"jj\s+(commit|split)", False, id="no_match"),
        ],
    )
    def test_matches(self, raw: str, pattern: str, expected: bool) -> None:
        assert parse(raw).matches(pattern) is expected

    @pytest.mark.parametrize(
        ("raw", "pattern", "expected"),
        [
            pytest.param("uv run mtest run tests/ --last-failed", r"--last-failed", True, id="has_arg"),
            pytest.param("uv run mtest run tests/ -k test_name", r"^-k$", True, id="has_arg_k"),
            pytest.param("uv run mtest run tests/", r"--last-failed", False, id="no_arg"),
        ],
    )
    def test_has_arg(self, raw: str, pattern: str, expected: bool) -> None:
        assert parse(raw).has_arg(pattern) is expected

    @pytest.mark.parametrize(
        ("raw", "needle", "expected"),
        [
            pytest.param("uv run mtest run tests/test_foo.py", ".py", True, id="contains"),
            pytest.param("jj commit -m msg", ".py", False, id="not_contains"),
        ],
    )
    def test_contains(self, raw: str, needle: str, expected: bool) -> None:
        assert (needle in parse(raw)) is expected

    @pytest.mark.parametrize(
        ("raw", "rendered"),
        [
            pytest.param("ENV=val uv run mtest", "uv run mtest", id="strips_env"),
            pytest.param("jj commit", "jj commit", id="simple"),
        ],
    )
    def test_str(self, raw: str, rendered: str) -> None:
        assert str(parse(raw)) == rendered

    def test_truthiness(self) -> None:
        assert bool(parse("cat")) is True

    def test_append_redirect(self) -> None:
        assert parse("echo hello >> out.txt").redirects == (Redirect(op=">>", target="out.txt", fd=None),)

    def test_fd_redirect(self) -> None:
        assert parse("cmd 2>&1").redirects == (Redirect(op=">&", target="1", fd=2),)

    def test_multiple_redirects(self) -> None:
        assert parse("echo hello >> out.txt 2>&1") == Command(
            raw="echo hello",
            executable="echo",
            args=("hello",),
            redirects=(Redirect(op=">>", target="out.txt", fd=None), Redirect(op=">&", target="1", fd=2)),
        )


class TestUnwrapped:
    @pytest.mark.parametrize(
        ("raw", "argv"),
        [
            pytest.param("sudo git push", ("git", "push"), id="sudo"),
            pytest.param("env -i FOO=bar make test", ("make", "test"), id="env_flags_and_assignments"),
            pytest.param("timeout 30 git push", ("git", "push"), id="timeout_bare_integer"),
            pytest.param("nice -n 10 cargo build", ("cargo", "build"), id="nice_flag_plus_integer"),
            pytest.param("sudo env FOO=1 timeout 5 ls -la", ("ls", "-la"), id="nested_wrappers"),
            pytest.param("sudo", (), id="wrapper_alone_is_empty"),
        ],
    )
    def test_unwrapped_argv(self, raw: str, argv: tuple[str, ...]) -> None:
        assert parse(raw).unwrapped.argv == argv

    def test_no_unwrapping_returns_self(self) -> None:
        cmd = parse("ls -la")
        assert cmd.unwrapped is cmd

    def test_unwrapped_preserves_redirects_and_env(self) -> None:
        cmd = parse("VAR=1 sudo git push > log.txt").unwrapped
        assert cmd.argv == ("git", "push")
        assert cmd.env == (("VAR", "1"),)
        assert cmd.redirects == (Redirect(op=">", target="log.txt", fd=None),)


class TestPrefix:
    @pytest.mark.parametrize(
        ("raw", "prefix"),
        [
            pytest.param("git commit -m x", "git commit", id="multilevel_subcommand"),
            pytest.param("git --version", "git", id="multilevel_only_flags"),
            pytest.param("docker compose up -d", "docker compose", id="docker_compose"),
            pytest.param("ls -la", "ls", id="plain_executable"),
            pytest.param("sudo docker compose up", "docker compose", id="unwraps_wrapper"),
            pytest.param("nice -n 10 cargo build", "cargo build", id="unwraps_flag_and_integer"),
            pytest.param("sudo", None, id="wrapper_alone_is_none"),
        ],
    )
    def test_prefix(self, raw: str, prefix: str | None) -> None:
        assert parse(raw).prefix == prefix


class TestCommandRuns:
    @pytest.mark.parametrize(
        ("argv", "expected"),
        [
            pytest.param(("git",), True, id="one_token_prefix"),
            pytest.param(("git", "push"), True, id="two_token_prefix"),
            pytest.param(("git", "push", "-f"), True, id="full_argv"),
            pytest.param(("git", "commit"), False, id="wrong_subcommand"),
            pytest.param((), False, id="empty_argv_is_false"),
        ],
    )
    def test_runs_unwraps_wrappers(self, argv: tuple[str, ...], expected: bool) -> None:
        assert parse("sudo git push -f").runs(*argv) is expected


class TestCommandLine:
    @pytest.mark.parametrize(
        ("raw", "length", "primary_executable"),
        [
            pytest.param("jj commit", 1, "jj", id="simple"),
            pytest.param("cmd1; cmd2 && cmd3", 3, "cmd3", id="mixed_operators"),
        ],
    )
    def test_length_and_primary(self, raw: str, length: int, primary_executable: str) -> None:
        cl = CommandLine.parse(raw)
        assert len(cl) == length
        assert cl.primary is not None
        assert cl.primary.executable == primary_executable

    @pytest.mark.parametrize(
        ("raw", "op"),
        [
            pytest.param("cmd1; cmd2", ";", id="semicolon_chain"),
            pytest.param("cmd1 || cmd2", "||", id="or_chain"),
        ],
    )
    def test_two_part_chain(self, raw: str, op: str) -> None:
        cl = CommandLine.parse(raw)
        assert len(cl) == 2
        assert cl.parts[0][1] == op

    def test_primary_executable(self) -> None:
        assert parse("cd /dir && ./setup.sh").executable == "./setup.sh"

    def test_and_chain(self) -> None:
        cl = CommandLine.parse('eval "$(direnv export bash)" && uv run mtest run tests/')
        assert len(cl) == 2
        assert cl.parts[0][1] == "&&"
        assert cl.primary is not None
        assert cl.primary.executable == "uv"
        assert cl.primary.program == "mtest"

    def test_pipe_chain(self) -> None:
        cl = CommandLine.parse("cat file.py | grep pattern")
        assert len(cl) == 2
        assert cl.parts[0][1] == "|"
        assert cl.commands[0].executable == "cat"
        assert cl.commands[1].executable == "grep"

    def test_head(self) -> None:
        cl = CommandLine.parse("cd /dir && ./setup.sh")
        assert cl.head is not None
        assert cl.head.executable == "cd"

    def test_iter(self) -> None:
        assert [cmd.executable for cmd in CommandLine.parse("cmd1 && cmd2 && cmd3")] == ["cmd1", "cmd2", "cmd3"]

    def test_str_returns_raw(self) -> None:
        assert str(CommandLine.parse("a && b")) == "a && b"

    @pytest.mark.parametrize(
        ("raw", "needle", "expected"),
        [
            pytest.param('eval "$(direnv)" && uv run', "direnv", True, id="contains_raw"),
            pytest.param("jj commit", "direnv", False, id="not_contains"),
        ],
    )
    def test_contains(self, raw: str, needle: str, expected: bool) -> None:
        assert (needle in CommandLine.parse(raw)) is expected

    def test_truthy(self) -> None:
        assert bool(CommandLine.parse("cmd")) is True

    @pytest.mark.parametrize(
        ("raw",),
        [
            pytest.param("", id="blank"),
            pytest.param("# just a comment", id="comment_only"),
        ],
    )
    def test_nothing_parses_to_empty_parts(self, raw: str) -> None:
        cl = CommandLine.parse(raw)
        assert cl.parts == ()
        assert cl.primary is None
        assert cl.head is None
        assert len(cl) == 0
        assert bool(cl) is False
        assert cl.prefixes == ()

    def test_pipe_heredoc_not_treated_as_command(self) -> None:
        cl = CommandLine.parse("cat <<EOF\ngit push --force\nEOF")
        assert cl.primary is not None
        assert cl.primary.executable == "cat"
        assert not any(cmd.executable == "git" for cmd in cl.commands)

    def test_subshell_parsed(self) -> None:
        assert len(CommandLine.parse('eval "$(direnv export bash)"')) >= 1

    def test_prefixes(self) -> None:
        assert CommandLine.parse("sudo git push -f && echo hi").prefixes == ("git push", "echo")

    def test_prefixes_drops_empty_executable(self) -> None:
        cl = CommandLine.parse("> out.txt")
        assert len(cl) == 1
        assert cl.prefixes == ()


class TestCommandLineQuery:
    @pytest.mark.parametrize(
        ("raw", "argv", "expected"),
        [
            pytest.param("cd /x && git push -f", ("git", "push"), True, id="primary_prefix"),
            pytest.param("cd /x && sudo git push", ("git", "push"), True, id="unwraps_primary"),
            pytest.param("git commit -m x", ("git", "push"), False, id="wrong_subcommand"),
            pytest.param("git push", (), False, id="empty_argv_is_false"),
            pytest.param("", ("git",), False, id="none_primary_is_false"),
            pytest.param("# comment", ("git",), False, id="comment_only_is_false"),
        ],
    )
    def test_runs(self, raw: str, argv: tuple[str, ...], expected: bool) -> None:
        assert CommandLine.parse(raw).q.runs(*argv) is expected

    @pytest.mark.parametrize(
        ("raw", "name", "expected"),
        [
            pytest.param("git push origin main", "push", True, id="subcommand_present"),
            pytest.param("ls -la && git commit -m x", "commit", True, id="any_command_in_line"),
            pytest.param("ls -la", "push", False, id="absent"),
        ],
    )
    def test_has_subcommand(self, raw: str, name: str, expected: bool) -> None:
        assert CommandLine.parse(raw).q.has_subcommand(name) is expected

    def test_any_command(self) -> None:
        cl = CommandLine.parse("cat file.py | grep pattern")
        assert cl.q.any_command(lambda cmd: cmd.executable == "grep") is True
        assert cl.q.any_command(lambda cmd: cmd.executable == "sed") is False

    @pytest.mark.parametrize(
        ("raw", "expected"),
        [
            pytest.param("cat f | grep x", True, id="pipe"),
            pytest.param("echo hi > f.txt", True, id="file_redirect"),
            pytest.param("ls -la", False, id="plain"),
        ],
    )
    def test_uses_redirect(self, raw: str, expected: bool) -> None:
        assert CommandLine.parse(raw).q.uses_redirect() is expected

    @pytest.mark.parametrize(
        ("raw", "token", "expected"),
        [
            pytest.param("git push origin", "git", True, id="matches_executable"),
            pytest.param("git push origin", "origin", True, id="matches_arg"),
            pytest.param("git push origin", "orig", False, id="no_substring_match"),
        ],
    )
    def test_contains_token(self, raw: str, token: str, expected: bool) -> None:
        assert CommandLine.parse(raw).q.contains_token(token) is expected


class TestDequote:
    @pytest.mark.parametrize(
        ("text", "expected"),
        [
            pytest.param("\"'hello'\"", "'hello'", id="one_layer_only"),
            pytest.param("'hello'", "hello", id="matching_single_quotes"),
            pytest.param('"hello"', "hello", id="matching_double_quotes"),
            pytest.param("'", "'", id="lone_quote_untouched"),
            pytest.param('"a', '"a', id="unmatched_open_quote_untouched"),
            pytest.param("hello", "hello", id="unquoted_untouched"),
            pytest.param("", "", id="empty"),
        ],
    )
    def test_dequote(self, text: str, expected: str) -> None:
        assert CommandLine.dequote(text) == expected


class TestEdgeCases:
    def test_malformed_quote(self) -> None:
        cmd = parse('echo "unterminated')
        assert cmd.executable == "echo"

    def test_nested_quotes_keep_inner_layer(self) -> None:
        assert parse("echo \"'hello'\"").args == ("'hello'",)

    def test_colons_preserved(self) -> None:
        cmd = parse("uv run mtest run tests/test_foo.py::TestClass::test_method")
        assert "tests/test_foo.py::TestClass::test_method" in cmd.args

    def test_complex_direnv(self) -> None:
        cl = CommandLine.parse(
            'eval "$(direnv export bash)" && ENV=prod uv run mtest run tests/test_foo.py -k test_name 2>&1 | head -50'
        )
        assert len(cl) == 3
        assert cl.primary is not None
        assert cl.primary.executable == "head"

        mtest_cmd = next(cmd for cmd in cl if cmd.program == "mtest")
        assert mtest_cmd.env == (("ENV", "prod"),)
        assert mtest_cmd.has_arg(r"^-k$")
        assert mtest_cmd.redirects == (Redirect(op=">&", target="1", fd=2),)


PIN_DELIM = "|"
PINS_PATH = Path(__file__).resolve().parent.parent / "rust" / "data" / "command_prefix_pins.tsv"


def decode_pin(field: str) -> str:
    chars = iter(field)
    out: list[str] = []
    for ch in chars:
        if ch != "\\":
            out.append(ch)
            continue
        nxt = next(chars)
        out.append("\n" if nxt == "n" else nxt)
    return "".join(out)


def load_prefix_pins() -> tuple[list[tuple[str, tuple[str, ...]]], list[str]]:
    rows = [
        line.split("\t")
        for line in PINS_PATH.read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
    cases = [(decode_pin(cmd), tuple(exp.split(PIN_DELIM)) if exp else ()) for _id, cmd, exp in rows]
    return cases, [row[0] for row in rows]


PREFIX_PIN_CASES, PREFIX_PIN_IDS = load_prefix_pins()


class TestCommandPrefixes:
    @pytest.mark.parametrize(("command", "expected"), PREFIX_PIN_CASES, ids=PREFIX_PIN_IDS)
    def test_command_prefixes(self, command: str, expected: tuple[str, ...]) -> None:
        assert command_prefixes(command) == expected

    def test_bulk_command_prefixes(self) -> None:
        assert bulk_command_prefixes(["ls -la", "sudo git push -f && echo hi"]) == [("ls",), ("git push", "echo")]

    def test_parse_command_line_caches(self) -> None:
        assert parse_command_line("ls -la") is parse_command_line("ls -la")
