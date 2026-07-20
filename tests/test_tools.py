from __future__ import annotations

from collections.abc import Iterator
from datetime import UTC, datetime
from typing import Any

import pytest

from cc_transcript.command import CommandLine
from cc_transcript.ids import tool_digest
from cc_transcript.models import Question
from cc_transcript.tools import (
    ApplyPatchCall,
    AskUserQuestionResult,
    BashCall,
    BashResult,
    CodeModeCall,
    EditCall,
    EditResult,
    EditSpan,
    FallbackCall,
    FallbackResult,
    GrepCall,
    Hunk,
    MultiEditCall,
    NotebookEditCall,
    OtherCall,
    OtherResult,
    PatchEdit,
    QuestionAnnotation,
    ReadResult,
    SkillResult,
    SpanEditCall,
    TaskCall,
    TaskLaunchResult,
    TaskResult,
    TaskUpdateCall,
    TextResult,
    ToolInputError,
    UpdatePlanCall,
    WorkflowCall,
    WriteCall,
    WriteResult,
    WriteStdinCall,
    edits_of,
    expand_tool_names,
    file_path_of,
    file_paths_of,
    hunks_of,
    matches_names,
    mcp_access,
    mcp_parts,
    parse_tool_call,
    parse_tool_result,
    register_mcp_tool,
    tool_name_matches,
    unregister_mcp_tool,
)

SYN_SPAN_EDIT = "syn_span_edit"
SYN_GATE_WRITE = "syn_gate_write"


@pytest.fixture
def registered_syn_specs() -> Iterator[None]:
    """Registers a span-edit and a behaves-like-only MCP spec, then cleans up."""
    register_mcp_tool(SYN_SPAN_EDIT, "Edit", {"path": "path", "content": "content", "delete": "delete"})
    register_mcp_tool(SYN_GATE_WRITE, "Write")
    try:
        yield
    finally:
        unregister_mcp_tool(SYN_SPAN_EDIT)
        unregister_mcp_tool(SYN_GATE_WRITE)


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
def test_non_mapping_input_degrades_to_other_preserving_raw(name: str) -> None:
    call = parse_tool_call(name, ["not", "a", "mapping"], on_error="other")  # type: ignore[arg-type]
    assert isinstance(call, OtherCall)
    assert (call.name, list(call.raw)) == (name, ["not", "a", "mapping"])


def test_non_mapping_degrade_digest_reflects_the_preserved_raw() -> None:
    call = parse_tool_call("Bash", None, on_error="other")  # type: ignore[arg-type]
    assert call.raw is None
    assert call.digest == tool_digest("Bash", None)


def test_on_error_other_degrades_with_correct_digest() -> None:
    raw = {"file_path": "a.py", "unexpected_shape": True}
    call = parse_tool_call("Edit", raw, on_error="other")
    assert isinstance(call, OtherCall)
    assert call.digest == tool_digest("Edit", raw)


def test_unknown_tool_degrades_with_no_error() -> None:
    call = parse_tool_call("mcp__github__search", {"q": "x"})
    assert isinstance(call, OtherCall)
    assert call.error is None


@pytest.mark.parametrize(
    ("name", "raw", "field_name"),
    [
        pytest.param("Edit", {"file_path": "a.py", "old_string": "x"}, "new_string", id="edit-missing-new-string"),
        pytest.param("Bash", {"description": "hi"}, "command", id="bash-missing-command"),
    ],
)
def test_malformed_known_tool_records_strict_parse_failure(name: str, raw: dict[str, object], field_name: str) -> None:
    call = parse_tool_call(name, raw, on_error="other")
    assert isinstance(call, OtherCall)
    assert call.error is not None and field_name in call.error


def test_non_mapping_known_tool_degrade_records_error() -> None:
    call = parse_tool_call("Bash", ["not", "a", "mapping"], on_error="other")  # type: ignore[arg-type]
    assert isinstance(call, OtherCall)
    assert call.error == "input must be a mapping, got list"


def test_non_mapping_unknown_tool_degrade_records_error() -> None:
    call = parse_tool_call("mcp__github__search", ["not", "a", "mapping"], on_error="other")  # type: ignore[arg-type]
    assert isinstance(call, OtherCall)
    assert call.error == "input must be a mapping, got list"


def test_non_mapping_unserializable_degrades_to_fallback() -> None:
    call = parse_tool_call("Bash", [float("nan")], on_error="other")  # type: ignore[arg-type]
    assert isinstance(call, FallbackCall)
    assert call.error is not None


def test_fallback_call_records_serialization_failure() -> None:
    call = parse_tool_call("X", {"t": datetime(2026, 1, 1, tzinfo=UTC)}, on_error="other")
    assert isinstance(call, FallbackCall)
    assert call.error is not None


def test_fallback_result_records_serialization_failure() -> None:
    result = parse_tool_result("Bash", {"when": datetime(2026, 1, 1, tzinfo=UTC)}, on_error="other")
    assert isinstance(result, FallbackResult)
    assert result.error is not None


def test_other_call_error_excluded_from_repr_and_equality() -> None:
    missing = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x"}, on_error="other")
    wrong_type = parse_tool_call("Edit", {"file_path": 42}, on_error="other")
    assert isinstance(missing, OtherCall) and isinstance(wrong_type, OtherCall)
    assert missing.error != wrong_type.error
    assert "error" not in repr(missing)
    assert repr(missing) == repr(wrong_type)
    assert missing == wrong_type


def test_fallback_call_error_excluded_from_repr_and_equality() -> None:
    fallback = FallbackCall(name="X", raw={"k": "v"}, error="boom")
    assert "error" not in repr(fallback) and "boom" not in repr(fallback)
    assert fallback == FallbackCall(name="X", raw={"k": "v"}, error="different")
    assert fallback == FallbackCall(name="X", raw={"k": "v"})


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


@pytest.mark.usefixtures("registered_syn_specs")
def test_expand_tool_names_includes_registered_and_alias_spellings() -> None:
    assert expand_tool_names("Bash|Write") == frozenset(
        {"Bash", "Execute", "exec_command", "Write", "Create", SYN_GATE_WRITE}
    )
    assert expand_tool_names("Execute") == frozenset({"Bash", "Execute", "exec_command"})
    assert expand_tool_names("Edit|Write") == frozenset(
        {"Edit", "apply_patch", "Write", "Create", SYN_SPAN_EDIT, SYN_GATE_WRITE}
    )
    assert expand_tool_names("Grep") == frozenset({"Grep"})


def test_expand_tool_names_omits_unregistered_names() -> None:
    assert expand_tool_names("Edit|Write") == frozenset({"Edit", "apply_patch", "Write", "Create"})


def test_apply_patch_forward_aliases_to_edit() -> None:
    assert "apply_patch" in expand_tool_names("Edit")
    assert tool_name_matches("apply_patch", "Edit|Write")
    assert tool_name_matches("apply_patch", "Edit")
    # forward-only: a spec written as apply_patch does not match a canonical Edit call
    assert not tool_name_matches("Edit", "apply_patch")


@pytest.mark.parametrize(
    ("actual", "spec", "expected"),
    [
        ("Execute", "Bash", True),
        ("Bash", "Execute", True),
        ("exec_command", "Execute", True),
        ("mcp__github__Grep", "Grep", True),
        ("mcp__semble__search", "search", True),
        ("mcp__github__Grep", "Bash", False),
        ("mcp__server", "server", False),
        ("Read", "Bash|Grep", False),
        ("mcp__cc-context__syn_span_edit", "Edit|Write|MultiEdit", True),
        ("mcp__cc-context__syn_gate_write", "Write", True),
        ("mcp__cc-context__syn_unregistered", "Edit|Write|MultiEdit", False),
    ],
    ids=[
        "alias-forward",
        "alias-reverse",
        "alias-sibling",
        "mcp-suffix",
        "mcp-suffix-server-tool",
        "mcp-miss",
        "mcp-too-few-parts",
        "plain-miss",
        "registered-span-edit-aliases-edit-gate",
        "registered-write-aliases-write",
        "unregistered-not-edit-gate",
    ],
)
@pytest.mark.usefixtures("registered_syn_specs")
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
        ("mcp__cc-context__syn_span_edit", frozenset({"Edit"}), True),
        ("mcp__cc-context__syn_gate_write", frozenset({"Write"}), True),
        ("mcp__cc-context__syn_span_edit", frozenset({"Write", "MultiEdit"}), False),
        ("mcp__cc-context__syn_unregistered", frozenset({"Edit", "Write", "MultiEdit"}), False),
    ],
    ids=[
        "exact",
        "pre-expanded-alias",
        "mcp-suffix",
        "mcp-too-few-parts",
        "no-alias-closure",
        "registered-span-edit-aliases-edit",
        "registered-write-aliases-write",
        "registered-edit-not-in-write-set",
        "unregistered-not-edit",
    ],
)
@pytest.mark.usefixtures("registered_syn_specs")
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


@pytest.mark.usefixtures("registered_syn_specs")
def test_unregistered_mcp_name_neither_matches_nor_lowers() -> None:
    call = parse_tool_call("mcp__cc-context__syn_unregistered", {"path": "/a", "content": "x"})
    assert isinstance(call, OtherCall)
    assert matches_names("mcp__cc-context__syn_unregistered", frozenset({"Edit", "Write"})) is False


@pytest.mark.usefixtures("registered_syn_specs")
def test_behaves_like_only_spec_gates_but_lowers_to_other() -> None:
    call = parse_tool_call("mcp__cc-context__syn_gate_write", {"content": "x"})
    assert isinstance(call, OtherCall)
    assert matches_names("mcp__cc-context__syn_gate_write", frozenset({"Write"})) is True


@pytest.mark.usefixtures("registered_syn_specs")
def test_span_edit_lowers_path_and_content_to_span_edit_call() -> None:
    call = parse_tool_call("mcp__cc-context__syn_span_edit", {"path": "/a.py", "content": "body"})
    assert isinstance(call, SpanEditCall)
    assert (call.name, call.file_path, call.new) == ("mcp__cc-context__syn_span_edit", "/a.py", "body")
    assert hunks_of(call) == ()
    assert file_path_of(call) == "/a.py"


@pytest.mark.usefixtures("registered_syn_specs")
def test_span_edit_delete_key_truthy_yields_new_none() -> None:
    call = parse_tool_call("mcp__cc-context__syn_span_edit", {"path": "/a.py", "delete": True})
    assert isinstance(call, SpanEditCall)
    assert call.new is None


@pytest.mark.usefixtures("registered_syn_specs")
def test_span_edit_supports_keyword_pattern_matching() -> None:
    call = parse_tool_call("mcp__cc-context__syn_span_edit", {"path": "/a.py", "content": "body"})
    match call:
        case SpanEditCall(file_path=path, new=new):
            assert (path, new) == ("/a.py", "body")
        case _:  # pragma: no cover
            pytest.fail("expected a SpanEditCall pattern match")


@pytest.mark.usefixtures("registered_syn_specs")
@pytest.mark.parametrize(
    "payload",
    [{"content": "body"}, {"path": "/a.py"}],
    ids=["missing-path", "missing-content"],
)
def test_span_edit_missing_required_key_raises_and_degrades(payload: dict[str, object]) -> None:
    with pytest.raises(ToolInputError, match="input missing or malformed"):
        parse_tool_call("mcp__cc-context__syn_span_edit", payload, on_error="raise")
    assert isinstance(parse_tool_call("mcp__cc-context__syn_span_edit", payload, on_error="other"), OtherCall)


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


# v14 accepted divergence: input routes through JSON, so JSON-expressible-but-non-JSON
# Python values normalize per JSON semantics rather than round-tripping identically.
@pytest.mark.parametrize(
    ("input", "expected_raw"),
    [
        pytest.param({"x": (1, 2)}, {"x": [1, 2]}, id="tuple-to-list"),
        pytest.param({1: "x"}, {"1": "x"}, id="int-key-to-str"),
    ],
)
def test_parse_tool_call_normalizes_json_expressible_values(input: dict, expected_raw: dict) -> None:
    assert parse_tool_call("Unknown", input).raw == expected_raw


@pytest.mark.parametrize(
    "input",
    [
        pytest.param({"t": datetime(2026, 1, 1, tzinfo=UTC)}, id="datetime"),
        pytest.param({"b": b"raw"}, id="bytes"),
    ],
)
def test_parse_tool_call_out_of_contract_input(input: dict) -> None:
    # strict mode surfaces the serialization failure; on_error='other' never raises
    # and preserves the original mapping verbatim (identity), mirroring the old fallback.
    with pytest.raises(TypeError):
        parse_tool_call("X", input)
    call = parse_tool_call("X", input, on_error="other")
    assert isinstance(call, FallbackCall)
    assert call.name == "X"
    assert call.raw is input


def test_parse_tool_call_reference_cycle_falls_back() -> None:
    cyclic: dict[str, object] = {}
    cyclic["self"] = cyclic
    with pytest.raises(ValueError):
        parse_tool_call("X", cyclic)
    call = parse_tool_call("X", cyclic, on_error="other")
    assert isinstance(call, FallbackCall)
    assert call.raw is cyclic


def test_parse_tool_result_out_of_contract_payload() -> None:
    payload = {"when": datetime(2026, 1, 1, tzinfo=UTC)}
    with pytest.raises(TypeError):
        parse_tool_result("Bash", payload)
    result = parse_tool_result("Bash", payload, on_error="other")
    assert isinstance(result, FallbackResult)
    assert result.name == "Bash"
    assert result.raw is payload


@pytest.mark.parametrize("value", [float("nan"), float("inf"), float("-inf")])
def test_parse_tool_call_json_serializable_but_unparseable_falls_back(value: float) -> None:
    # json.dumps emits NaN/Infinity, which the native JSON parser rejects — on_error='other'
    # must catch that leg, not only the serialization TypeError.
    payload = {"v": value}
    with pytest.raises(ValueError):
        parse_tool_call("X", payload)
    call = parse_tool_call("X", payload, on_error="other")
    assert isinstance(call, FallbackCall)
    assert call.raw is payload


def test_parse_tool_result_json_serializable_but_unparseable_falls_back() -> None:
    payload = {"v": float("nan")}
    with pytest.raises(ValueError):
        parse_tool_result("Bash", payload)
    result = parse_tool_result("Bash", payload, on_error="other")
    assert isinstance(result, FallbackResult)
    assert result.raw is payload


PATCH_ENVELOPE = (
    "*** Begin Patch\n"
    "*** Update File: src/a.py\n"
    "*** Move to: src/b.py\n"
    "@@ def f():\n"
    " ctx\n"
    "-old\n"
    "+new\n"
    "*** Add File: src/c.py\n"
    "+line1\n"
    "+line2\n"
    "*** Delete File: src/d.py\n"
    "*** End Patch\n"
)


def test_exec_command_string_parses_to_bash_shape() -> None:
    call = parse_tool_call("exec_command", '{"cmd": "ls /tmp", "workdir": "/tmp"}')
    native = parse_tool_call("Bash", {"command": "ls /tmp"})
    assert isinstance(call, BashCall) and isinstance(native, BashCall)
    assert (call.name, call.command) == ("exec_command", "ls /tmp")
    assert call.command == native.command
    assert isinstance(call.command_line, CommandLine)
    assert call.command_line.prefixes == ("ls",)
    assert call.raw == '{"cmd": "ls /tmp", "workdir": "/tmp"}'


def test_exec_string_parses_to_code_mode_verbatim() -> None:
    source = "python3 -c 'print(1)'"
    call = parse_tool_call("exec", source)
    assert isinstance(call, CodeModeCall)
    assert call.source == source
    assert call.raw == source


def test_apply_patch_multi_file_envelope_lists_every_edit() -> None:
    call = parse_tool_call("apply_patch", PATCH_ENVELOPE)
    assert isinstance(call, ApplyPatchCall)
    assert len(call.edits) == 3
    a, c, d = call.edits
    assert isinstance(a, PatchEdit)
    assert (a.file_path, a.kind, a.move_path) == ("src/a.py", "update", "src/b.py")
    assert a.hunks == (Hunk("ctx\nold", "ctx\nnew"),)
    assert (c.file_path, c.kind) == ("src/c.py", "add")
    assert c.hunks == (Hunk("", "line1\nline2"),)
    assert (d.file_path, d.kind, d.hunks) == ("src/d.py", "delete", ())
    assert call.raw == PATCH_ENVELOPE


def test_apply_patch_malformed_envelope_yields_no_edits_preserving_raw() -> None:
    call = parse_tool_call("apply_patch", "not a patch at all")
    assert isinstance(call, ApplyPatchCall)
    assert call.edits == ()
    assert call.raw == "not a patch at all"


@pytest.mark.parametrize(
    "envelope",
    [
        "*** Begin Patch\n*** Update File: src/a.py\n@@\n-old\n+new\n*** Bogus Directive\n*** End Patch\n",
        "*** Begin Patch\n*** Update File: src/a.py\n@@\n-old\n+new\n",
    ],
    ids=["bogus-directive", "truncated"],
)
def test_apply_patch_partial_malformed_envelope_discards_every_edit(envelope: str) -> None:
    call = parse_tool_call("apply_patch", envelope)
    assert isinstance(call, ApplyPatchCall)
    assert call.edits == ()
    assert call.raw == envelope


def test_apply_patch_file_paths_and_edits_cover_every_file() -> None:
    call = parse_tool_call("apply_patch", PATCH_ENVELOPE)
    assert file_paths_of(call) == ("src/a.py", "src/c.py", "src/d.py")
    projected = edits_of(call)
    assert [path for path, _ in projected] == ["src/a.py", "src/c.py", "src/d.py"]
    assert projected[2] == ("src/d.py", ())
    assert file_path_of(call) is None
    assert hunks_of(call) == ()


def test_plural_edit_projections_accept_fallback_calls() -> None:
    call = FallbackCall(name="apply_patch", raw=PATCH_ENVELOPE, error="unserializable")
    assert file_paths_of(call) == ()
    assert edits_of(call) == ()


def test_update_plan_and_write_stdin_decode_arguments() -> None:
    plan = parse_tool_call(
        "update_plan", '{"plan": [{"step": "a", "status": "pending"}], "explanation": "why"}'
    )
    assert isinstance(plan, UpdatePlanCall)
    assert plan.plan == [{"step": "a", "status": "pending"}]
    assert plan.explanation == "why"
    stdin = parse_tool_call(
        "write_stdin",
        '{"chars": "y\\n", "session_id": 7, "yield_time_ms": 1000, "max_output_tokens": 2000}',
    )
    assert isinstance(stdin, WriteStdinCall)
    assert (stdin.chars, stdin.session_id, stdin.yield_time_ms, stdin.max_output_tokens) == ("y\n", 7, 1000, 2000)


def test_update_plan_rejects_non_list_plan() -> None:
    with pytest.raises(ToolInputError, match="plan must be a list"):
        parse_tool_call("update_plan", '{"plan": {}}')


@pytest.mark.parametrize("name", ["apply_patch", "exec"])
@pytest.mark.parametrize("input", [[], {}, None, 7], ids=["list", "mapping", "null", "number"])
def test_string_typed_codex_calls_reject_non_string_input(name: str, input: Any) -> None:
    with pytest.raises(ToolInputError, match="must be a mapping"):
        parse_tool_call(name, input)


def test_exec_command_list_input_raises_input_type_error() -> None:
    input: Any = []
    with pytest.raises(ToolInputError, match="must be a mapping"):
        parse_tool_call("exec_command", input)


def test_untyped_codex_string_call_degrades_to_other_without_error() -> None:
    call = parse_tool_call("send_message", '{"message": "hi"}')
    assert isinstance(call, OtherCall)
    assert call.error is None
    assert call.raw == '{"message": "hi"}'


def test_non_codex_name_with_string_input_raises_non_mapping() -> None:
    with pytest.raises(ToolInputError, match="must be a mapping"):
        parse_tool_call("Read", "/etc/hosts")
