from __future__ import annotations

import pytest

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
    ToolInputError,
    WriteCall,
    expand_tool_names,
    file_path_of,
    hunks_of,
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


def test_task_normalizes_subagent_type() -> None:
    call = parse_tool_call("Agent", {"prompt": "p", "subagent_type": "Explore", "name": "scout"})
    assert isinstance(call, TaskCall)
    assert (call.agent_type, call.agent_name) == ("Explore", "scout")


def test_grep_maps_type_to_file_type() -> None:
    call = parse_tool_call("Grep", {"pattern": "x", "type": "py"})
    assert isinstance(call, GrepCall) and call.file_type == "py"


def test_unknown_and_mcp_tools_parse_to_other() -> None:
    call = parse_tool_call("mcp__github__search", {"q": "x"})
    assert isinstance(call, OtherCall) and call.raw.get("q") == "x"


def test_malformed_known_tool_raises_by_default() -> None:
    with pytest.raises(ToolInputError, match="Edit input missing"):
        parse_tool_call("Edit", {"file_path": "a.py"})


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
        ("mcp__github__Grep", "Bash", False),
        ("Read", "Bash|Grep", False),
    ],
    ids=["alias-forward", "alias-reverse", "mcp-suffix", "mcp-miss", "plain-miss"],
)
def test_tool_name_matches(actual: str, spec: str, expected: bool) -> None:
    assert tool_name_matches(actual, spec) is expected
