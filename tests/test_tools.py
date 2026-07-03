from __future__ import annotations

import pytest

from cc_transcript.command import CommandLine
from cc_transcript.ids import tool_digest
from cc_transcript.tools import (
    BashCall,
    EditCall,
    EditSpan,
    GrepCall,
    Hunk,
    MultiEditCall,
    NotebookEditCall,
    OtherCall,
    TaskCall,
    TaskUpdateCall,
    ToolInputError,
    WorkflowCall,
    WriteCall,
    expand_tool_names,
    file_path_of,
    hunks_of,
    matches_names,
    mcp_access,
    mcp_parts,
    parse_tool_call,
    tool_name_matches,
)


def test_edit_parses_typed_fields() -> None:
    call = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
    assert call == EditCall(
        name="Edit", raw={}, file_path="a.py", old="x", new="y", replace_all=False
    )  # raw excluded from equality
    assert isinstance(call, EditCall) and call.raw["old_string"] == "x"


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


def test_task_update_without_task_id_raises_by_default() -> None:
    with pytest.raises(ToolInputError, match="TaskUpdate input missing"):
        parse_tool_call("TaskUpdate", {"status": "completed"})


def test_task_update_without_task_id_degrades_under_other() -> None:
    call = parse_tool_call("TaskUpdate", {"status": "completed"}, on_error="other")
    assert isinstance(call, OtherCall)
    assert call.raw == {"status": "completed"}


def test_grep_maps_type_to_file_type() -> None:
    call = parse_tool_call("Grep", {"pattern": "x", "type": "py"})
    assert isinstance(call, GrepCall) and call.file_type == "py"


def test_unknown_and_mcp_tools_parse_to_other() -> None:
    call = parse_tool_call("mcp__github__search", {"q": "x"})
    assert isinstance(call, OtherCall) and call.raw.get("q") == "x"


def test_malformed_known_tool_raises_by_default() -> None:
    with pytest.raises(ToolInputError, match="Edit input missing"):
        parse_tool_call("Edit", {"file_path": "a.py"})


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
    assert expand_tool_names("Bash|Write") == frozenset({"Bash", "Execute", "Write", "Create"})


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
    ],
    ids=[
        "alias-forward",
        "alias-reverse",
        "mcp-suffix",
        "mcp-suffix-server-tool",
        "mcp-miss",
        "mcp-too-few-parts",
        "plain-miss",
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
    ],
    ids=["exact", "pre-expanded-alias", "mcp-suffix", "mcp-too-few-parts", "no-alias-closure"],
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
