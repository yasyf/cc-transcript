from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript.command import (
    PAYLOAD_DEPTH_LIMIT,
    Command,
    CommandLine,
    Redirect,
    Word,
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

    @pytest.mark.parametrize(
        ("raw", "executable"),
        [
            pytest.param("env -u HOME rm /x", "rm", id="env_unset_value_flag"),
            pytest.param("sudo -u root rm /x", "rm", id="sudo_user_value_flag"),
            pytest.param("timeout 5s rm /x", "rm", id="timeout_duration_suffix"),
            pytest.param("sudo -u root -g wheel rm /x", "rm", id="sudo_two_value_flags"),
            pytest.param("env --unset=HOME rm /x", "rm", id="equals_join_consumes_nothing"),
            pytest.param("nice -n 10 rm /x", "rm", id="nice_flag_and_integer"),
            pytest.param("xargs -I{} rm {}", "rm", id="xargs_replace"),
            pytest.param("sudo env -u HOME rm /x", "rm", id="nested_wrapper_chain"),
            pytest.param("sudo -Z rm /x", "rm", id="unknown_flag_flag_only_skip"),
            pytest.param("/usr/bin/sudo rm /x", "rm", id="basename_head"),
        ],
    )
    def test_arity_aware_unwrap_reaches_real_command(self, raw: str, executable: str) -> None:
        assert parse(raw).unwrapped.executable == executable

    @pytest.mark.parametrize(
        ("raw", "executable"),
        [
            pytest.param("timeout rm -rf /", "rm", id="non_digit_slot_is_command"),
            pytest.param("timeout git push", "git", id="bare_command_after_timeout"),
            pytest.param("timeout ٣ git push", "٣", id="arabic_digit_not_skipped"),
        ],
    )
    def test_operand_skip_only_consumes_digit_led_duration(self, raw: str, executable: str) -> None:
        assert parse(raw).unwrapped.executable == executable


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


class TestSplitOptions:
    @pytest.mark.parametrize(
        ("raw", "value_flags", "options", "operands"),
        [
            pytest.param("cmd -a -- -b c", (), ("-a",), ("-b", "c"), id="double_dash_terminates"),
            pytest.param("cmd - -v", (), ("-v",), ("-",), id="lone_dash_is_operand"),
            pytest.param("cmd -o file rest", ("-o",), ("-o", "file"), ("rest",), id="value_flag_consumes_next"),
            pytest.param("cmd -o=file rest", ("-o",), ("-o=file",), ("rest",), id="equals_join_consumes_nothing"),
            pytest.param("cmd", ("-o",), (), (), id="empty_args"),
        ],
    )
    def test_split_options(
        self, raw: str, value_flags: tuple[str, ...], options: tuple[str, ...], operands: tuple[str, ...]
    ) -> None:
        opts, opers = parse(raw).split_options(value_flags)
        assert tuple(word.value for word in opts) == options
        assert tuple(word.value for word in opers) == operands

    def test_split_options_returns_words_with_provenance(self) -> None:
        opts, opers = parse('run --name "x y" pos').split_options(("--name",))
        assert tuple(word.value for word in opts) == ("--name", "x y")
        assert opts[1].raw == '"x y"'
        assert opts[1].span is not None
        assert tuple(word.value for word in opers) == ("pos",)


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
        assert [cmd.executable for cmd in cl.commands] == ["eval", "direnv", "uv"]
        assert cl.parts[1][1] == "&&"
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


class TestCommandSubstitutions:
    @pytest.mark.parametrize(
        ("raw", "executables"),
        [
            pytest.param("x=$(ccx repo overview)", ["ccx"], id="assignment_position"),
            pytest.param("echo $(ccx repo overview)", ["echo", "ccx"], id="word_position"),
            pytest.param("echo `ccx repo overview`", ["echo", "ccx"], id="backticks"),
            pytest.param('echo "$(a)"', ["echo", "a"], id="double_quoted"),
            pytest.param("FOO=$(a) cmd", ["cmd", "a"], id="env_prefix_value"),
            pytest.param("$(which python) --version", ["$(which python)", "which"], id="command_name"),
            pytest.param("diff $(sort a) $(b $(c))", ["diff", "sort", "b", "c"], id="nested_document_order"),
            pytest.param("echo hi > $(target)", ["echo"], id="redirect_target_excluded"),
        ],
    )
    def test_substitutions_join_enumeration(self, raw: str, executables: list[str]) -> None:
        assert [cmd.executable for cmd in CommandLine.parse(raw).commands] == executables

    def test_word_position_mirrors_assignment_position_operators(self) -> None:
        assign = CommandLine.parse("x=$(a) && foo")
        assert [(cmd.executable, op) for cmd, op in assign.parts] == [("a", "&&"), ("foo", None)]
        word = CommandLine.parse("echo $(a) && foo")
        assert [(cmd.executable, op) for cmd, op in word.parts] == [("echo", None), ("a", "&&"), ("foo", None)]
        assert word.occurrences[1].prev_op is None
        assert word.occurrences[1].next_op == "&&"
        assert word.occurrences[2].prev_op == "&&"

    def test_nested_substitution_is_visible_but_spanless(self) -> None:
        line = CommandLine.parse("echo $(ccx repo overview)")
        assert line.commands[1].span is None
        with pytest.raises(ValueError, match="no span"):
            line.splice({1: "ls"})

    def test_assignment_substitution_keeps_span_and_splices(self) -> None:
        line = CommandLine.parse("x=$(ccx repo overview)")
        assert line.commands[0].span == (4, 21)
        assert line.splice({0: "ls"}) == "x=$(ls)"


class TestWords:
    def test_words_parallel_args(self) -> None:
        cmd = parse("echo 'sq x' \"dq\" plain $V")
        assert isinstance(cmd.words, tuple)
        assert len(cmd.words) == len(cmd.args) + 1
        assert [w.raw for w in cmd.words] == ["echo", "'sq x'", '"dq"', "plain", "$V"]
        assert [w.value for w in cmd.words] == ["echo", "sq x", "dq", "plain", None]
        start, end = cmd.words[1].span or (0, 0)
        assert cmd.raw[start:end] == "'sq x'"
        assert cmd.words[1].expandable is False
        assert parse("ls *.py").words[1].expandable is True

    def test_word_constructs_and_compares(self) -> None:
        word = Word("x", value="x", span=(0, 1), expandable=False)
        assert word == Word("x", value="x", span=(0, 1))
        assert word != Word("y", value="y")
        assert repr(word).startswith("Word(")


class TestPayloads:
    def test_shell_payload_enumerates(self) -> None:
        line = CommandLine.parse("bash -c 'rm -rf /tmp/x'")
        assert [cmd.executable for cmd in line.commands] == ["bash", "rm"]
        host, payload = line.occurrences
        assert (host.nesting, payload.nesting) == (0, 1)
        assert host.host is None
        assert payload.host is not None
        assert payload.host.index == 0
        assert host.quote_contexts == ()
        assert payload.quote_contexts == ("'",)
        assert line.primary is not None
        assert line.primary.executable == "bash"

    def test_payload_splice_and_embed_guard(self) -> None:
        line = CommandLine.parse("bash -c 'rm -rf /tmp/x'")
        assert line.splice({1: "trash /tmp/x"}) == "bash -c 'trash /tmp/x'"
        with pytest.raises(ValueError, match="quote layers"):
            line.splice({1: "echo 'hi'"})

    def test_quote_helpers(self) -> None:
        assert CommandLine.quote("safe.txt") == "safe.txt"
        assert CommandLine.quote("a b") == "'a b'"
        assert CommandLine.quote("a'b") == "'a'\\''b'"
        occ = CommandLine.parse("bash -c 'x'").occurrences[1]
        assert occ.embeddable("trash /tmp/x") is True
        assert occ.embeddable("don't") is False
        assert occ.quote_for("a b") == '"a b"'
        top = CommandLine.parse("ls").occurrences[0]
        assert top.embeddable("anything ' goes") is True
        assert top.quote_for("a b") == "'a b'"

    def test_operand_ends_the_option_scan(self) -> None:
        script = CommandLine.parse("bash script.sh -c 'rm x'")
        assert [cmd.executable for cmd in script.commands] == ["bash"]
        terminated = CommandLine.parse("bash -- s.sh -c 'rm x'")
        assert [cmd.executable for cmd in terminated.commands] == ["bash"]
        flagged = CommandLine.parse("bash -euo pipefail -c 'rm x'")
        assert [cmd.executable for cmd in flagged.commands] == ["bash", "rm"]

    def test_depth_limit_exported(self) -> None:
        assert PAYLOAD_DEPTH_LIMIT == 3


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


class TestSplice:
    def test_middle_of_three_preserves_neighbors(self) -> None:
        assert CommandLine.parse("a; b; c").splice({1: "XX"}) == "a; XX; c"

    @pytest.mark.parametrize(
        ("raw", "spliced"),
        [
            pytest.param("a && b", "X && b", id="and"),
            pytest.param("a || b", "X || b", id="or"),
            pytest.param("a | b", "X | b", id="pipe"),
            pytest.param("a & b", "X & b", id="background"),
            pytest.param("a\nb", "X\nb", id="newline"),
        ],
    )
    def test_joining_operator_survives(self, raw: str, spliced: str) -> None:
        assert CommandLine.parse(raw).splice({0: "X"}) == spliced

    def test_redirect_outside_span_survives(self) -> None:
        assert CommandLine.parse("cat a > b; echo x").splice({0: "dog"}) == "dog > b; echo x"

    def test_leading_redirect_outside_span(self) -> None:
        # A leading redirect is a child of the command node; its bytes stay outside the span.
        assert CommandLine.parse(">out echo hi").splice({0: "R"}) == ">out R"

    def test_both_edge_redirects_preserved(self) -> None:
        assert CommandLine.parse("<in echo hi >out").splice({0: "R"}) == "<in R >out"

    def test_interleaved_redirect_has_no_span(self) -> None:
        # 'echo a >out b' — tree-sitter folds the trailing 'b' into the redirect, splitting
        # the command's args, so there is no contiguous span and splice refuses.
        cl = CommandLine.parse("echo a >out b")
        assert cl.occurrences[0].command.span is None
        with pytest.raises(ValueError, match="no span"):
            cl.splice({0: "X"})

    def test_repeated_identical_commands_target_by_index(self) -> None:
        # Defeats an rfind/replace-style implementation: only the index-1 'x' is rewritten.
        assert CommandLine.parse("x; x; x").splice({1: "Y"}) == "x; Y; x"

    def test_heredoc_body_matching_later_command(self) -> None:
        # EDGE_CASES heredoc shape + a real trailing command identical to a body line:
        # the byte span targets the real command (byte 42), never str.find's body hit (byte 21).
        raw = "cat <<'EOF'\nrm -rf /\ngit push --force\nEOF\ngit push --force"
        cl = CommandLine.parse(raw)
        assert [occ.command.executable for occ in cl.occurrences] == ["cat", "git"]
        spliced = cl.splice({1: "true"})
        assert spliced == "cat <<'EOF'\nrm -rf /\ngit push --force\nEOF\ntrue"
        assert "rm -rf /\ngit push --force\n" in spliced

    def test_multi_heredoc_drops_body_and_guards_splice(self) -> None:
        # Degraded multi-heredoc: walk keeps cat + the real trailing command, and the degraded
        # command is span-less so splice can never rewrite heredoc bytes.
        cl = CommandLine.parse("cat <<A <<B\none\nA\ntwo\nB\necho done")
        assert [occ.command.executable for occ in cl.occurrences] == ["cat", "echo"]
        with pytest.raises(ValueError, match="no span"):
            cl.splice({0: "X"})
        assert cl.splice({1: "echo DONE"}) == "cat <<A <<B\none\nA\ntwo\nB\necho DONE"

    def test_comment_tail_preserved(self) -> None:
        assert CommandLine.parse("echo hi # trailing comment").splice({0: "ls"}) == "ls # trailing comment"

    def test_subshell_interior(self) -> None:
        assert CommandLine.parse("(cd src && make)").splice({1: "test"}) == "(cd src && test)"

    def test_multibyte_unicode_before_span(self) -> None:
        # 'é' is two UTF-8 bytes; the later command's span is a byte offset, not a char offset.
        assert CommandLine.parse("echo café; rm x").splice({1: "ls"}) == "echo café; ls"

    def test_span_less_command_raises(self) -> None:
        line = CommandLine(raw="foo", parts=((Command(raw="foo", executable="foo", args=()), None),))
        with pytest.raises(ValueError, match="no span"):
            line.splice({0: "bar"})

    def test_overlapping_spans_raise(self) -> None:
        parts = (
            (Command(raw="ab", executable="ab", args=(), span=(0, 4)), None),
            (Command(raw="cd", executable="cd", args=(), span=(2, 6)), None),
        )
        line = CommandLine(raw="abcdef", parts=parts)
        with pytest.raises(ValueError, match="overlaps"):
            line.splice({0: "X", 1: "Y"})

    def test_rewrite_occurrences_maps_and_splices(self) -> None:
        cl = CommandLine.parse("git push; ls; git pull")
        rewritten = cl.rewrite_occurrences(lambda occ: "BLOCKED" if occ.command.executable == "git" else None)
        assert rewritten == "BLOCKED; ls; BLOCKED"

    def test_rewrite_occurrences_none_when_no_match(self) -> None:
        assert CommandLine.parse("ls -la").rewrite_occurrences(lambda occ: None) is None

    def test_accepts_any_mapping(self) -> None:
        from collections import UserDict
        from types import MappingProxyType

        assert CommandLine.parse("a; b").splice(MappingProxyType({1: "X"})) == "a; X"
        assert CommandLine.parse("a; b").splice(UserDict({1: "X"})) == "a; X"

    def test_negative_index_resolves_like_tuple(self) -> None:
        assert CommandLine.parse("a; b").splice({-1: "X"}) == "a; X"
        assert CommandLine.parse("a; b").splice({-2: "X"}) == "X; b"

    @pytest.mark.parametrize("index", [2, -3])
    def test_out_of_range_index_raises_indexerror(self, index: int) -> None:
        with pytest.raises(IndexError, match="tuple index out of range"):
            CommandLine.parse("a; b").splice({index: "X"})


class TestOccurrences:
    def test_one_occurrence_per_part_in_order(self) -> None:
        cl = CommandLine.parse("cmd1 && cmd2 || cmd3")
        assert [occ.index for occ in cl.occurrences] == [0, 1, 2]
        assert [occ.command.executable for occ in cl.occurrences] == ["cmd1", "cmd2", "cmd3"]

    def test_prev_and_next_op(self) -> None:
        first, mid, last = CommandLine.parse("cmd1 && cmd2 || cmd3").occurrences
        assert (first.prev_op, first.next_op) == (None, "&&")
        assert (mid.prev_op, mid.next_op) == ("&&", "||")
        assert (last.prev_op, last.next_op) == ("||", None)

    @pytest.mark.parametrize(
        ("raw", "piped"),
        [
            pytest.param("foo | bar", [True, True], id="pipe"),
            pytest.param("a |& b", [True, True], id="pipe_ampersand"),
            pytest.param("cat f | grep x | wc -l", [True, True, True], id="pipe_chain"),
            pytest.param("a\nb", [False, False], id="newline_statements"),
            pytest.param("a && b", [False, False], id="and_not_piped"),
            pytest.param("a # x|y\nb", [False, False], id="comment_pipe"),
            pytest.param("cat a > 'x|y'\nb", [False, False], id="quoted_redirect_pipe"),
            pytest.param("cat <<'EOF'\nx|y\nEOF\nb", [False, False], id="heredoc_body_pipe"),
            pytest.param("a\n[[ x || y ]]\nb", [False, False], id="test_command_or"),
            pytest.param("a\n((1|2))\nb", [False, False], id="arithmetic_pipe"),
        ],
    )
    def test_piped(self, raw: str, piped: list[bool]) -> None:
        assert [occ.piped for occ in CommandLine.parse(raw).occurrences] == piped


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
        assert [cmd.executable for cmd in cl.commands] == ["eval", "direnv", "uv", "head"]
        assert cl.primary is not None
        assert cl.primary.executable == "head"

        mtest_cmd = next(cmd for cmd in cl if cmd.program == "mtest")
        assert mtest_cmd.env == (("ENV", "prod"),)
        assert mtest_cmd.has_arg(r"^-k$")
        assert mtest_cmd.redirects == (Redirect(op=">&", target="1", fd=2),)


PIN_DELIM = "|"
PINS_PATH = Path(__file__).resolve().parent.parent / "rust" / "crates" / "py" / "data" / "command_prefix_pins.tsv"


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

    def test_parse_command_line_caches(self) -> None:
        assert parse_command_line("ls -la") is parse_command_line("ls -la")
