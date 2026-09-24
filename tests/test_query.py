from __future__ import annotations

import json
import os
import pathlib
import re
import threading
from concurrent.futures import ThreadPoolExecutor
from datetime import timedelta
from typing import TYPE_CHECKING, Any

import pytest

from cc_transcript import query
from cc_transcript.activity import SessionActivity, ToolUse
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import ToolUseId
from cc_transcript.query import (
    DEEP_LIFTS,
    SIDECHAIN_INPUTS,
    UNREADABLE,
    FileRef,
    PredicateInputs,
    Session,
    SubagentIndex,
    SubagentSession,
    ToolCallQuery,
)
from cc_transcript.tools import file_paths_of, register_mcp_tool, unregister_mcp_tool
from tests import testkit
from tests.support import BASE, SESSION, assistant, user

if TYPE_CHECKING:
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent
    from cc_transcript.query import DeepLift

PLAN_FILE = "/Users/x/.claude/plans/p.md"


def tool(id: str, name: str, **input: Any) -> dict[str, Any]:
    return testkit.tool_use(id, name, input)


def bash(id: str, command: str) -> dict[str, Any]:
    return tool(id, "Bash", command=command)


def edit(id: str, path: str, old: str = "a", new: str = "b") -> dict[str, Any]:
    return tool(id, "Edit", file_path=path, old_string=old, new_string=new)


def write(id: str, path: str, content: str = "x") -> dict[str, Any]:
    return tool(id, "Write", file_path=path, content=content)


def read(id: str, path: str) -> dict[str, Any]:
    return tool(id, "Read", file_path=path)


def result(id: str, content: str = "ok", *, is_error: bool = False) -> dict[str, Any]:
    return testkit.tool_result(id, content, is_error=is_error)


def session(*events: TranscriptEvent) -> Session:
    return Session.from_activity(SessionActivity.from_events(SESSION, events))


def plan_session() -> Session:
    return session(
        user("u0", "draft a plan"),
        assistant("a0", "", blocks=(write("t1", PLAN_FILE, "# Plan"),), secs=1),
        assistant("a1", "", blocks=(tool("t2", "ExitPlanMode", plan="# Plan"),), secs=2),
        user("u1", "tweak it", secs=3),
        assistant("a2", "", blocks=(write("t3", PLAN_FILE, "# Plan v2"),), secs=4),
    )


@pytest.mark.parametrize(
    ("path", "globs", "expected"),
    [
        pytest.param("/repo/src/app.py", ("*.py",), True, id="basename_glob"),
        pytest.param("/repo/src/app.py", ("/repo/src/app.py",), True, id="exact_path"),
        pytest.param("/repo/src/app.py", ("*.md", "*.toml"), False, id="no_match"),
        pytest.param("/repo/docs/guide.md", ("**/docs/*.md",), True, id="full_path_glob"),
    ],
)
def test_fileref_matches(path: str, globs: tuple[str, ...], expected: bool) -> None:
    assert FileRef(path).matches(*globs) is expected


@pytest.mark.parametrize(
    ("path", "prefixes", "expected"),
    [
        pytest.param("/repo/src/app.py", ("src/",), True, id="anchored_segment"),
        pytest.param("src/app.py", ("src/",), True, id="leading_prefix"),
        pytest.param("/repo/source/app.py", ("src/",), False, id="no_partial_segment"),
    ],
)
def test_fileref_under(path: str, prefixes: tuple[str, ...], expected: bool) -> None:
    assert FileRef(path).under(*prefixes) is expected


@pytest.mark.parametrize(
    ("path", "expected"),
    [
        pytest.param("/repo/tests/test_app.py", True, id="test_file"),
        pytest.param("/repo/tests/unit/conftest.py", True, id="conftest"),
        pytest.param("/repo/tests/fixtures/data.py", True, id="under_tests"),
        pytest.param("/repo/src/app.py", False, id="source_file"),
    ],
)
def test_fileref_is_test(path: str, expected: bool) -> None:
    assert FileRef(path).is_test is expected


@pytest.mark.parametrize(
    ("path", "expected"),
    [
        pytest.param("/repo/src/app.py", ".py", id="py"),
        pytest.param("README.md", ".md", id="md_basename"),
        pytest.param("/repo/data/archive.tar.gz", ".gz", id="last_only"),
        pytest.param("/repo/Makefile", "", id="no_extension"),
    ],
)
def test_fileref_suffix(path: str, expected: str) -> None:
    assert FileRef(path).suffix == expected


def test_fileref_str_and_fspath() -> None:
    ref = FileRef("/repo/src/app.py")
    assert str(ref) == "/repo/src/app.py"
    assert os.fspath(ref) == "/repo/src/app.py"


def test_every_slice_returns_a_session() -> None:
    sess = plan_session()
    assert all(
        isinstance(view, Session)
        for view in (
            sess.after(tool="Write"),
            sess.before(tool="Write"),
            sess.prior(),
            sess.recent(2),
            sess.current_turn,
        )
    )


def test_plan_rewrite_idiom_after_write_sees_exit_plan_mode() -> None:
    sess = plan_session()
    fp = PLAN_FILE
    assert sess.prior().after(tool="Write", file=str(fp)).has_tool("ExitPlanMode")
    assert not sess.after(tool="Write", file=str(fp)).has_tool("ExitPlanMode")


def test_after_without_match_is_empty() -> None:
    sess = plan_session()
    sliced = sess.after(tool="Grep")
    assert len(sliced) == 0
    assert not sliced.has_tool("Write", subagents=False)


def test_after_file_filters_by_substring() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(write("t1", "/a.py"),), secs=1),
        assistant("a1", "", blocks=(write("t2", "/b.py"),), secs=2),
        assistant("a2", "", blocks=(tool("t3", "ExitPlanMode", plan="p"),), secs=3),
    )
    assert sess.after(tool="Write", file="/a.py").has_tool("Write", subagents=False)
    assert not sess.after(tool="Write", file="/b.py").has_tool("Write", subagents=False)


def test_before_slices_out_match_and_tail() -> None:
    sess = plan_session()
    sliced = sess.before(tool="ExitPlanMode")
    assert sliced.has_tool("Write", subagents=False)
    assert not sliced.has_tool("ExitPlanMode", subagents=False)
    assert sliced.before(tool="Grep") == sliced


def test_prior_drops_the_last_conversational_event() -> None:
    sess = plan_session()
    assert not sess.prior().after(tool="ExitPlanMode").tool_calls.named("Edit|Write").any()
    assert sess.after(tool="ExitPlanMode").tool_calls.named("Edit|Write").any()


def test_recent_keeps_the_last_n_events() -> None:
    sess = plan_session()
    assert len(sess.recent(2)) == 2
    assert sess.recent(2).first_prompt == "tweak it"
    assert len(sess.recent(99)) == len(sess)


def hook_attachment(uuid: str, secs: int) -> TranscriptEvent:
    return testkit.parse_event(
        {"type": "attachment", "attachment": {"type": "hook_success", "content": "ok"}}
        | testkit.meta_fields(uuid, session_id=SESSION, timestamp=BASE, secs=secs)
    )


def test_recent_messages_counts_only_messages() -> None:
    queued_send = testkit.parse_event(
        {"type": "attachment", "attachment": {"type": "queued_command", "prompt": "send it", "commandMode": "prompt"}}
        | testkit.meta_fields("q0", session_id=SESSION, timestamp=BASE, secs=2)
    )
    sess = session(
        user("u0", "draft it"),
        hook_attachment("h0", 1),
        queued_send,
        *(hook_attachment(f"h{i}", 2 + i) for i in range(1, 9)),
        assistant("a0", "sent", secs=12),
        hook_attachment("h9", 13),
    )
    assert len(sess.recent(3).turns[0].events) == 3
    assert [type(event).__name__ for event in sess.recent_messages(2).events] == [
        "AttachmentEvent",
        *["AttachmentEvent"] * 8,
        "AssistantEvent",
        "AttachmentEvent",
    ]
    assert sess.recent_messages(2).events[0].detail.prompt == "send it"
    assert len(sess.recent_messages(99)) == len(sess)


def test_trimmed_boundary_turn_drops_prompt_and_earlier_tool_uses() -> None:
    sliced = plan_session().prior().after(tool="Write")
    assert [use.call.name for use in sliced.tool_calls] == ["ExitPlanMode"]
    assert sliced.turns[0].index == 0
    assert sliced.turns[0].prompt == ""
    assert sliced.first_prompt == "tweak it"


def test_slicing_drops_emptied_turns_entirely() -> None:
    sliced = plan_session().after(tool="ExitPlanMode")
    assert [turn.index for turn in sliced.turns] == [1]
    assert sliced.first_prompt == "tweak it"
    assert [use.call.name for use in sliced.tool_calls] == ["Write"]


def test_current_turn_and_user_text() -> None:
    sess = plan_session()
    assert sess.user_text == "tweak it"
    turn_view = sess.current_turn
    assert turn_view.user_text == "tweak it"
    assert [use.call.name for use in turn_view.tool_calls] == ["Write"]
    assert session().user_text == ""


def test_compact_summary_last_user_folds_into_current_turn() -> None:
    sess = session(
        user("u0", "real ask"),
        assistant("a0", "working", secs=1),
        user("u1", "compact recap", is_compact_summary=True, secs=2),
    )
    assert len(sess.turns) == 1
    assert sess.user_text == "real ask"
    assert sess.current_turn.user_text == "real ask"


def test_first_prompt_skips_the_preamble_turn() -> None:
    sess = session(user("u0", "injected", is_meta=True), user("u1", "real ask", secs=1))
    assert sess.first_prompt == "real ask"
    assert session().first_prompt is None


def test_user_said_matches_prompts_case_insensitively() -> None:
    sess = plan_session()
    assert sess.user_said("TWEAK")
    assert not sess.user_said("deploy")
    assert not sess.user_said()


def test_named_files_returns_filerefs_with_path_strings() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(edit("t1", "/a.py"), write("t2", "/b.py"), bash("t3", "ls")), secs=1),
    )
    files = sess.tool_calls.named("Edit|Write").files()
    assert files == (FileRef("/a.py"), FileRef("/b.py"))
    assert [str(f) for f in files] == ["/a.py", "/b.py"]
    assert [str(f) for f in reversed(files)] == ["/b.py", "/a.py"]


def test_execute_alias_matches_named_bash_and_commands() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(tool("t1", "Execute", command="uv run pytest"),), secs=1),
    )
    assert sess.tool_calls.named("Bash").count() == 1
    assert sess.commands() == ("uv run pytest",)
    assert sess.has_command("uv", "run", "pytest", subagents=False)
    assert not sess.has_command("cargo", subagents=False)
    assert sess.has_tool("Bash", subagents=False)


@pytest.mark.parametrize(
    ("command", "expected"),
    [
        pytest.param("sudo git push -f", True, id="unwraps_sudo"),
        pytest.param("cd x && git push", True, id="matches_any_segment"),
        pytest.param('echo "git push"', False, id="ignores_quoted_argument"),
        pytest.param("git pull", False, id="different_subcommand"),
    ],
)
def test_has_command_matches_argv_prefix(command: str, expected: bool) -> None:
    sess = session(user("u0", "go"), assistant("a0", "", blocks=(bash("t1", command),), secs=1))
    assert sess.has_command("git", "push", subagents=False) is expected


def test_has_command_finds_compound_body() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(bash("t1", "if true; then rm -rf /; fi"),), secs=1),
    )
    assert sess.has_command("rm", subagents=False)


def test_command_lines_parses_each_bash_command() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(bash("t1", "git add . && pytest"), bash("t2", "ls -la")), secs=1),
    )
    assert sess.commands() == ("git add . && pytest", "ls -la")
    assert [line.prefixes for line in sess.command_lines()] == [("git add", "pytest"), ("ls",)]


def test_mcp_suffix_matches_named_and_has_tool() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(tool("t1", "mcp__conductor__AskUserQuestion", question="?"),), secs=1),
    )
    assert sess.has_tool("AskUserQuestion", subagents=False)
    assert sess.tool_calls.named("AskUserQuestion").count() == 1
    assert not sess.has_tool("Question", subagents=False)


def override_session(*tail: TranscriptEvent) -> Session:
    return session(
        user("u0", "go"),
        assistant("a0", "", blocks=(edit("t1", "/a.py"),), secs=1),
        assistant("a1", "OVERRIDE-XYZ accepted", secs=2),
        *tail,
    )


def test_has_override_true_when_token_stands() -> None:
    assert override_session().has_override("OVERRIDE-XYZ")
    assert not override_session().has_override("OTHER-TOKEN")


def test_has_override_invalidated_by_later_edit() -> None:
    sess = override_session(assistant("a2", "", blocks=(edit("t2", "/b.py"),), secs=3))
    assert not sess.has_override("OVERRIDE-XYZ")


def test_has_override_invalidation_honors_aliases() -> None:
    sess = override_session(assistant("a2", "", blocks=(tool("t2", "Create", file_path="/b.py", content="x"),), secs=3))
    assert not sess.has_override("OVERRIDE-XYZ")


def test_has_override_custom_invalidators() -> None:
    sess = override_session(assistant("a2", "", blocks=(edit("t2", "/b.py"),), secs=3))
    assert sess.has_override("OVERRIDE-XYZ", invalidated_by=("Bash",))
    bash_after = override_session(assistant("a2", "", blocks=(bash("t2", "rm -rf build"),), secs=3))
    assert not bash_after.has_override("OVERRIDE-XYZ", invalidated_by=("Bash",))


def test_has_override_counts_last_occurrence() -> None:
    sess = override_session(
        assistant("a2", "", blocks=(edit("t2", "/b.py"),), secs=3),
        assistant("a3", "OVERRIDE-XYZ again", secs=4),
    )
    assert sess.has_override("OVERRIDE-XYZ")


def test_count_failures_counts_error_results() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(bash("t1", "ls"), bash("t2", "false"), bash("t3", "true")), secs=1),
        user("u1", "", blocks=(result("t1"), result("t2", "boom", is_error=True)), secs=2),
    )
    assert sess.count_failures() == 1
    assert session().count_failures() == 0


def test_assistant_text_caps_count_and_chars_per_message() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "first answer", secs=1),
        assistant("a1", "   ", secs=2),
        assistant("a2", "second answer", secs=3),
        assistant("a3", "third answer", secs=4),
    )
    assert sess.assistant_text(n=2, max_per_msg=6) == "second\n---\nthird "
    assert sess.assistant_text() == "first answer\n---\nsecond answer\n---\nthird answer"


def query_fixture() -> Session:
    return session(
        user("u0", "go"),
        assistant(
            "a0",
            "",
            blocks=(
                edit("t1", "/repo/src/app.py"),
                edit("t2", "/repo/tests/test_app.py"),
                read("t3", "/repo/README.md"),
                bash("t4", "uv run pytest"),
            ),
            secs=1,
        ),
        user("u1", "again", secs=2),
        assistant("a1", "", blocks=(edit("t5", "/repo/src/app.py"),), secs=3),
        user("u2", "", blocks=(result("t5", "fail", is_error=True),), secs=4),
    )


def test_touching_and_under_filter_by_file() -> None:
    calls = query_fixture().tool_calls
    assert calls.touching("*.py").count() == 2
    assert calls.under("tests/").count() == 1
    assert calls.named("Edit").under("src/").count() == 1


def test_failed_and_with_errors_widen_the_default_view() -> None:
    calls = query_fixture().tool_calls
    assert calls.named("Edit").count() == 2
    assert calls.named("Edit").with_errors.count() == 3
    assert calls.failed().count() == 1
    assert (failed := calls.failed().first()) is not None and failed.ref.tool_use_id == ToolUseId("t5")


def test_in_turns_filters_by_turn_index() -> None:
    calls = query_fixture().tool_calls.with_errors
    assert calls.in_turns(0).count() == 4
    assert calls.in_turns(1).count() == 1
    assert calls.in_turns(0, 1).count() == 5


def test_where_and_where_input_rules() -> None:
    calls = query_fixture().tool_calls
    assert calls.where(lambda use: use.call.name == "Read").count() == 1
    assert calls.where_input(file_path="/repo/README.md").count() == 1
    assert calls.where_input(file_path=re.compile(r"src/")).count() == 1
    assert calls.where_input(command=lambda v: "pytest" in str(v)).count() == 1
    assert calls.where_input(missing_key="x").count() == 0


def test_where_input_task_update_idiom() -> None:
    sess = session(
        user("u0", "go"),
        assistant(
            "a0",
            "",
            blocks=(
                tool("t1", "TaskUpdate", taskId="1", status="completed"),
                tool("t2", "TaskUpdate", taskId="2", status="in_progress"),
            ),
            secs=1,
        ),
    )
    resolved = sess.tool_calls.named("TaskUpdate").where_input(
        status=lambda v: v in ("completed", "deleted"), taskId=lambda _: True
    )
    assert resolved.count() == 1
    assert (first := resolved.first()) is not None and first.call.raw["taskId"] == "1"


def test_terminals_and_dunders() -> None:
    calls = query_fixture().tool_calls.named("Edit")
    assert calls.any()
    assert len(calls) == 2
    assert bool(calls)
    assert [type(use) for use in calls] == [ToolUse, ToolUse]
    assert (first := calls.first()) is not None and first.ref.tool_use_id == ToolUseId("t1")
    assert (last := calls.last()) is not None and last.ref.tool_use_id == ToolUseId("t2")
    empty = calls.named("Grep")
    assert isinstance(empty, ToolCallQuery)
    assert (empty.count(), empty.any(), empty.first(), empty.last()) == (0, False, None, None)


def test_files_touched_and_edited_files() -> None:
    sess = query_fixture()
    assert [str(f) for f in sess.files_touched] == [
        "/repo/src/app.py",
        "/repo/tests/test_app.py",
        "/repo/README.md",
    ]
    assert [str(f) for f in sess.edited_files] == ["/repo/src/app.py", "/repo/tests/test_app.py"]
    assert [str(f) for f in sess.tool_calls.with_errors.named("Edit").files()] == [
        "/repo/src/app.py",
        "/repo/tests/test_app.py",
        "/repo/src/app.py",
    ]


def test_has_edit_to_and_has_read() -> None:
    sess = query_fixture()
    assert sess.has_edit_to("/repo/src/app.py", subagents=False)
    assert sess.has_edit_to("*.py", subagents=False)
    assert not sess.has_edit_to("/repo/README.md", subagents=False)
    assert sess.has_read("README.md", subagents=False)
    assert not sess.has_read("CHANGELOG.md", subagents=False)


def test_has_skill_matches_exact_names() -> None:
    sess = session(
        user("u0", "go"),
        assistant("a0", "", blocks=(tool("t1", "Skill", skill="codex:codex"),), secs=1),
    )
    assert sess.has_skill("codex", "codex:codex", subagents=False)
    assert not sess.has_skill("verify", subagents=False)


def transcript_line(uuid: str, secs: int, **overrides: Any) -> str:
    return json.dumps(
        {
            "uuid": uuid,
            "parentUuid": None,
            "sessionId": str(SESSION),
            "timestamp": (BASE + timedelta(seconds=secs)).isoformat(),
            "cwd": "/repo",
            "gitBranch": "main",
            "version": "1.2.3",
            "isSidechain": False,
        }
        | overrides
    )


def assistant_line(uuid: str, secs: int, blocks: list[dict[str, Any]], **overrides: Any) -> str:
    return transcript_line(
        uuid,
        secs,
        type="assistant",
        message={"role": "assistant", "model": "claude-opus-4-7", "content": blocks},
        **overrides,
    )


def user_line(uuid: str, secs: int, content: str | list[dict[str, Any]], **overrides: Any) -> str:
    return transcript_line(uuid, secs, type="user", message={"role": "user", "content": content}, **overrides)


def tool_block(id: str, name: str, **input: Any) -> dict[str, Any]:
    return {"type": "tool_use", "id": id, "name": name, "input": input}


def result_block(id: str, content: str = "ok", *, is_error: bool = False) -> dict[str, Any]:
    return {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}


def write_main_transcript(root: Path) -> Path:
    path = root / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    lines = [
        user_line("u0", 0, "run the tests"),
        assistant_line("a0", 1, [tool_block("t9", "Task", prompt="run tests", subagent_type="test-runner")]),
        user_line("u1", 2, [result_block("t9", "done")]),
        assistant_line("a1", 3, [tool_block("t10", "Task", prompt="scout", subagent_type="explorer")]),
        user_line("u2", 4, [result_block("t10", "crashed", is_error=True)]),
    ]
    path.write_text("\n".join(lines) + "\n")
    return path


def write_subagent_transcripts(main: Path) -> None:
    directory = main.parent / main.stem / "subagents"
    directory.mkdir(parents=True)
    (directory / "agent-t9.jsonl").write_text(
        "\n".join(
            [
                user_line("s0", 1, "run tests", isSidechain=True),
                assistant_line("s1", 2, [tool_block("b1", "Bash", command="uv run pytest")], isSidechain=True),
                user_line("s2", 3, [result_block("b1", "1 failed", is_error=True)], isSidechain=True),
                assistant_line(
                    "s3", 4, [tool_block("b2", "Bash", command="uv run pytest -k failing")], isSidechain=True
                ),
                user_line("s4", 5, [result_block("b2", "1 passed")], isSidechain=True),
            ]
        )
        + "\n"
    )
    (directory / "agent-t10.jsonl").write_text(
        "\n".join(
            [
                user_line("s3", 3, "scout", isSidechain=True),
                assistant_line("s4", 4, [tool_block("g1", "Grep", pattern="TODO")], isSidechain=True),
            ]
        )
        + "\n"
    )
    (directory / "._agent-t9.jsonl").write_bytes(b"\x00\x05\x16\x07 not a transcript")


def test_subagent_recursion_gated_by_flag(tmp_path: Path) -> None:
    main = write_main_transcript(tmp_path)
    write_subagent_transcripts(main)
    sess = Session.from_path(main)
    assert not sess.has_tool("Grep", subagents=False)
    assert sess.has_tool("Grep")
    assert sess.has_command("uv", "run", "pytest")
    assert not sess.has_command("uv", "run", "pytest", subagents=False)
    assert not sess.has_tool("Grep", subagents=False)


def test_recursion_is_inert_without_a_path() -> None:
    sess = session(user("u0", "go"))
    assert not sess.has_tool("Grep")
    assert sess.subagents == SubagentIndex(())


def test_subagent_index_with_type_and_failed(tmp_path: Path) -> None:
    main = write_main_transcript(tmp_path)
    write_subagent_transcripts(main)
    subagents = Session.from_path(main).subagents
    assert len(subagents) == 2
    assert all(isinstance(sub, SubagentSession) for sub in subagents)
    (runner,) = subagents.with_type("test-runner")
    assert runner.id == ToolUseId("t9")
    assert runner.failed
    assert runner.tool_calls.with_errors.named("Bash").count() == 2
    assert runner.tool_calls.named("Bash").count() == 1
    (explorer,) = subagents.with_type("explorer|other")
    assert explorer.failed
    assert explorer.session.has_tool("Grep", subagents=False)
    assert subagents.with_type("missing") == ()


def test_with_type_matches_agent_types_alias_free(tmp_path: Path) -> None:
    path = tmp_path / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    path.write_text(
        "\n".join(
            [
                user_line("u0", 0, "go"),
                assistant_line("a0", 1, [tool_block("t1", "Task", prompt="p", subagent_type="Agent")]),
                user_line("u1", 2, [result_block("t1", "done")]),
            ]
        )
        + "\n"
    )
    directory = path.parent / path.stem / "subagents"
    directory.mkdir(parents=True)
    (directory / "agent-t1.jsonl").write_text(user_line("s0", 1, "go", isSidechain=True) + "\n")
    subagents = Session.from_path(path).subagents
    assert subagents.with_type("Task") == ()
    assert [sub.type for sub in subagents.with_type("Agent")] == ["Agent"]


def test_subagent_failed_false_when_clean(tmp_path: Path) -> None:
    path = tmp_path / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    path.write_text(
        "\n".join(
            [
                user_line("u0", 0, "scout"),
                assistant_line("a0", 1, [tool_block("t10", "Task", prompt="scout", subagent_type="explorer")]),
                user_line("u1", 2, [result_block("t10", "done")]),
            ]
        )
        + "\n"
    )
    directory = path.parent / path.stem / "subagents"
    directory.mkdir(parents=True)
    (directory / "agent-t10.jsonl").write_text(
        assistant_line("s0", 1, [tool_block("g1", "Grep", pattern="TODO")], isSidechain=True) + "\n"
    )
    (clean,) = Session.from_path(path).subagents
    assert not clean.failed


def test_from_path_lifts_turns_and_sets_path(tmp_path: Path) -> None:
    main = write_main_transcript(tmp_path)
    sess = Session.from_path(main)
    assert sess.path == main
    assert sess.first_prompt == "run the tests"
    assert sess.tool_calls.with_errors.named("Task").count() == 2


def test_from_id_discovers_the_transcript(tmp_path: Path) -> None:
    main = write_main_transcript(tmp_path)
    sess = Session.from_id(SESSION, root=tmp_path)
    assert sess.path == main
    assert sess.first_prompt == "run the tests"


def test_from_id_raises_expired_when_transcript_gone(tmp_path: Path) -> None:
    with pytest.raises(TranscriptExpiredError) as excinfo:
        Session.from_id(SESSION, root=tmp_path)
    assert excinfo.value.session_id == SESSION


def write_nested_subagent_transcripts(root: Path) -> Path:
    """Fabricate a two-level sidechain tree ``sess/subagents/agent-a/subagents/agent-b.jsonl``.

    ``agent-b`` (depth 2) carries tools that appear nowhere else, so a deep
    predicate that reaches it is unambiguous.
    """
    main = root / "proj" / f"{SESSION}.jsonl"
    main.parent.mkdir(parents=True)
    main.write_text(
        "\n".join(
            [
                user_line("u0", 0, "run it"),
                assistant_line("a0", 1, [tool_block("a", "Task", prompt="delegate", subagent_type="worker")]),
                user_line("u1", 2, [result_block("a", "done")]),
            ]
        )
        + "\n"
    )
    a_dir = main.parent / main.stem / "subagents"
    a_dir.mkdir(parents=True)
    (a_dir / "agent-a.jsonl").write_text(
        "\n".join(
            [
                user_line("s0", 1, "worker", isSidechain=True),
                assistant_line("s1", 2, [tool_block("c1", "Bash", command="cargo build")], isSidechain=True),
                assistant_line(
                    "s2", 3, [tool_block("b", "Task", prompt="nest", subagent_type="deep")], isSidechain=True
                ),
                user_line("s3", 4, [result_block("b", "done")], isSidechain=True),
            ]
        )
        + "\n"
    )
    b_dir = a_dir / "agent-a" / "subagents"
    b_dir.mkdir(parents=True)
    (b_dir / "agent-b.jsonl").write_text(
        "\n".join(
            [
                user_line("d0", 1, "deep", isSidechain=True),
                assistant_line("d1", 2, [tool_block("g1", "Grep", pattern="DEEP_TODO")], isSidechain=True),
                assistant_line(
                    "d2",
                    3,
                    [tool_block("e1", "Edit", file_path="/deep/only.py", old_string="x", new_string="y")],
                    isSidechain=True,
                ),
                assistant_line("d3", 4, [tool_block("r1", "Read", file_path="/deep/nested.py")], isSidechain=True),
                assistant_line("d4", 5, [tool_block("k1", "Skill", skill="deepskill")], isSidechain=True),
                assistant_line("d5", 6, [tool_block("z1", "Bash", command="deeptool run")], isSidechain=True),
            ]
        )
        + "\n"
    )
    return main


def test_nested_sidechain_has_tool_finds_grandchild_tool(tmp_path: Path) -> None:
    """PIN: has_* already reaches a depth-2 sidechain before the walk() relocation.

    Committed against the unmodified query surface to lock the behavior the
    relocation must preserve; ``Grep`` lives only in ``agent-b`` (depth 2).
    """
    main = write_nested_subagent_transcripts(tmp_path)
    sess = Session.from_path(main)
    assert sess.has_tool("Grep")
    assert not sess.has_tool("Grep", subagents=False)
    assert sess.has_command("deeptool", "run")
    assert not sess.has_command("deeptool", "run", subagents=False)


def test_walk_yields_descendants_depth_first_then_unions(tmp_path: Path) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    walked = list(sess.walk())
    assert [d.path.name for d in walked] == ["agent-a.jsonl", "agent-b.jsonl"]
    assert [d.depth for d in walked] == [1, 2]
    assert [d.spawned_by for d in walked] == [ToolUseId("a"), ToolUseId("b")]
    assert all(d.provider == "claude" for d in walked)


def test_walk_lifts_each_sidechain_once_until_it_grows(tmp_path: Path) -> None:
    main = write_nested_subagent_transcripts(tmp_path)
    sess = Session.from_path(main)
    DEEP_LIFTS.clear()

    lifted = [deep.session for deep in sess.walk()]
    assert all(new is held for new, held in zip((deep.session for deep in sess.walk()), lifted, strict=True))
    assert not sess.has_command("deeptool", "later")

    child = main.parent / main.stem / "subagents" / "agent-a.jsonl"
    with child.open("a") as handle:
        handle.write(
            assistant_line("s4", 5, [tool_block("c2", "Bash", command="deeptool later")], isSidechain=True) + "\n"
        )
    assert sess.has_command("deeptool", "later")


def write_flat_subagent_tree(root: Path, lead: str, count: int) -> Path:
    main = root / lead / f"{SESSION}.jsonl"
    main.parent.mkdir(parents=True)
    main.write_text(user_line("u0", 0, "fan out") + "\n")
    subagents = main.parent / main.stem / "subagents"
    subagents.mkdir(parents=True)
    for index in range(count):
        (subagents / f"agent-{index}.jsonl").write_text(
            "\n".join(
                [
                    user_line(f"s{index}", 1, "work", isSidechain=True),
                    assistant_line(
                        f"b{index}", 2, [tool_block(f"c{index}", "Bash", command=f"step {index}")], isSidechain=True
                    ),
                ]
            )
            + "\n"
        )
    return main


def sidechain_bytes(main: Path) -> int:
    return sum(path.stat().st_size for path in (main.parent / main.stem / "subagents").glob("agent-*.jsonl"))


def count_lifts(monkeypatch: pytest.MonkeyPatch) -> list[Path]:
    lifts: list[Path] = []
    lift = query.lift_deep_session

    def counted(path: Path, depth: int, spawned_by: ToolUseId | None) -> tuple[DeepLift, bool]:
        lifts.append(path)
        return lift(path, depth, spawned_by)

    monkeypatch.setattr(query, "lift_deep_session", counted)
    return lifts


def test_deep_predicates_lift_each_sidechain_once_after_another_lead_filled_the_budget(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    hog = write_flat_subagent_tree(tmp_path, "hog", 8)
    lead = write_flat_subagent_tree(tmp_path, "lead", 8)
    monkeypatch.setattr("cc_transcript.query.DEEP_LIFT_BUDGET", sidechain_bytes(lead))
    DEEP_LIFTS.clear()
    assert not Session.from_path(hog).has_tool("Grep")

    lifts = count_lifts(monkeypatch)
    for _ in range(2):
        sess = Session.from_path(lead)
        assert not sess.has_command("git", "push")
        assert not sess.has_tool("Grep")
        assert not sess.has_edit_to("*.py")
        assert not sess.has_skill("review")

    assert len(lifts) == 8


def test_a_grown_sidechain_releases_its_stale_bytes(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 2)
    DEEP_LIFTS.clear()
    list(Session.from_path(main).walk())
    assert DEEP_LIFTS.size == sidechain_bytes(main)

    with (main.parent / main.stem / "subagents" / "agent-0.jsonl").open("a") as handle:
        handle.write(assistant_line("b9", 3, [tool_block("c9", "Bash", command="step later")], isSidechain=True) + "\n")

    assert any(deep.session.has_command("step", "later") for deep in Session.from_path(main).walk())
    assert DEEP_LIFTS.size == sidechain_bytes(main)
    assert len(DEEP_LIFTS) == 2


def test_deep_predicates_hold_no_lifts_and_reanswer_without_lifting(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 4)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    lifts = count_lifts(monkeypatch)
    for _ in range(3):
        sess = Session.from_path(main)
        assert sess.has_command("step", "2")
        assert not sess.has_command("git", "push")
        assert not sess.has_tool("Grep")
        assert sess.has_tool("Bash")
        assert not sess.has_edit_to("*.py")
        assert not sess.has_read("README")
        assert not sess.has_skill("review")

    assert len(lifts) == 4
    assert len(DEEP_LIFTS) == 0
    assert len(SIDECHAIN_INPUTS) == 4


def test_a_grown_sidechain_is_the_only_one_relifted_for_a_predicate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 3)
    SIDECHAIN_INPUTS.clear()
    assert not Session.from_path(main).has_command("step", "later")

    grown = main.parent / main.stem / "subagents" / "agent-1.jsonl"
    with grown.open("a") as handle:
        handle.write(assistant_line("b9", 3, [tool_block("c9", "Bash", command="step later")], isSidechain=True) + "\n")

    lifts = count_lifts(monkeypatch)
    assert Session.from_path(main).has_command("step", "later")
    assert lifts == [grown]


def test_deep_inputs_mirror_the_walk_without_holding_it(tmp_path: Path) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()

    inputs = list(sess.deep_inputs())
    assert len(DEEP_LIFTS) == 0
    walked = [sess, *(deep.session for deep in sess.walk())]
    assert [i.calls for i in inputs] == [
        tuple((use.call.name, tuple(file_paths_of(use.call))) for use in s.tool_calls) for s in walked
    ]
    assert [i.files("Read|Grep") for i in inputs] == [s.tool_calls.named("Read|Grep").files() for s in walked]


def test_a_sidechain_added_after_a_walk_is_reached_by_the_next_one(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 2)
    assert not Session.from_path(main).has_command("step", "late")

    (main.parent / main.stem / "subagents" / "agent-late.jsonl").write_text(
        assistant_line("b7", 2, [tool_block("c7", "Bash", command="step late")], isSidechain=True) + "\n"
    )
    assert Session.from_path(main).has_command("step", "late")
    assert [deep.path.name for deep in Session.from_path(main).walk()][-1] == "agent-late.jsonl"


def test_a_retargeted_symlinked_sidechain_is_resolved_afresh(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    elsewhere = tmp_path / "elsewhere.jsonl"
    elsewhere.write_text(
        assistant_line("b6", 2, [tool_block("c6", "Bash", command="git push")], isSidechain=True) + "\n"
    )
    link = main.parent / main.stem / "subagents" / "agent-link.jsonl"
    target = tmp_path / "current.jsonl"
    target.symlink_to(main)
    link.symlink_to(target)
    assert not Session.from_path(main).has_command("git", "push")

    target.unlink()
    target.symlink_to(elsewhere)
    assert Session.from_path(main).has_command("git", "push")


def test_an_unreadable_subagents_directory_is_listed_again_once_readable(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    directory = main.parent / main.stem / "subagents"
    directory.chmod(0o100)
    try:
        assert not Session.from_path(main).has_command("step", "0")
    finally:
        directory.chmod(0o700)
    assert Session.from_path(main).has_command("step", "0")


def test_a_tool_registered_after_an_answer_changes_deep_has_tool(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    with (main.parent / main.stem / "subagents" / "agent-0.jsonl").open("a") as handle:
        handle.write(assistant_line("b5", 3, [tool_block("c5", "mcp__review__lookalike")], isSidechain=True) + "\n")
    assert not Session.from_path(main).has_tool("Grep")

    register_mcp_tool("lookalike", "Grep")
    try:
        assert Session.from_path(main).has_tool("Grep")
    finally:
        unregister_mcp_tool("lookalike")


def test_memoized_answers_stay_bounded_under_varying_arguments(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    for index in range(PredicateInputs.MAX_ANSWERS * 3):
        Session.from_path(main).has_edit_to(f"src/file_{index}.py")
    assert all(len(inputs.answers) <= PredicateInputs.MAX_ANSWERS for inputs in Session.from_path(main).deep_inputs())


def test_an_errored_sidechain_call_stays_invisible_to_deep_predicates(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    with (main.parent / main.stem / "subagents" / "agent-0.jsonl").open("a") as handle:
        handle.write(
            "\n".join(
                [
                    assistant_line("b8", 3, [tool_block("c8", "Bash", command="git push")], isSidechain=True),
                    user_line("r8", 4, [result_block("c8", "rejected", is_error=True)], isSidechain=True),
                ]
            )
            + "\n"
        )
    sess = Session.from_path(main)
    assert not sess.has_command("git", "push")
    assert any(
        deep.session.tool_calls.with_errors.named("Bash").where_input(command="git push") for deep in sess.walk()
    )


def test_the_least_recently_walked_lift_is_evicted_past_the_budget(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    first, second, third = (write_flat_subagent_tree(tmp_path, lead, 1) for lead in ("first", "second", "third"))
    monkeypatch.setattr("cc_transcript.query.DEEP_LIFT_BUDGET", sidechain_bytes(first) + sidechain_bytes(second))
    DEEP_LIFTS.clear()
    for main in (first, second, first, third):
        list(Session.from_path(main).walk())

    lifts = count_lifts(monkeypatch)
    for main in (first, third, second):
        list(Session.from_path(main).walk())

    assert [path.parent.parent.parent.name for path in lifts] == ["second"]


def test_a_sidechain_larger_than_the_budget_is_never_held(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("cc_transcript.query.DEEP_LIFT_BUDGET", 1)
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    DEEP_LIFTS.clear()

    assert [deep.path.name for deep in sess.walk()] == ["agent-a.jsonl", "agent-b.jsonl"]
    assert len(DEEP_LIFTS) == 0
    assert DEEP_LIFTS.size == 0


def write_branching_subagent_transcripts(root: Path) -> Path:
    """Fabricate a *branching* sidechain tree that distinguishes DFS from BFS.

    Two depth-1 siblings hang off the main session; the path-first sibling
    ``agent-p`` owns a depth-2 child ``agent-c`` while ``agent-q`` has none.
    DFS visits ``[agent-p, agent-c, agent-q]`` (a sibling's whole subtree
    before the next sibling); BFS would visit ``[agent-p, agent-q, agent-c]``.
    On a flat-sibling or single-chain fixture the two orders coincide, so this
    is the only topology that pins the documented DFS contract.
    """
    main = root / "proj" / f"{SESSION}.jsonl"
    main.parent.mkdir(parents=True)
    main.write_text(
        "\n".join(
            [
                user_line("u0", 0, "branch it"),
                assistant_line("a0", 1, [tool_block("p", "Task", prompt="first", subagent_type="worker")]),
                user_line("u1", 2, [result_block("p", "done")]),
                assistant_line("a1", 3, [tool_block("q", "Task", prompt="second", subagent_type="worker")]),
                user_line("u2", 4, [result_block("q", "done")]),
            ]
        )
        + "\n"
    )
    subs = main.parent / main.stem / "subagents"
    subs.mkdir(parents=True)
    (subs / "agent-p.jsonl").write_text(
        "\n".join(
            [
                user_line("s0", 1, "first", isSidechain=True),
                assistant_line(
                    "s1", 2, [tool_block("c", "Task", prompt="nest", subagent_type="deep")], isSidechain=True
                ),
                user_line("s2", 3, [result_block("c", "done")], isSidechain=True),
            ]
        )
        + "\n"
    )
    (subs / "agent-q.jsonl").write_text(
        "\n".join(
            [
                user_line("t0", 1, "second", isSidechain=True),
                assistant_line("t1", 2, [tool_block("g1", "Grep", pattern="Q_TODO")], isSidechain=True),
            ]
        )
        + "\n"
    )
    c_dir = subs / "agent-p" / "subagents"
    c_dir.mkdir(parents=True)
    (c_dir / "agent-c.jsonl").write_text(
        "\n".join(
            [
                user_line("d0", 1, "deep", isSidechain=True),
                assistant_line("d1", 2, [tool_block("x1", "Bash", command="deep run")], isSidechain=True),
            ]
        )
        + "\n"
    )
    return main


def test_walk_order_is_depth_first_not_breadth_first(tmp_path: Path) -> None:
    """PIN: walk() descends each sibling's subtree before the next sibling (DFS).

    A BFS rewrite would yield ``[agent-p, agent-q, agent-c]``; the DFS contract
    the docs, changelog, and positional ``first()``/``last()`` promise requires
    the grandchild ``agent-c`` land between its parent ``agent-p`` and the next
    sibling ``agent-q``.
    """
    walked = list(Session.from_path(write_branching_subagent_transcripts(tmp_path)).walk())
    assert [d.path.name for d in walked] == ["agent-p.jsonl", "agent-c.jsonl", "agent-q.jsonl"]
    assert [d.depth for d in walked] == [1, 2, 1]
    assert [d.spawned_by for d in walked] == [ToolUseId("p"), ToolUseId("c"), ToolUseId("q")]


def test_deep_unions_root_and_every_descendant(tmp_path: Path) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    deep = sess.deep
    assert deep.tool_calls.named("Grep").any()
    assert deep.tool_calls.named("Task").count() == 2
    assert {str(f) for f in deep.tool_calls.named("Edit|Write").files()} == {"/deep/only.py"}
    assert [d.path.name for d in deep] == ["agent-a.jsonl", "agent-b.jsonl"]
    assert deep.events == sess.events + tuple(e for d in deep.sessions for e in d.session.events)


def test_deep_tool_calls_preserve_errored_call_parity(tmp_path: Path) -> None:
    """PIN: the deep union carries every call, errored ones included.

    The union must fold each session's ``all_items`` (not its error-filtered
    ``items``), so ``deep.tool_calls`` matches ``Session.tool_calls``'s
    error-inclusive contract at every depth. Baking error-filtering into the
    pool would silently zero out ``with_errors``/``failed()`` and any
    error-sensitive count over descendants. The fixture spreads failures across
    depths: the root's ``t10`` Task crashed and ``agent-t9``'s ``b1`` Bash
    errored, while the default view keeps only the three that succeeded.
    """
    main = write_main_transcript(tmp_path)
    write_subagent_transcripts(main)
    deep = Session.from_path(main).deep
    assert deep.tool_calls.with_errors.count() == 5
    assert deep.tool_calls.failed().count() == 2
    assert deep.tool_calls.count() == 3
    assert {use.ref.tool_use_id for use in deep.tool_calls.failed()} == {ToolUseId("t10"), ToolUseId("b1")}


def test_bare_tool_calls_stay_window_scoped(tmp_path: Path) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    assert not sess.tool_calls.named("Grep").any()
    assert sess.tool_calls.named("Task").count() == 1


@pytest.mark.parametrize(
    ("method", "args", "deep_expected"),
    [
        ("has_tool", ("Grep",), True),
        ("has_tool", ("Glob",), False),
        ("has_command", ("cargo", "build"), True),
        ("has_command", ("deeptool", "run"), True),
        ("has_command", ("nonesuch",), False),
        ("has_edit_to", ("/deep/only.py",), True),
        ("has_edit_to", ("*.md",), False),
        ("has_read", ("nested.py",), True),
        ("has_read", ("missing.py",), False),
        ("has_skill", ("deepskill",), True),
        ("has_skill", ("elsewhere",), False),
    ],
    ids=[
        "tool-grep-depth2",
        "tool-glob-absent",
        "command-cargo-depth1",
        "command-deeptool-depth2",
        "command-absent",
        "edit-depth2",
        "edit-absent",
        "read-depth2",
        "read-absent",
        "skill-depth2",
        "skill-absent",
    ],
)
def test_has_star_equivalence_over_walk(
    tmp_path: Path, method: str, args: tuple[str, ...], deep_expected: bool
) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    assert getattr(sess, method)(*args) is deep_expected
    assert getattr(sess, method)(*args, subagents=False) is False


def test_empty_after_window_still_unions_descendants(tmp_path: Path) -> None:
    sess = Session.from_path(write_nested_subagent_transcripts(tmp_path))
    empty = sess.after(tool="Grep")
    assert len(empty) == 0
    assert empty.has_tool("Grep")
    assert not empty.has_tool("Grep", subagents=False)


def write_attachment_transcript(root: Path, name: str, id: str, tool: str, **inp: Any) -> Path:
    path = root / "ext" / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "\n".join(
            [
                user_line("x0", 0, "external"),
                assistant_line("x1", 1, [tool_block(id, tool, **inp)]),
            ]
        )
        + "\n"
    )
    return path


def test_windowing_preserves_attachments(tmp_path: Path) -> None:
    main = write_nested_subagent_transcripts(tmp_path)
    att = write_attachment_transcript(tmp_path, "ext.jsonl", "xg1", "Glob", pattern="*.rs")
    base = Session.from_path(main)
    sess = Session(base.turns, base.path, (att,))
    assert sess.has_tool("Glob")
    assert sess.has_tool("Grep")
    narrowed = sess.after(tool="Task")
    assert narrowed.attachments == (att,)
    assert narrowed.has_tool("Glob")
    assert narrowed.has_tool("Grep")


def test_current_turn_preserves_attachments(tmp_path: Path) -> None:
    main = write_nested_subagent_transcripts(tmp_path)
    att = write_attachment_transcript(tmp_path, "ext.jsonl", "xg1", "Glob", pattern="*.rs")
    base = Session.from_path(main)
    sess = Session(base.turns, base.path, (att,))
    turn = sess.current_turn
    assert turn.attachments == (att,)
    assert turn.has_tool("Glob")
    assert not turn.has_tool("Glob", subagents=False)


def test_deep_dedupes_attachment_equal_to_sidechain(tmp_path: Path) -> None:
    main = write_nested_subagent_transcripts(tmp_path)
    agent_a = main.parent / main.stem / "subagents" / "agent-a.jsonl"
    base = Session.from_path(main)
    sess = Session(base.turns, base.path, (agent_a,))
    resolved = [d.path.resolve() for d in sess.walk()]
    assert resolved.count(agent_a.resolve()) == 1
    a_visit = next(d for d in sess.walk() if d.path.resolve() == agent_a.resolve())
    assert a_visit.spawned_by == ToolUseId("a")


def test_deep_dedupes_symlink_spelling_and_double_registration(tmp_path: Path) -> None:
    main = write_main_transcript(tmp_path)
    write_subagent_transcripts(main)
    real = write_attachment_transcript(tmp_path, "rollout.jsonl", "xg", "Glob", pattern="*")
    link = tmp_path / "ext" / "alias.jsonl"
    link.symlink_to(real)
    base = Session.from_path(main)
    sess = Session(base.turns, base.path, (real, link, real))
    ext_visits = [d for d in sess.walk() if d.path.resolve() == real.resolve()]
    assert len(ext_visits) == 1


def test_walk_terminates_on_symlink_cycle(tmp_path: Path) -> None:
    main = tmp_path / "proj" / f"{SESSION}.jsonl"
    main.parent.mkdir(parents=True)
    main.write_text(
        "\n".join(
            [
                user_line("u0", 0, "go"),
                assistant_line("a0", 1, [tool_block("a", "Task", prompt="p", subagent_type="w")]),
            ]
        )
        + "\n"
    )
    a_dir = main.parent / main.stem / "subagents"
    a_dir.mkdir(parents=True)
    agent_a = a_dir / "agent-a.jsonl"
    agent_a.write_text(
        "\n".join(
            [
                user_line("s0", 1, "a", isSidechain=True),
                assistant_line("s1", 2, [tool_block("g", "Grep", pattern="X")], isSidechain=True),
            ]
        )
        + "\n"
    )
    cycle_dir = a_dir / "agent-a" / "subagents"
    cycle_dir.mkdir(parents=True)
    (cycle_dir / "agent-a.jsonl").symlink_to(agent_a)
    walked = list(Session.from_path(main).walk())
    assert [d.path.name for d in walked].count("agent-a.jsonl") == 1
    assert Session.from_path(main).has_tool("Grep")


def test_pathless_session_with_attachments_walks_only_attachments(tmp_path: Path) -> None:
    att = write_attachment_transcript(tmp_path, "ext.jsonl", "xg1", "WebFetch", url="http://x")
    sess = Session(session(user("u0", "go")).turns, None, (att,))
    walked = list(sess.walk())
    assert [d.path.resolve() for d in walked] == [att.resolve()]
    assert walked[0].depth == 1
    assert walked[0].spawned_by is None
    assert sess.has_tool("WebFetch")


def test_walk_descends_children_of_unreadable_transcript(tmp_path: Path) -> None:
    """An unreadable (missing/OSError) transcript is skipped, its children still walked.

    The parent file never exists — a pruned or permission-denied transcript —
    but its derived ``<stem>/subagents/`` dir carries a real child. ``walk()``
    must yield the child even though the parent failed to load.
    """
    missing_parent = tmp_path / "gone" / "rollout.jsonl"
    child_dir = missing_parent.parent / missing_parent.stem / "subagents"
    child_dir.mkdir(parents=True)
    child = child_dir / "agent-x.jsonl"
    child.write_text(
        "\n".join(
            [
                user_line("s0", 1, "child", isSidechain=True),
                assistant_line("s1", 2, [tool_block("g1", "Grep", pattern="CHILD_TODO")], isSidechain=True),
            ]
        )
        + "\n"
    )
    sess = Session(session(user("u0", "go")).turns, None, (missing_parent,))
    walked = list(sess.walk())
    assert [d.path.resolve() for d in walked] == [child.resolve()]
    assert walked[0].depth == 2
    assert walked[0].spawned_by == ToolUseId("x")
    assert sess.has_tool("Grep")


def test_walk_is_lazy(tmp_path: Path) -> None:
    import cc_transcript.query as query_module

    main = write_main_transcript(tmp_path)
    write_subagent_transcripts(main)
    sess = Session.from_path(main)
    calls: list[Path] = []
    real_parse = query_module.parse
    query_module.parse = lambda p: (calls.append(p), real_parse(p))[1]
    try:
        view = sess.deep
        sess.walk()
        assert calls == []
        assert sess.has_tool("Task")
        assert calls == []
        assert len(view.sessions) == 2
        assert len(calls) >= 2
    finally:
        query_module.parse = real_parse


GROWTH_MATERIAL = pathlib.Path(__file__).resolve().parent / "testdata" / "views_edge" / "edge_core.jsonl"


def grow(path: Path, text: str | bytes) -> None:
    with path.open("ab") as handle:
        handle.write(text.encode() if isinstance(text, str) else text)


def step_line(uuid: str, secs: int, *words: str) -> str:
    return assistant_line(uuid, secs, [tool_block(uuid, "Bash", command=" ".join(words))], isSidechain=True) + "\n"


def only_child(main: Path) -> Path:
    return main.parent / main.stem / "subagents" / "agent-0.jsonl"


def assert_deep_matches_cold(main: Path, child: Path) -> None:
    cold = Session.from_path(child)
    sess = Session.from_path(main)
    (walked,) = sess.walk()
    assert walked.session.turns == cold.turns
    assert walked.path == child
    assert list(sess.deep_inputs())[1] == PredicateInputs.of(cold)
    for name in ("Bash", "Edit", "Read", "Grep"):
        assert sess.has_tool(name) == (sess.has_tool(name, subagents=False) or cold.has_tool(name, subagents=False))


def test_a_sidechain_grown_in_pieces_matches_a_cold_walk_at_every_step(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    child.write_bytes(b"")
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    lifts = count_lifts(monkeypatch)
    assert_deep_matches_cold(main, child)
    for line in GROWTH_MATERIAL.read_bytes().splitlines(keepends=True):
        half = len(line) // 2
        for piece in (line[:half], line[half:-1], line[-1:]):
            grow(child, piece)
            assert_deep_matches_cold(main, child)
    assert len(DEEP_LIFTS) == 1
    assert DEEP_LIFTS.size == child.stat().st_size
    assert lifts.count(child) == 2


def test_a_sidechain_grown_a_line_at_a_time_is_extended_not_relifted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    assert_deep_matches_cold(main, child)
    lifts = count_lifts(monkeypatch)
    for line in GROWTH_MATERIAL.read_bytes().splitlines(keepends=True):
        grow(child, line)
        assert_deep_matches_cold(main, child)
    assert lifts == []


def test_deep_predicates_hold_a_sidechain_only_once_it_has_changed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 2)
    child = main.parent / main.stem / "subagents" / "agent-1.jsonl"
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    assert not Session.from_path(main).has_command("step", "later")
    assert len(DEEP_LIFTS) == 0

    grow(child, step_line("b1", 3, "step", "later"))
    lifts = count_lifts(monkeypatch)
    assert Session.from_path(main).has_command("step", "later")
    assert lifts == [child]
    assert len(DEEP_LIFTS) == 1
    assert DEEP_LIFTS.size == child.stat().st_size

    grow(child, step_line("b2", 4, "step", "again"))
    assert Session.from_path(main).has_command("step", "again")
    assert lifts == [child]
    assert DEEP_LIFTS.size == child.stat().st_size


def held_after_one_growth(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[Path, Path, list[Path]]:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    list(Session.from_path(main).walk())
    grow(child, step_line("b1", 3, "step", "one"))
    lifts = count_lifts(monkeypatch)
    assert Session.from_path(main).has_command("step", "one")
    assert lifts == []
    return main, child, lifts


def test_a_shrunk_sidechain_is_lifted_afresh(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    child.write_bytes(child.read_bytes()[: -len(step_line("b1", 3, "step", "one"))])
    assert not Session.from_path(main).has_command("step", "one")
    assert lifts == [child]
    assert_deep_matches_cold(main, child)


def test_a_sidechain_rewritten_at_the_same_size_is_lifted_afresh(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    child.write_bytes(child.read_bytes().replace(b"step one", b"step two"))
    assert Session.from_path(main).has_command("step", "two")
    assert not Session.from_path(main).has_command("step", "one")
    assert lifts == [child]


def test_a_sidechain_rewritten_and_grown_is_lifted_afresh(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    child.write_bytes(
        child.read_bytes().replace(b"step one", b"step two") + step_line("b2", 4, "step", "three").encode()
    )
    assert Session.from_path(main).has_command("step", "three")
    assert not Session.from_path(main).has_command("step", "one")
    assert lifts == [child]
    assert_deep_matches_cold(main, child)


def test_a_replaced_sidechain_is_lifted_afresh(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_bytes(child.read_bytes() + step_line("b2", 4, "step", "two").encode())
    os.replace(fresh, child)
    assert Session.from_path(main).has_command("step", "two")
    assert lifts == [child]
    assert_deep_matches_cold(main, child)


def test_a_growth_longer_than_the_fence_is_extended(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    grow(child, "".join(step_line(f"b{k}", 4 + k, "step", str(k)) for k in range(2, 40)))
    assert Session.from_path(main).has_command("step", "39")
    assert lifts == []
    assert_deep_matches_cold(main, child)


def test_a_held_cursor_goes_with_its_evicted_entry(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    monkeypatch.setattr("cc_transcript.query.DEEP_LIFT_BUDGET", 1)
    grow(child, step_line("b2", 4, "step", "two"))
    assert Session.from_path(main).has_command("step", "two")
    assert lifts == []
    assert len(DEEP_LIFTS) == 0
    grow(child, step_line("b3", 5, "step", "three"))
    assert Session.from_path(main).has_command("step", "three")
    assert lifts == [child]


def test_racing_deep_predicates_over_a_growing_sidechain_agree_with_a_cold_walk(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    root = Session.from_path(main)
    assert root.has_command("step", "0")
    stop = threading.Event()

    def read() -> int:
        seen = 0
        while not stop.is_set():
            steps = sorted(
                int(command.split()[1])
                for inputs in root.deep_inputs()
                for command in inputs.commands
                if command.startswith("grow ")
            )
            assert steps == list(range(len(steps))), steps
            assert len(steps) >= seen
            seen = len(steps)
        return seen

    with ThreadPoolExecutor(max_workers=16) as pool:
        readers = [pool.submit(read) for _ in range(16)]
        for k in range(200):
            grow(child, step_line(f"g{k}", 3 + k, "grow", str(k)))
        stop.set()
        seen = [future.result() for future in readers]

    assert all(count > 0 for count in seen)
    assert_deep_matches_cold(main, child)
    assert len(DEEP_LIFTS) == 1
    assert Session.from_path(main).has_command("grow", "199")


def test_a_barrier_forced_take_put_race_publishes_twice_and_the_later_put_wins(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    key = (child, 1, ToolUseId("0"))
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    list(Session.from_path(main).walk())
    assert Session.from_path(main).has_command("step", "0")
    assert SIDECHAIN_INPUTS.holds(child)
    grow(child, step_line("g", 3, "step", "grown"))

    parked = threading.Event()
    release = threading.Event()
    real = query.grown_lift

    def parking(held, path, stamp):
        result = real(held, path, stamp)
        parked.set()
        assert release.wait(5)
        return result

    monkeypatch.setattr(query, "grown_lift", parking)
    with ThreadPoolExecutor(max_workers=1) as pool:
        extender = pool.submit(lambda: Session.from_path(main).has_command("step", "grown"))
        assert parked.wait(5)
        assert key not in DEEP_LIFTS.held
        assert Session.from_path(child).has_command("step", "grown", subagents=False)
        monkeypatch.undo()
        assert Session.from_path(main).has_command("step", "grown")
        racer = DEEP_LIFTS.held[key]
        assert racer.deep.session.has_command("step", "grown", subagents=False)
        release.set()
        assert extender.result()

    assert list(DEEP_LIFTS.held) == [key]
    assert DEEP_LIFTS.held[key] is not racer
    assert DEEP_LIFTS.held[key].deep.session.turns == racer.deep.session.turns
    assert_deep_matches_cold(main, child)


class Clock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def install_clock(monkeypatch: pytest.MonkeyPatch) -> Clock:
    monkeypatch.setattr(query, "monotonic", clock := Clock())
    return clock


def test_a_burst_of_hits_never_releases_a_recently_grown_cursor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    clock = install_clock(monkeypatch)
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    assert len(DEEP_LIFTS) == 1

    clock.advance(query.IDLE_SECONDS_BEFORE_RELEASE - 1)
    for _ in range(50):
        assert Session.from_path(main).has_command("step", "one")
    assert len(DEEP_LIFTS) == 1

    clock.advance(1)
    assert Session.from_path(main).has_command("step", "one")
    assert len(DEEP_LIFTS) == 0
    assert SIDECHAIN_INPUTS.holds(child)
    assert lifts == []

    grow(child, step_line("b2", 4, "step", "two"))
    assert Session.from_path(main).has_command("step", "two")
    assert lifts == [child]
    assert len(DEEP_LIFTS) == 1
    assert_deep_matches_cold(main, child)


def test_a_growth_resets_the_idle_clock(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    clock = install_clock(monkeypatch)
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    for index in range(2, 6):
        clock.advance(query.IDLE_SECONDS_BEFORE_RELEASE - 1)
        grow(child, step_line(f"b{index}", index + 2, "step", str(index)))
        assert Session.from_path(main).has_command("step", str(index))
        assert len(DEEP_LIFTS) == 1
    assert lifts == []


def test_the_parity_sweep_holds_across_an_idle_release(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    clock = install_clock(monkeypatch)
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    assert_deep_matches_cold(main, child)
    for index, line in enumerate(GROWTH_MATERIAL.read_bytes().splitlines(keepends=True)):
        grow(child, line)
        assert_deep_matches_cold(main, child)
        if index % 3 == 0:
            clock.advance(query.IDLE_SECONDS_BEFORE_RELEASE)
            assert_deep_matches_cold(main, child)
    assert_deep_matches_cold(main, child)


def bad_line(uuid: str, secs: int) -> str:
    return json.dumps({"uuid": uuid, "type": "assistant", "sessionId": str(SESSION), "isSidechain": True}) + "\n"


def test_an_unreadable_sidechain_is_parsed_once_until_it_changes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    grow(child, bad_line("b1", 3))
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    UNREADABLE.clear()
    parses: list[Path] = []
    real = query.parsed
    monkeypatch.setattr(query, "parsed", lambda path, *args: parses.append(path) or real(path, *args))

    for _ in range(3):
        assert list(Session.from_path(main).walk()) == []
        assert not Session.from_path(main).has_command("step", "0")
    assert parses == [child]
    assert len(UNREADABLE) == 1

    child.write_bytes(
        child.read_bytes().replace(bad_line("b1", 3).encode(), step_line("b2", 4, "step", "fixed").encode())
    )
    assert Session.from_path(main).has_command("step", "fixed")
    assert parses[1:] == [child]


def test_a_readable_sidechain_that_grows_a_bad_line_is_skipped_until_it_changes(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    UNREADABLE.clear()
    assert Session.from_path(main).has_command("step", "0")

    grow(child, bad_line("b1", 3))
    assert list(Session.from_path(main).walk()) == []
    assert not Session.from_path(main).has_command("step", "0")
    assert len(DEEP_LIFTS) == 0
    assert len(UNREADABLE) == 1

    child.write_bytes(
        child.read_bytes().replace(bad_line("b1", 3).encode(), step_line("b2", 3, "step", "fixed").encode())
    )
    assert Session.from_path(main).has_command("step", "fixed")
    assert_deep_matches_cold(main, child)


def test_a_transient_read_error_is_not_held_and_retries_next_walk(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    UNREADABLE.clear()
    real = pathlib.Path.open

    def failing(self, *args, **kwargs):
        if self == child:
            raise OSError(5, "transient I/O error")
        return real(self, *args, **kwargs)

    monkeypatch.setattr(pathlib.Path, "open", failing)
    assert not Session.from_path(main).has_command("step", "0")
    assert len(UNREADABLE) == 0

    monkeypatch.undo()
    assert Session.from_path(main).has_command("step", "0")


def test_a_read_time_permission_error_is_not_held_and_retries_once_readable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    UNREADABLE.clear()
    real = pathlib.Path.open

    def denied(self, *args, **kwargs):
        if self == child:
            raise PermissionError(13, "permission denied")
        return real(self, *args, **kwargs)

    monkeypatch.setattr(pathlib.Path, "open", denied)
    assert not Session.from_path(main).has_command("step", "0")
    assert len(UNREADABLE) == 0

    monkeypatch.undo()
    assert Session.from_path(main).has_command("step", "0")


def replace_before_read(monkeypatch: pytest.MonkeyPatch, target: Path, contents: bytes) -> None:
    real = query.appended_bytes
    seen = {"done": False}

    def swapping(path: Path, start: int, stop: int, stamp) -> bytes | None:
        if path == target and not seen["done"]:
            seen["done"] = True
            fresh = target.with_name("replacement.jsonl")
            fresh.write_bytes(contents)
            os.replace(fresh, target)
        return real(path, start, stop, stamp)

    monkeypatch.setattr(query, "appended_bytes", swapping)


def test_an_equal_length_replacement_during_a_growth_read_falls_to_a_cold_relift(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    child.write_bytes(step_line("old", 1, "step", "old").encode() + user_line("pad", 2, "x" * 200).encode() + b"\n")
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    list(Session.from_path(main).walk())
    held = next(iter(DEEP_LIFTS.held.values()))
    grow(child, step_line("add", 3, "step", "add"))
    replacement = child.read_bytes().replace(b"step old", b"step new")
    assert child.read_bytes()[held.consumed - len(held.fence) : held.consumed] == held.fence

    lifts = count_lifts(monkeypatch)
    replace_before_read(monkeypatch, child, replacement)
    sess = Session.from_path(main)
    assert not sess.has_command("step", "old")
    assert sess.has_command("step", "new")
    assert lifts == [child]
    assert_deep_matches_cold(main, child)


CODEX_FIXTURE = (
    pathlib.Path(__file__).resolve().parent
    / "testdata"
    / "codex"
    / "rollout-2026-01-20T04-00-05-019bd9c0-0a1b-7c2d-8e3f-000000000101.jsonl"
)


def test_a_codex_response_item_appended_to_a_claude_attachment_reads_as_a_cold_walk_does(tmp_path: Path) -> None:
    path = tmp_path / "mixed.jsonl"
    path.write_text(user_line("u0", 0, "start") + "\n")
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    root = Session((), None, (path,))
    assert not root.has_tool("exec_command")

    with path.open("a") as handle:
        handle.write(
            json.dumps(
                {
                    "timestamp": "2026-01-01T00:00:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": '{"cmd":"echo INCREMENTAL_ONLY"}',
                        "call_id": "call-1",
                    },
                }
            )
            + "\n"
        )
    cold = Session.from_path(path)
    assert root.has_tool("exec_command") == cold.has_tool("exec_command", subagents=False)
    assert [inputs.commands for inputs in root.deep_inputs()][1:] == [cold.predicate_inputs.commands]
    assert not root.has_command("echo", "INCREMENTAL_ONLY")


def test_a_growing_codex_attachment_matches_a_cold_walk_at_every_line(tmp_path: Path) -> None:
    lines = CODEX_FIXTURE.read_bytes().splitlines(keepends=True)
    path = tmp_path / CODEX_FIXTURE.name
    path.write_bytes(lines[0])
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    root = Session((), None, (path,))

    def deep_matches_cold() -> None:
        cold = Session.from_path(path)
        (walked,) = root.walk()
        assert walked.provider == "codex"
        assert walked.session.turns == cold.turns
        assert [inputs.commands for inputs in root.deep_inputs()][1] == cold.predicate_inputs.commands
        assert root.has_tool("Bash") == cold.has_tool("Bash", subagents=False)

    deep_matches_cold()
    for line in lines[1:]:
        with path.open("ab") as handle:
            handle.write(line)
        deep_matches_cold()


def test_a_retargeted_attachment_symlink_is_reached_after_the_retarget(tmp_path: Path) -> None:
    target = write_attachment_transcript(tmp_path, "target.jsonl", "t", "Bash", command="reach target")
    other = write_attachment_transcript(tmp_path, "other.jsonl", "o", "Read", file_path="other")
    link = tmp_path / "ext" / "link.jsonl"
    link.symlink_to(other)
    root = Session((), None, (link,))
    assert not root.has_command("reach", "target")
    assert [d.path.name for d in root.walk()] == ["link.jsonl"]

    link.unlink()
    link.symlink_to(target)
    assert root.has_command("reach", "target")
    assert [d.path.resolve() for d in root.walk()] == [target.resolve()]


def test_append_only_is_the_contract_a_rewrite_behind_the_fence_reads_as_growth(tmp_path: Path) -> None:
    main = write_flat_subagent_tree(tmp_path, "lead", 1)
    child = only_child(main)
    child.write_bytes(step_line("old", 1, "step", "old").encode() + user_line("pad", 2, "x" * 200).encode() + b"\n")
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    list(Session.from_path(main).walk())
    held = next(iter(DEEP_LIFTS.held.values()))

    child.write_bytes(
        child.read_bytes().replace(b"step old", b"step new") + step_line("add", 3, "step", "add").encode()
    )
    assert child.read_bytes()[held.consumed - len(held.fence) : held.consumed] == held.fence

    sess = Session.from_path(main)
    assert sess.has_command("step", "add")
    assert sess.has_command("step", "old")
    assert not sess.has_command("step", "new")
    cold = Session.from_path(child)
    assert cold.has_command("step", "new", subagents=False)
    assert not cold.has_command("step", "old", subagents=False)


class AppendingReader:
    def __init__(self, handle, target: Path, line: str) -> None:
        self.handle, self.target, self.line = handle, target, line

    def __enter__(self) -> AppendingReader:
        return self

    def __exit__(self, *args: object) -> None:
        self.handle.__exit__(*args)

    def fileno(self) -> int:
        return self.handle.fileno()

    def read(self, size: int) -> bytes:
        raw = self.handle.read(size)
        with self.target.open("a") as out:
            out.write(self.line)
        return raw


def opening_through(monkeypatch: pytest.MonkeyPatch, link: Path, target: Path, restore: Path, line: str | None):
    real = pathlib.Path.open

    def swapped(self, *args, **kwargs):
        if self != link or args != ("rb",):
            return real(self, *args, **kwargs)
        if target != link:
            link.unlink()
            link.symlink_to(target)
        handle = real(link, *args, **kwargs)
        if target != link:
            link.unlink()
            link.symlink_to(restore)
        return handle if line is None else AppendingReader(handle, target, line)

    monkeypatch.setattr(pathlib.Path, "open", swapped)


def test_an_unsettled_cold_read_publishes_to_no_cache(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / "a.jsonl"
    path.write_text(step_line("a", 1, "step", "a"))
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    root = Session((), None, (path,))
    opening_through(monkeypatch, path, path, path, user_line("later", 9, "append during cold read") + "\n")
    assert root.has_command("step", "a")
    assert len(DEEP_LIFTS) == 0
    assert len(SIDECHAIN_INPUTS) == 0
    monkeypatch.undo()
    assert root.has_command("step", "a")
    assert len(SIDECHAIN_INPUTS) == 1


def test_a_link_swapped_under_an_unsettled_read_does_not_cache_the_other_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    a, b, link = (tmp_path / name for name in ("a.jsonl", "b.jsonl", "link.jsonl"))
    a.write_text(step_line("a", 1, "step", "a"))
    b.write_text(step_line("b", 1, "step", "b"))
    link.symlink_to(a)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    root = Session((), None, (link,))
    opening_through(monkeypatch, link, b, a, user_line("later", 9, "append during cold read") + "\n")
    assert root.has_command("step", "b")
    monkeypatch.undo()
    assert [root.has_command("step", "b") for _ in range(3)] == [False, False, False]
    assert root.has_command("step", "a")


def test_a_link_swapped_to_an_unparseable_file_is_not_held_against_the_readable_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    a, b, link = (tmp_path / name for name in ("a.jsonl", "b.jsonl", "link.jsonl"))
    a.write_text(step_line("a", 1, "step", "a"))
    b.write_text(bad_line("b", 1))
    link.symlink_to(a)
    DEEP_LIFTS.clear()
    SIDECHAIN_INPUTS.clear()
    UNREADABLE.clear()
    root = Session((), None, (link,))
    opening_through(monkeypatch, link, b, a, None)
    assert not root.has_command("step", "a")
    assert len(UNREADABLE) == 1
    assert UNREADABLE.has(link, query.stamp_of(b.stat()))
    monkeypatch.undo()
    assert [root.has_command("step", "a") for _ in range(3)] == [True, True, True]


def test_a_bad_line_appended_to_a_held_cursor_is_held_after_its_first_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    main, child, lifts = held_after_one_growth(tmp_path, monkeypatch)
    UNREADABLE.clear()
    parses: list[Path] = []
    real = query.parsed
    monkeypatch.setattr(query, "parsed", lambda path, *args: parses.append(path) or real(path, *args))

    grow(child, bad_line("b2", 4))
    assert not Session.from_path(main).has_command("step", "one")
    assert len(UNREADABLE) == 1
    assert UNREADABLE.has(child, query.stamp_of(child.stat()))
    assert parses == [child]
    assert lifts == []

    assert not Session.from_path(main).has_command("step", "one")
    assert parses == [child]
    assert lifts == []


def write_bash_transcript(path: Path, uuid: str, command: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "\n".join(
            [
                user_line(f"{uuid}-u", 0, "go"),
                assistant_line(f"{uuid}-a", 1, [tool_block(uuid, "Bash", command=command)]),
            ]
        )
        + "\n"
    )
    return path


def test_a_retarget_in_the_middle_of_a_symlink_chain_is_followed(tmp_path: Path) -> None:
    a = write_bash_transcript(tmp_path / "a.jsonl", "a", "from A")
    b = write_bash_transcript(tmp_path / "b.jsonl", "b", "from B")
    middle = tmp_path / "middle.jsonl"
    link = tmp_path / "link.jsonl"
    middle.symlink_to(a)
    link.symlink_to(middle)
    root = Session((), None, (a, link))
    assert [d.path.name for d in root.walk()] == ["a.jsonl"]
    assert not root.has_command("from", "B")

    middle.unlink()
    middle.symlink_to(b)
    assert [d.path.name for d in root.walk()] == ["a.jsonl", "link.jsonl"]
    assert root.has_command("from", "B")


def test_a_directory_retarget_between_hard_linked_transcripts_reaches_the_new_children(tmp_path: Path) -> None:
    parent_a = write_bash_transcript(tmp_path / "a" / "parent.jsonl", "pa", "parent common")
    parent_b = tmp_path / "b" / "parent.jsonl"
    parent_b.parent.mkdir()
    os.link(parent_a, parent_b)
    write_bash_transcript(tmp_path / "a" / "parent" / "subagents" / "agent-a.jsonl", "ca", "child A")
    write_bash_transcript(tmp_path / "b" / "parent" / "subagents" / "agent-b.jsonl", "cb", "child B")
    current = tmp_path / "current"
    current.symlink_to(tmp_path / "a", target_is_directory=True)
    through = current / "parent.jsonl"
    root = Session((), None, (parent_a, through))
    assert [d.path.name for d in root.walk()] == ["parent.jsonl", "agent-a.jsonl"]
    assert not root.has_command("child", "B")

    current.unlink()
    current.symlink_to(tmp_path / "b", target_is_directory=True)
    assert [d.path.name for d in root.walk()] == ["parent.jsonl", "agent-a.jsonl", "parent.jsonl", "agent-b.jsonl"]
    assert root.has_command("child", "B")


def test_a_relative_symlink_resolves_against_the_directory_it_now_lives_in(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    old, new = tmp_path / "old", tmp_path / "new"
    write_bash_transcript(old / "target.jsonl", "new", "from NEW")
    (old / "link.jsonl").symlink_to("target.jsonl")
    monkeypatch.chdir(old)
    link = pathlib.Path("link.jsonl")
    assert Session((), None, (link,)).has_command("from", "NEW")

    os.rename(old, new)
    old_target = write_bash_transcript(old / "target.jsonl", "old", "from OLD")
    parsed = Session.from_path(old_target)
    root = Session(parsed.turns, parsed.path, (link,))
    assert [d.path.name for d in root.walk()] == ["link.jsonl"]
    assert root.has_command("from", "NEW")
    assert root.has_command("from", "OLD", subagents=False)
