from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from cc_transcript.query import PredicateInputs, Session
from cc_transcript.snapshots import CancellationToken, TranscriptStore, decode_projection
from cc_transcript.tools import SpanEditCall, register_mcp_tool, unregister_mcp_tool
from tests.test_snapshots import CLASSIFIER, LIMITS, context, deadline, event, request

TOOL = "syn_pinned_query"
SPECS = [{"name": TOOL, "behaves_like": "Edit", "span_edit": {"path": "path", "content": "body", "delete": None}}]


def source(path: Path) -> None:
    path.write_bytes(
        event(0, "OVERRIDE")
        + json.dumps(
            {
                "type": "assistant",
                "uuid": "a",
                "sessionId": "s",
                "timestamp": "2026-01-02T03:04:06Z",
                "message": {
                    "role": "assistant",
                    "model": "claude",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "t",
                            "name": f"mcp__fixture__{TOOL}",
                            "input": {"path": "a.py", "body": "kept"},
                        }
                    ],
                },
            }
        ).encode()
        + b"\n"
        + event(2, "after")
    )


def acquire_with_context(store: TranscriptStore, path: Path, call_context: Any) -> dict[str, Any]:
    cancel = CancellationToken()
    reply = store.request(
        request("acquire", path=str(path), classifier=CLASSIFIER, limits=LIMITS, deadline_unix_ms=deadline()),
        context=call_context,
        cancellation=cancel,
    )
    for _ in range(100):
        if reply["status"] != "incomplete":
            assert reply["status"] == "ok", reply
            return dict(reply["data"]["description"])
        assert reply["cursor"], reply
        reply = store.request(request("resume", cursor=reply["cursor"]), context=call_context, cancellation=cancel)
    raise AssertionError("fixture source did not finish its bounded preparation")


def test_lazy_events_and_call_queries_use_the_pinned_registry(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    source(path)
    store = TranscriptStore({})
    bound = context()
    bound["registry_generation"] = store.register_tool_registry(SPECS, context=bound)
    description = acquire_with_context(store, path, bound)
    with store.borrow_snapshot(
        description["handle"],
        context=bound,
        cancellation=CancellationToken(),
        limits=LIMITS,
        deadline_unix_ms=deadline(),
    ) as snapshot:
        saved_event = snapshot.events[1]
        activity = snapshot.activity(CLASSIFIER)
    register_mcp_tool(TOOL, "Read")
    try:
        call = saved_event.blocks[0].call
        assert isinstance(call, SpanEditCall)
        assert call.new == "kept"
        assert call.matches("Edit")
        assert not call.matches("Read")
        session = Session.from_activity(activity)
        assert session.tool_calls.named("Edit").count() == 1
        assert session.tool_calls.named("Read").count() == 0
        assert session.before(tool="Edit").tool_calls.count() == 0
        assert session.after(tool="Edit").user_text == "after"
        assert not session.has_override("OVERRIDE")
        inputs = PredicateInputs.of(session)
        assert inputs.has_tool("Edit")
        assert not inputs.has_tool("Read")
        assert [file.path for file in inputs.files("Edit")] == ["a.py"]
        assert session.has_tool("Edit", subagents=False)
        assert not session.has_tool("Read", subagents=False)
    finally:
        unregister_mcp_tool(TOOL)


def test_predicate_projection_decoder_captures_explicit_registry(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    source(path)
    store = TranscriptStore({})
    bound = context()
    bound["registry_generation"] = store.register_tool_registry(SPECS, context=bound)
    description = acquire_with_context(store, path, bound)
    reply = store.request(
        request(
            "query",
            view={"handle": description["handle"], "classifier": CLASSIFIER, "selectors": [], "attachments": []},
            query={"kind": "deep_predicate_inputs", "order": "forward"},
            limits=LIMITS,
            deadline_unix_ms=deadline(),
        ),
        context=bound,
        cancellation=CancellationToken(),
    )
    assert reply["status"] == "ok", reply
    records = decode_projection(reply["data"]["record_schema"], reply["data"]["records_json"], tool_registry=SPECS)
    register_mcp_tool(TOOL, "Read")
    try:
        assert any(inputs.has_tool("Edit") for inputs in records)
        assert not any(inputs.has_tool("Read") for inputs in records)
    finally:
        unregister_mcp_tool(TOOL)
