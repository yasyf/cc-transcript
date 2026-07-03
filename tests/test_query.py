from __future__ import annotations

import json
import os
import re
from datetime import UTC, datetime, timedelta
from functools import partial
from typing import TYPE_CHECKING, Any

import anyio
import pytest

from cc_transcript.activity import SessionActivity, ToolUse
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import EventUuid, SessionId, ToolUseId
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.query import FileRef, Session, SubagentIndex, SubagentSession, ToolCallQuery

if TYPE_CHECKING:
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("11111111-1111-1111-1111-111111111111")
PLAN_FILE = "/Users/x/.claude/plans/p.md"


def meta(uuid: str, *, secs: int = 0, is_meta: bool = False, is_sidechain: bool = False) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SESSION,
        timestamp=BASE + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=is_sidechain,
        is_meta=is_meta,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(
    uuid: str,
    text: str = "",
    *,
    blocks: tuple[ContentBlock, ...] = (),
    secs: int = 0,
    is_meta: bool = False,
) -> UserEvent:
    return UserEvent(meta=meta(uuid, secs=secs, is_meta=is_meta), text=text, blocks=blocks, interrupted=False)


def assistant(uuid: str, text: str = "", *, blocks: tuple[ContentBlock, ...] = (), secs: int = 0) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-7", text=text, blocks=blocks, stop_reason=None, usage=None
    )


def tool(id: str, name: str, **input: Any) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name=name, input=input)


def bash(id: str, command: str) -> ToolUseBlock:
    return tool(id, "Bash", command=command)


def edit(id: str, path: str, old: str = "a", new: str = "b") -> ToolUseBlock:
    return tool(id, "Edit", file_path=path, old_string=old, new_string=new)


def write(id: str, path: str, content: str = "x") -> ToolUseBlock:
    return tool(id, "Write", file_path=path, content=content)


def read(id: str, path: str) -> ToolUseBlock:
    return tool(id, "Read", file_path=path)


def result(id: str, content: str = "ok", *, is_error: bool = False) -> ToolResultBlock:
    return ToolResultBlock(tool_use_id=ToolUseId(id), content=content, is_error=is_error)


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
    assert calls.list() == [*calls.items]
    assert (first := calls.first()) is not None and first.ref.tool_use_id == ToolUseId("t1")
    assert (last := calls.last()) is not None and last.ref.tool_use_id == ToolUseId("t2")
    empty = calls.named("Grep")
    assert isinstance(empty, ToolCallQuery)
    assert (empty.count(), empty.any(), empty.first(), empty.last(), empty.list()) == (0, False, None, None, [])


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
        assistant_line(
            "a0", 1, [tool_block("t9", "Task", prompt="run tests", subagent_type="test-runner")]
        ),
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
                assistant_line(
                    "s1", 2, [tool_block("b1", "Bash", command="uv run pytest")], isSidechain=True
                ),
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
    sess = anyio.run(partial(Session.from_id, SESSION, root=tmp_path))
    assert sess.path == main
    assert sess.first_prompt == "run the tests"


def test_from_id_raises_expired_when_transcript_gone(tmp_path: Path) -> None:
    with pytest.raises(TranscriptExpiredError) as excinfo:
        anyio.run(partial(Session.from_id, SESSION, root=tmp_path))
    assert excinfo.value.session_id == SESSION
