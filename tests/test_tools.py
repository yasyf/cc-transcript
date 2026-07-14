from __future__ import annotations

import pytest

from cc_transcript.command import CommandLine
from cc_transcript.ids import tool_digest
from cc_transcript.models import Question
from cc_transcript.tools import (
    AskUserQuestionResult,
    BashCall,
    BashResult,
    EditCall,
    EditResult,
    EditSpan,
    GrepCall,
    Hunk,
    MultiEditCall,
    NotebookEditCall,
    OtherCall,
    OtherResult,
    QuestionAnnotation,
    ReadResult,
    SkillResult,
    TaskCall,
    TaskLaunchResult,
    TaskResult,
    TaskUpdateCall,
    TextResult,
    ToolInputError,
    WorkflowCall,
    WriteCall,
    WriteResult,
    expand_tool_names,
    file_path_of,
    hunks_of,
    matches_names,
    mcp_access,
    mcp_parts,
    parse_tool_call,
    parse_tool_result,
    tool_name_matches,
)


def test_edit_parses_typed_fields() -> None:
    call = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
    assert isinstance(call, EditCall)
    assert (call.name, call.file_path, call.old, call.new, call.replace_all) == ("Edit", "a.py", "x", "y", False)
    assert call.raw["old_string"] == "x"  # raw excluded from equality


def test_multiedit_keeps_every_span_in_order() -> None:
    call = parse_tool_call(
        "MultiEdit",
        {
            "file_path": "a.py",
            "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c", "new_string": "d", "replace_all": True},
            ],
        },
    )
    assert isinstance(call, MultiEditCall)
    assert call.edits == (EditSpan("a", "b"), EditSpan("c", "d", replace_all=True))
    assert hunks_of(call) == (Hunk("a", "b"), Hunk("c", "d"))


def test_bash_and_aliases_parse_to_the_same_shape() -> None:
    bash = parse_tool_call("Bash", {"command": "ls"})
    execute = parse_tool_call("Execute", {"command": "ls"})
    assert isinstance(bash, BashCall) and isinstance(execute, BashCall)
    assert bash.name == "Bash" and execute.name == "Execute"
    assert isinstance(parse_tool_call("Create", {"file_path": "f", "content": "c"}), WriteCall)
    assert isinstance(parse_tool_call("Task", {"prompt": "p"}), TaskCall)


def test_bash_command_line_parses_to_a_command_line() -> None:
    call = parse_tool_call("Bash", {"command": "sudo git push -f && echo hi"})
    assert isinstance(call, BashCall)
    assert isinstance(call.command_line, CommandLine)
    assert call.command_line.prefixes == ("git push", "echo")


def test_task_normalizes_subagent_type() -> None:
    call = parse_tool_call("Agent", {"prompt": "p", "subagent_type": "Explore", "name": "scout"})
    assert isinstance(call, TaskCall)
    assert (call.agent_type, call.agent_name) == ("Explore", "scout")


def test_workflow_parses_typed_fields() -> None:
    call = parse_tool_call(
        "Workflow",
        {"script": "export const meta = {}", "args": ["a.ts"], "resumeFromRunId": "wf_abc123"},
    )
    assert isinstance(call, WorkflowCall)
    assert (call.script, call.script_path, call.args) == ("export const meta = {}", None, ["a.ts"])
    assert call.resume_from_run_id == "wf_abc123"


def test_workflow_distinguishes_workflow_name_from_tool_name() -> None:
    call = parse_tool_call("Workflow", {"name": "review-changes", "scriptPath": "/tmp/wf.js"})
    assert isinstance(call, WorkflowCall)
    assert (call.name, call.workflow_name, call.script_path) == ("Workflow", "review-changes", "/tmp/wf.js")
    assert call.script is None


def test_dual_key_first_present_key_wins_over_truthiness() -> None:
    call = parse_tool_call("Workflow", {"scriptPath": "", "script_path": "x"})
    assert isinstance(call, WorkflowCall)
    assert call.script_path == ""


def test_dual_key_explicit_null_falls_through_to_next_key() -> None:
    call = parse_tool_call("Agent", {"prompt": "p", "subagent_type": None, "agent_type": "Explore"})
    assert isinstance(call, TaskCall)
    assert call.agent_type == "Explore"


def test_task_update_accepts_either_task_id_spelling() -> None:
    for raw in ({"taskId": "T1", "status": "completed"}, {"task_id": "T1", "status": "completed"}):
        call = parse_tool_call("TaskUpdate", raw)
        assert isinstance(call, TaskUpdateCall)
        assert (call.task_id, call.status) == ("T1", "completed")


def test_task_update_null_task_id_falls_through_to_the_other_spelling() -> None:
    call = parse_tool_call("TaskUpdate", {"taskId": None, "task_id": "abc"})
    assert isinstance(call, TaskUpdateCall)
    assert call.task_id == "abc"


def test_task_update_all_null_task_id_raises_by_default() -> None:
    with pytest.raises(ToolInputError, match="TaskUpdate input missing"):
        parse_tool_call("TaskUpdate", {"taskId": None})


def test_grep_maps_type_to_file_type() -> None:
    call = parse_tool_call("Grep", {"pattern": "x", "type": "py"})
    assert isinstance(call, GrepCall) and call.file_type == "py"


def test_unknown_and_mcp_tools_parse_to_other() -> None:
    call = parse_tool_call("mcp__github__search", {"q": "x"})
    assert isinstance(call, OtherCall) and call.raw.get("q") == "x"


REQUIRED_STR_FIELDS = [
    ("Bash", {"command": "ls"}, "command"),
    ("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"}, "file_path"),
    ("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"}, "old_string"),
    ("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"}, "new_string"),
    ("MultiEdit", {"file_path": "a.py", "edits": [{"old_string": "a", "new_string": "b"}]}, "file_path"),
    ("Write", {"file_path": "f.py", "content": "body"}, "file_path"),
    ("Write", {"file_path": "f.py", "content": "body"}, "content"),
    ("Read", {"file_path": "r.py"}, "file_path"),
    ("NotebookEdit", {"notebook_path": "n.ipynb", "new_source": "src"}, "notebook_path"),
    ("NotebookEdit", {"notebook_path": "n.ipynb", "new_source": "src"}, "new_source"),
    ("Grep", {"pattern": "x"}, "pattern"),
    ("Glob", {"pattern": "*.py"}, "pattern"),
    ("Agent", {"prompt": "p"}, "prompt"),
    ("Skill", {"skill": "verify"}, "skill"),
    ("TaskCreate", {"subject": "s"}, "subject"),
    ("TaskUpdate", {"taskId": "T1"}, "taskId"),
    ("ExitPlanMode", {"plan": "p"}, "plan"),
]


@pytest.mark.parametrize("mutation", ["missing", "int-typed", "explicit-null"])
@pytest.mark.parametrize(
    ("name", "valid", "key"), REQUIRED_STR_FIELDS, ids=[f"{name}-{key}" for name, _, key in REQUIRED_STR_FIELDS]
)
def test_required_fields_validate_at_the_boundary(name: str, valid: dict[str, object], key: str, mutation: str) -> None:
    assert not isinstance(parse_tool_call(name, valid), OtherCall)
    raw = (
        {k: v for k, v in valid.items() if k != key}
        if mutation == "missing"
        else valid | {key: 42 if mutation == "int-typed" else None}
    )
    with pytest.raises(ToolInputError, match=f"{name} input missing or malformed"):
        parse_tool_call(name, raw)
    degraded = parse_tool_call(name, raw, on_error="other")
    assert isinstance(degraded, OtherCall)
    assert (degraded.name, degraded.raw) == (name, raw)


@pytest.mark.parametrize(
    "edits",
    [
        [{"old_string": 42, "new_string": "b"}],
        [{"old_string": "a", "new_string": None}],
        [{"old_string": "a"}],
        [None],
        ["not-a-span"],
        "not-a-list",
        None,
    ],
    ids=[
        "int-old-string",
        "null-new-string",
        "missing-new-string",
        "null-span",
        "str-span",
        "str-edits",
        "null-edits",
    ],
)
def test_multiedit_validates_spans_shallowly(edits: object) -> None:
    raw = {"file_path": "a.py", "edits": edits}
    with pytest.raises(ToolInputError, match="MultiEdit input missing or malformed"):
        parse_tool_call("MultiEdit", raw)
    degraded = parse_tool_call("MultiEdit", raw, on_error="other")
    assert isinstance(degraded, OtherCall)
    assert degraded.raw == raw


@pytest.mark.parametrize("edits", [[], "", {}], ids=["empty-list", "empty-str", "empty-dict"])
def test_multiedit_empty_iterable_yields_empty_span_list(edits: object) -> None:
    call = parse_tool_call("MultiEdit", {"file_path": "a.py", "edits": edits})
    assert isinstance(call, MultiEditCall)
    assert call.edits == ()


@pytest.mark.parametrize("edits", ["ab", 5, {"k": "v"}], ids=["nonempty-str", "scalar", "nonempty-dict"])
def test_multiedit_nonempty_non_span_edits_degrade_to_other(edits: object) -> None:
    raw = {"file_path": "a.py", "edits": edits}
    with pytest.raises(ToolInputError, match="MultiEdit input missing or malformed"):
        parse_tool_call("MultiEdit", raw)
    assert isinstance(parse_tool_call("MultiEdit", raw, on_error="other"), OtherCall)


@pytest.mark.parametrize("name", ["Edit", "mcp__github__search"], ids=["known-tool", "mcp-tool"])
def test_non_mapping_input_raises_under_strict(name: str) -> None:
    with pytest.raises(ToolInputError, match="must be a mapping"):
        parse_tool_call(name, ["not", "a", "mapping"])  # type: ignore[arg-type]


@pytest.mark.parametrize("name", ["Edit", "mcp__github__search"], ids=["known-tool", "mcp-tool"])
def test_non_mapping_input_degrades_to_empty_other_under_other(name: str) -> None:
    call = parse_tool_call(name, ["not", "a", "mapping"], on_error="other")  # type: ignore[arg-type]
    assert isinstance(call, OtherCall)
    assert (call.name, dict(call.raw)) == (name, {})


def test_non_mapping_degrade_digest_is_the_empty_input_digest() -> None:
    call = parse_tool_call("Bash", None, on_error="other")  # type: ignore[arg-type]
    assert call.digest == tool_digest("Bash", {})


def test_on_error_other_degrades_with_correct_digest() -> None:
    raw = {"file_path": "a.py", "unexpected_shape": True}
    call = parse_tool_call("Edit", raw, on_error="other")
    assert isinstance(call, OtherCall)
    assert call.digest == tool_digest("Edit", raw)


def test_digest_matches_raw_substrate_for_typed_calls() -> None:
    raw = {"file_path": "a.py", "old_string": "x", "new_string": "y", "extra_field": 1}
    call = parse_tool_call("Edit", raw)
    assert call.digest == tool_digest("Edit", raw)


def test_write_and_notebook_lower_to_addition_hunks() -> None:
    write = parse_tool_call("Write", {"file_path": "f.py", "content": "body"})
    nb = parse_tool_call("NotebookEdit", {"notebook_path": "n.ipynb", "new_source": "src"})
    assert hunks_of(write) == (Hunk("", "body"),)
    assert hunks_of(nb) == (Hunk("", "src"),)
    assert hunks_of(parse_tool_call("Bash", {"command": "ls"})) == ()


def test_file_path_of_covers_file_shaped_calls() -> None:
    assert file_path_of(parse_tool_call("Read", {"file_path": "r.py"})) == "r.py"
    nb = parse_tool_call("NotebookEdit", {"notebook_path": "n.ipynb", "new_source": ""})
    assert isinstance(nb, NotebookEditCall)
    assert file_path_of(nb) == "n.ipynb"
    assert file_path_of(parse_tool_call("Bash", {"command": "ls"})) is None


def test_expand_tool_names_includes_both_alias_spellings() -> None:
    assert expand_tool_names("Bash|Write") == frozenset(
        {"Bash", "Execute", "Write", "Create", "ccx_code_replace"}
    )
    assert expand_tool_names("Edit|Write") == frozenset(
        {"Edit", "Write", "Create", "ccx_code_edit", "ccx_code_replace"}
    )
    assert expand_tool_names("Grep") == frozenset({"Grep"})


@pytest.mark.parametrize(
    ("actual", "spec", "expected"),
    [
        ("Execute", "Bash", True),
        ("Bash", "Execute", True),
        ("mcp__github__Grep", "Grep", True),
        ("mcp__semble__search", "search", True),
        ("mcp__github__Grep", "Bash", False),
        ("mcp__server", "server", False),
        ("Read", "Bash|Grep", False),
        ("mcp__cc-context__ccx_code_edit", "Edit|Write|MultiEdit", True),
        ("mcp__cc-context__ccx_code_replace", "Write", True),
        ("mcp__cc-context__ccx_code_read", "Edit|Write|MultiEdit", False),
    ],
    ids=[
        "alias-forward",
        "alias-reverse",
        "mcp-suffix",
        "mcp-suffix-server-tool",
        "mcp-miss",
        "mcp-too-few-parts",
        "plain-miss",
        "ccx-edit-aliases-edit-gate",
        "ccx-replace-aliases-write",
        "ccx-read-not-edit-gate",
    ],
)
def test_tool_name_matches(actual: str, spec: str, expected: bool) -> None:
    assert tool_name_matches(actual, spec) is expected


@pytest.mark.parametrize(
    ("actual", "names", "expected"),
    [
        ("ExitPlanMode", frozenset({"ExitPlanMode", "ExitSpecMode"}), True),
        ("ExitSpecMode", frozenset({"ExitPlanMode", "ExitSpecMode"}), True),
        ("mcp__conductor__ExitPlanMode", frozenset({"ExitPlanMode"}), True),
        ("mcp__ExitPlanMode", frozenset({"ExitPlanMode"}), False),
        ("Execute", frozenset({"Bash"}), False),
        ("mcp__cc-context__ccx_code_edit", frozenset({"Edit"}), True),
        ("mcp__cc-context__ccx_code_replace", frozenset({"Write"}), True),
        ("mcp__cc-context__ccx_code_read", frozenset({"Edit", "Write", "MultiEdit"}), False),
        ("mcp__cc-context__ccx_code_grep", frozenset({"Edit", "Write", "MultiEdit"}), False),
    ],
    ids=[
        "exact",
        "pre-expanded-alias",
        "mcp-suffix",
        "mcp-too-few-parts",
        "no-alias-closure",
        "ccx-edit-aliases-edit",
        "ccx-replace-aliases-write",
        "ccx-read-not-edit",
        "ccx-grep-not-edit",
    ],
)
def test_matches_names(actual: str, names: frozenset[str], expected: bool) -> None:
    assert matches_names(actual, names) is expected


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("mcp__semble__search", ("semble", "search")),
        ("mcp__railway__deploy", ("railway", "deploy")),
        ("mcp__github__Grep", ("github", "Grep")),
        ("Bash", None),
        ("mcp__server", None),
    ],
    ids=["semble-search", "railway-deploy", "github-grep", "non-mcp", "too-few-parts"],
)
def test_mcp_parts(name: str, expected: tuple[str, str] | None) -> None:
    assert mcp_parts(name) == expected


@pytest.mark.parametrize(
    ("tool", "expected"),
    [
        ("search", "read"),
        ("deploy", "write"),
        ("get_balance", "read"),
        ("list_calls", "read"),
        ("create_persona", "write"),
        ("Search", "read"),
        ("ccx_read", "read"),
        ("ccx_find", "read"),
        ("ccx_grep", "write"),
    ],
    ids=[
        "search-read",
        "deploy-write",
        "get-prefix-read",
        "list-prefix-read",
        "create-write",
        "case-insensitive",
        "namespaced-read-token",
        "namespaced-find-token",
        "namespaced-non-verb-write",
    ],
)
def test_mcp_access(tool: str, expected: str) -> None:
    assert mcp_access(tool) == expected


def test_bash_result_parses_typed_fields() -> None:
    result = parse_tool_result(
        "Bash",
        {
            "stdout": "hi",
            "stderr": "err",
            "interrupted": False,
            "isImage": False,
            "noOutputExpected": True,
            "backgroundTaskId": "bg1",
            "returnCodeInterpretation": "exit 0",
        },
    )
    assert isinstance(result, BashResult)
    assert (
        result.name,
        result.stdout,
        result.stderr,
        result.interrupted,
        result.is_image,
        result.no_output_expected,
        result.background_task_id,
        result.return_code_interpretation,
    ) == ("Bash", "hi", "err", False, False, True, "bg1", "exit 0")
    assert result.raw["stdout"] == "hi"  # raw excluded from equality


def test_execute_alias_parses_to_bash_result() -> None:
    result = parse_tool_result("Execute", {"stdout": "x"})
    assert isinstance(result, BashResult) and result.name == "Execute" and result.stdout == "x"


def test_edit_result_keeps_structured_patch_and_original_file_raw() -> None:
    result = parse_tool_result(
        "Edit",
        {
            "filePath": "/a.py",
            "oldString": "x",
            "newString": "y",
            "replaceAll": True,
            "userModified": True,
            "staleRecovered": False,
            "structuredPatch": [{"lines": ["-x", "+y"]}],
            "originalFile": "x\n",
        },
    )
    assert isinstance(result, EditResult)
    assert (
        result.name,
        result.file_path,
        result.old_string,
        result.new_string,
        result.replace_all,
        result.user_modified,
        result.stale_recovered,
        result.structured_patch,
        result.original_file,
    ) == ("Edit", "/a.py", "x", "y", True, True, False, [{"lines": ["-x", "+y"]}], "x\n")


def test_write_result_fields() -> None:
    result = parse_tool_result(
        "Create",  # Write alias
        {"content": "body", "filePath": "/w.py", "originalFile": None, "structuredPatch": [], "userModified": False},
    )
    assert isinstance(result, WriteResult) and result.name == "Create"
    assert result.content == "body" and result.file_path == "/w.py" and result.structured_patch == []


def test_read_result_keeps_file_mapping_raw() -> None:
    result = parse_tool_result("Read", {"type": "text", "file": {"filePath": "/r.py", "numLines": 3}})
    assert isinstance(result, ReadResult)
    assert (result.name, result.file, result.type) == ("Read", {"filePath": "/r.py", "numLines": 3}, "text")


def test_task_result_terminal_shape() -> None:
    result = parse_tool_result(
        "Agent",
        {
            "agentId": "a1",
            "agentType": "Explore",
            "status": "completed",
            "totalDurationMs": 100,
            "totalTokens": 50,
            "totalToolUseCount": 2,
            "toolStats": {"Read": 1},
            "usage": {"input_tokens": 3},
            "content": [{"type": "text", "text": "done"}],
            "prompt": "go",
            "resolvedModel": "m",
        },
    )
    assert isinstance(result, TaskResult)
    assert result.agent_id == "a1" and result.total_duration_ms == 100 and result.total_tool_use_count == 2
    assert result.tool_stats == {"Read": 1} and result.content == [{"type": "text", "text": "done"}]


def test_task_launch_result_inflight_shape() -> None:
    result = parse_tool_result(
        "Task",  # Agent alias
        {
            "agentId": "a2",
            "outputFile": "/out",
            "isAsync": True,
            "canReadOutputFile": True,
            "description": "d",
            "prompt": "p",
            "status": "async_launched",
            "resolvedModel": "claude-opus-4-8",
        },
    )
    assert isinstance(result, TaskLaunchResult) and result.name == "Task"
    assert result.output_file == "/out" and result.is_async is True and result.can_read_output_file is True
    assert result.resolved_model == "claude-opus-4-8" and result.status == "async_launched"


def test_task_payload_matching_neither_shape_degrades_to_other_result() -> None:
    result = parse_tool_result(
        "Agent",
        {
            "agent_id": "team-1",
            "agent_type": "reviewer",
            "name": "rev",
            "status": "spawned",
            "team_name": "core",
            "tmux_pane_id": "%1",
        },
    )
    assert isinstance(result, OtherResult) and result.raw["agent_id"] == "team-1"


def test_skill_result_extracts_allowed_tools() -> None:
    result = parse_tool_result("Skill", {"commandName": "codex", "success": True, "allowedTools": ["Bash", "Read"]})
    assert isinstance(result, SkillResult)
    assert (result.name, result.command_name, result.success, result.allowed_tools) == (
        "Skill",
        "codex",
        True,
        ("Bash", "Read"),
    )


def test_ask_user_question_result_lifts_questions_answers_annotations() -> None:
    result = parse_tool_result(
        "AskUserQuestion",
        {
            "questions": [
                {
                    "question": "Which approach?",
                    "header": "Approach",
                    "options": [{"label": "A", "description": "first"}, {"label": "B", "description": "second"}],
                    "multiSelect": False,
                }
            ],
            "answers": {"Which approach?": "A"},
            "annotations": {"Which approach?": {"preview": "picked A", "notes": "with a caveat"}},
        },
    )
    assert isinstance(result, AskUserQuestionResult)
    assert result.answers == {"Which approach?": "A"}
    assert result.annotations == {"Which approach?": QuestionAnnotation(preview="picked A", notes="with a caveat")}
    assert result.questions == (
        Question(question="Which approach?", header="Approach", multi_select=False, labels=("A", "B")),
    )


def test_ask_user_question_result_drops_non_string_answer_values() -> None:
    result = parse_tool_result("AskUserQuestion", {"answers": {"Q?": 42, "R?": "kept"}})
    assert isinstance(result, AskUserQuestionResult)
    assert result.answers == {"R?": "kept"}


def test_ask_user_question_result_non_string_annotation_leaves_read_as_none() -> None:
    result = parse_tool_result(
        "AskUserQuestion",
        {"answers": {"Q?": "A"}, "annotations": {"Q?": {"notes": 3, "preview": ["not", "a", "string"]}}},
    )
    assert isinstance(result, AskUserQuestionResult)
    assert result.annotations == {"Q?": QuestionAnnotation(preview=None, notes=None)}


def test_string_payload_becomes_text_result() -> None:
    result = parse_tool_result("Bash", "The user doesn't want to proceed with this tool use.")
    assert isinstance(result, TextResult)
    assert (result.name, result.text) == ("Bash", "The user doesn't want to proceed with this tool use.")


def test_absent_payload_becomes_other_result() -> None:
    result = parse_tool_result("Bash", None)
    assert isinstance(result, OtherResult) and result.name == "Bash" and result.raw is None


def test_unknown_and_untyped_tools_become_other_result() -> None:
    assert isinstance(parse_tool_result("TodoWrite", {"todos": []}), OtherResult)
    assert isinstance(parse_tool_result("mcp__x__do", {"k": "v"}), OtherResult)


def test_missing_keys_fall_back_to_defaults() -> None:
    result = parse_tool_result("Bash", {})
    assert isinstance(result, BashResult)
    assert result.stdout is None and result.interrupted is False and result.is_image is False
    assert result.background_task_id is None and result.return_code_interpretation is None
