from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    CompactBoundary,
    EntryMeta,
    HookAdditionalContext,
    HookSuccess,
    ModeEvent,
    OtherEvent,
    QueuedCommand,
    StopHookSummary,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.parser import parse
from cc_transcript.tools import (
    BashCall,
    EditCall,
    ExitPlanModeCall,
    MultiEditCall,
    OtherCall,
    ReadCall,
    SpanEditCall,
    TaskCall,
    TextResult,
    WriteCall,
    parse_tool_call,
    parse_tool_result,
    register_mcp_tool,
    unregister_mcp_tool,
)

REPO_ROOT = Path(__file__).resolve().parent.parent
EDGE = REPO_ROOT / "tests" / "testdata" / "views_edge"


@pytest.fixture(scope="module")
def events() -> list:
    return list(parse(EDGE / "edge_core.jsonl").events) + list(parse(
        EDGE / "edge_tools.jsonl"
    ).events)


def test_keyword_match_dispatches_every_event_class(events: list) -> None:
    seen: set[str] = set()
    for event in events:
        match event:
            case UserEvent(meta=EntryMeta(session_id=sid), text=text, interrupted=interrupted):
                assert isinstance(sid, str) and isinstance(text, str) and isinstance(interrupted, bool)
                seen.add("user")
            case AssistantEvent(meta=meta, model=model, blocks=blocks):
                assert isinstance(model, str) and isinstance(blocks, tuple) and meta.uuid
                seen.add("assistant")
            case SystemEvent(subtype=subtype, detail=detail):
                assert isinstance(subtype, str) and detail is not None
                seen.add("system")
            case ModeEvent(channel=channel, value=value):
                assert channel in ("mode", "permission-mode") and isinstance(value, str)
                seen.add("mode")
            case OtherEvent(type=ty, raw=raw):
                assert isinstance(ty, str) and isinstance(raw, dict)
                seen.add("other")
            case AttachmentEvent(attachment_type=at, detail=detail):
                assert isinstance(at, str) and detail is not None
                seen.add("attachment")
    assert seen == {"user", "assistant", "system", "mode", "other", "attachment"}


def test_keyword_match_dispatches_blocks(events: list) -> None:
    seen: set[str] = set()
    for event in events:
        match event:
            case UserEvent(blocks=blocks) | AssistantEvent(blocks=blocks):
                for block in blocks:
                    match block:
                        case ToolUseBlock(id=bid, name=name, input=inp):
                            assert bid and name and isinstance(inp, (dict, list))
                            seen.add("tool_use")
                        case ToolResultBlock(tool_use_id=tid, is_error=is_error):
                            assert tid and isinstance(is_error, bool)
                            seen.add("tool_result")
                        case TextBlock(text=text):
                            assert isinstance(text, str)
                            seen.add("text")
                        case ThinkingBlock(thinking=thinking):
                            assert isinstance(thinking, str)
                            seen.add("thinking")
            case _:
                pass
    assert {"tool_use", "tool_result", "text", "thinking"} <= seen


def test_keyword_match_dispatches_details(events: list) -> None:
    seen: set[str] = set()
    for event in events:
        match event:
            case SystemEvent(detail=StopHookSummary(hook_count=hook_count)):
                assert hook_count is None or isinstance(hook_count, int)
                seen.add("stop_hook")
            case SystemEvent(detail=CompactBoundary(trigger=trigger)):
                assert trigger is None or isinstance(trigger, str)
                seen.add("compact")
            case AttachmentEvent(detail=HookSuccess(hook_name=hook_name)):
                assert hook_name is None or isinstance(hook_name, str)
                seen.add("hook_success")
            case AttachmentEvent(detail=HookAdditionalContext(content=content)):
                assert isinstance(content, tuple)
                seen.add("hook_context")
            case AttachmentEvent(detail=QueuedCommand(prompt=prompt)):
                assert prompt is None or isinstance(prompt, str)
                seen.add("queued")
            case _:
                pass
    assert seen == {"stop_hook", "compact", "hook_success", "hook_context", "queued"}


def test_keyword_match_dispatches_tool_calls() -> None:
    calls = [
        parse_tool_call("Bash", {"command": "make test"}),
        parse_tool_call("Edit", {"file_path": "/a.py", "old_string": "x", "new_string": "y"}),
        parse_tool_call("MultiEdit", {"file_path": "/m.py", "edits": [{"old_string": "a", "new_string": "b"}]}),
        parse_tool_call("Create", {"file_path": "/w.py", "content": "pass"}),
        parse_tool_call("Read", {"file_path": "/r.py"}),
        parse_tool_call("Task", {"prompt": "explore"}),
        parse_tool_call("ExitSpecMode", {"plan": "the plan"}),
        parse_tool_call("mcp__semble__search", {"query": "q"}),
    ]
    seen: list[str] = []
    for call in calls:
        match call:
            case EditCall(new=new, file_path=file_path):
                assert new == "y" and file_path == "/a.py"
                seen.append("edit")
            case MultiEditCall(edits=edits):
                assert edits[0].new == "b"
                seen.append("multiedit")
            case BashCall(command=command):
                assert command == "make test"
                seen.append("bash")
            case WriteCall(content=content):
                assert content == "pass"
                seen.append("write")
            case ReadCall(file_path=file_path):
                assert file_path == "/r.py"
                seen.append("read")
            case TaskCall(prompt=prompt):
                assert prompt == "explore"
                seen.append("task")
            case ExitPlanModeCall(plan=plan):
                assert plan == "the plan"
                seen.append("exitplan")
            case OtherCall(name=name):
                assert name == "mcp__semble__search"
                seen.append("other")
    assert seen == ["bash", "edit", "multiedit", "write", "read", "task", "exitplan", "other"]


def test_keyword_match_dispatches_tool_results() -> None:
    match parse_tool_result("Bash", "denied by user"):
        case TextResult(text=text):
            assert text == "denied by user"
        case other:
            pytest.fail(f"expected TextResult, got {other!r}")


def test_positional_match_works_on_models_classes(events: list) -> None:
    # models.py dataclasses were positional; __match_args__ preserves that shape.
    match events[0]:
        case UserEvent(meta, text):
            assert meta.uuid and isinstance(text, str)
        case _:
            pytest.fail("positional UserEvent pattern did not match")


def test_positional_match_rejected_on_kw_only_tools() -> None:
    # tools.py dataclasses are kw_only: no positional sub-patterns, like the originals.
    with pytest.raises(TypeError):
        match parse_tool_call("Bash", {"command": "x"}):
            case BashCall(_):
                pass
    # The registry-backed SpanEditCall view joins the same kw_only contract.
    register_mcp_tool("syn_span_edit", "Edit", {"path": "path", "content": "content", "delete": "delete"})
    try:
        span = parse_tool_call("mcp__cc-context__syn_span_edit", {"path": "/a.py", "content": "body"})
        assert isinstance(span, SpanEditCall)
        with pytest.raises(TypeError):
            match span:
                case SpanEditCall(_):
                    pass
    finally:
        unregister_mcp_tool("syn_span_edit")
