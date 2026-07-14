"""v14 read-only inputs: memoized dict view fields are immutable mappings.

ToolUseBlock.input and ToolResultBlock.tool_use_result used to hand back the cached
mutable dict, which let a caller diverge it from the Rust-backed derived properties
(split-brain) and construct an uncollectable cycle through the untracked view. They
are now a read-only ``dict`` subclass (``ReadOnlyDict``) — still a ``dict`` for every
serialize/canonicalize/isinstance path, but mutation raises.
"""

from __future__ import annotations

import gc
import json
import weakref
from collections.abc import Mapping

import orjson
import pytest

from cc_transcript.ids import tool_digest
from cc_transcript.models import ReadOnlyDict, ToolResultBlock, ToolUseBlock
from tests import testkit

EDIT_INPUT = {"file_path": "a.py", "old_string": "x", "new_string": "y", "nested": {"k": [1, 2]}}


def tool_use_block(name: str = "Edit", input: dict | None = None) -> ToolUseBlock:
    event = testkit.parse_event(
        testkit.assistant_line("a1", "", blocks=[testkit.tool_use("t1", name, input or EDIT_INPUT)])
    )
    return event.blocks[0]


def tool_result_block(payload: dict) -> ToolResultBlock:
    event = testkit.parse_event(
        testkit.user_line("u1", "", blocks=[testkit.tool_result("t1", "ok")], tool_use_result=payload)
    )
    return next(block for block in event.blocks if isinstance(block, ToolResultBlock))


def test_input_is_a_read_only_mapping() -> None:
    block = tool_use_block()
    assert isinstance(block.input, ReadOnlyDict)
    assert isinstance(block.input, Mapping)
    assert block.input == EDIT_INPUT
    with pytest.raises(TypeError):
        block.input["file_path"] = "b"
    with pytest.raises(TypeError):
        del block.input["file_path"]
    with pytest.raises(TypeError):
        block.input.update({"x": 1})


def test_read_only_input_still_serializes_and_digests() -> None:
    # The dict-subclass choice keeps every dict-consuming path working.
    block = tool_use_block()
    assert isinstance(block.input, dict)
    assert orjson.loads(orjson.dumps(block.input)) == EDIT_INPUT
    assert json.loads(json.dumps(block.input)) == EDIT_INPUT
    assert tool_digest("Edit", block.input) == block.digest


def test_tool_use_result_dict_is_read_only() -> None:
    block = tool_result_block({"stdout": "hi", "meta": {"n": 1}})
    assert isinstance(block.tool_use_result, Mapping)
    assert block.tool_use_result == {"stdout": "hi", "meta": {"n": 1}}
    with pytest.raises(TypeError):
        block.tool_use_result["stdout"] = "bye"


def test_read_only_input_has_no_split_brain() -> None:
    block = tool_use_block()
    with pytest.raises(TypeError):
        block.input["file_path"] = "b"
    # input, file_path, and the typed call all still agree
    assert block.input["file_path"] == "a.py"
    assert block.file_path == "a.py"
    assert block.call.file_path == "a.py"
    assert block.digest == block.call.digest


def test_read_only_input_cannot_form_a_reference_cycle() -> None:
    block = tool_use_block()
    sentinel = type("Sentinel", (), {})()
    ref = weakref.ref(sentinel)
    # The refuter's cycle needs to store a value in the input dict; the proxy forbids it.
    with pytest.raises(TypeError):
        block.input["owner"] = sentinel
    del sentinel
    gc.collect()
    assert ref() is None
