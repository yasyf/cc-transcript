from __future__ import annotations

from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.snapshots import CancellationToken, TranscriptStore, decode_projection
from tests.test_snapshots import CLASSIFIER, LIMITS, acquire, context, deadline, event, request


def assistant(uuid: str, blocks: list[dict[str, Any]]) -> bytes:
    return orjson.dumps({
        "type": "assistant", "uuid": uuid, "sessionId": "s",
        "timestamp": "2026-01-02T03:04:06Z",
        "message": {"model": "m", "content": blocks},
    }) + b"\n"


def tool(name: str, input: dict[str, Any]) -> dict[str, Any]:
    return {"type": "tool_use", "id": name, "name": name, "input": input}


def query(
    store: TranscriptStore,
    description: dict[str, Any],
    operation: dict[str, Any],
    *,
    selectors: list[dict[str, Any]] | None = None,
    attachments: list[str] | None = None,
    limits: dict[str, int] | None = None,
) -> dict[str, Any]:
    return dict(store.request(
        request(
            "query",
            view={
                "handle": description["handle"], "classifier": CLASSIFIER,
                "selectors": selectors or [], "attachments": attachments or [],
            },
            query=operation,
            limits=LIMITS if limits is None else limits,
            deadline_unix_ms=deadline(),
        ),
        context=context(),
        cancellation=CancellationToken(),
    ))


def test_local_predicate_inputs_use_selected_cached_calls_without_opening_attachments(tmp_path: Path) -> None:
    path = tmp_path / "root.jsonl"
    path.write_bytes(
        event(0, "first")
        + assistant("a", [
            tool("Read", {"file_path": "a.py", "unused_payload": "x" * 64_000}),
            tool("Bash", {"command": "echo local"}),
            tool("Skill", {"skill": "review"}),
        ])
        + event(2, "next")
        + assistant("b", [tool("Edit", {"file_path": "b.py", "old_string": "old", "new_string": "new"})])
    )
    store = TranscriptStore({})
    description = acquire(store, path)
    reply = query(
        store, description, {"kind": "predicate_inputs", "order": "forward"},
        selectors=[{"kind": "event_range", "start": 0, "stop": 2}],
        attachments=[str(tmp_path / "must-not-open.jsonl")],
        limits={**LIMITS, "max_output_bytes": 4096},
    )
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert reply["usage"]["source_opens"] == 0
    assert reply["data"]["record_schema"] == "cc-transcript.predicate-inputs/1"
    assert len(reply["data"]["records_json"]) == 1
    raw = orjson.loads(reply["data"]["records_json"][0])
    assert raw == {
        "calls": [["Read", ["a.py"]], ["Bash", []], ["Skill", []]],
        "commands": ["echo local"], "edited_files": [], "skills": ["review"],
    }
    [inputs] = decode_projection(reply["data"]["record_schema"], reply["data"]["records_json"])
    assert inputs.has_tool("Read")
    assert not inputs.has_tool("Edit")
    assert [file.path for file in inputs.files("Read")] == ["a.py"]
    current = query(
        store, description, {"kind": "predicate_inputs", "order": "forward"},
        selectors=[{"kind": "current_turn"}],
    )
    assert current["status"] == "ok", current
    current_raw = orjson.loads(current["data"]["records_json"][0])
    assert current_raw["calls"] == [["Edit", ["b.py"]]]
    assert current_raw["edited_files"] == [{"path": "b.py"}]


def test_empty_local_predicate_inputs_are_one_complete_record(tmp_path: Path) -> None:
    path = tmp_path / "empty-tools.jsonl"
    path.write_bytes(event(0))
    store = TranscriptStore({})
    description = acquire(store, path)
    reply = query(store, description, {"kind": "predicate_inputs", "order": "forward"})
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert len(reply["data"]["records_json"]) == 1
    assert orjson.loads(reply["data"]["records_json"][0]) == {
        "calls": [], "commands": [], "edited_files": [], "skills": [],
    }
    limited = query(
        store, description, {"kind": "predicate_inputs", "order": "forward"},
        limits={**LIMITS, "max_output_bytes": 1},
    )
    assert limited["status"] == "output_limit"
    assert limited["complete"] is False


@pytest.mark.parametrize(("name", "errors", "input_regex", "expected"), [
    (None, "exclude", None, 2),
    (None, "include", None, 3),
    (None, "only", None, 1),
    (".*", "include", None, 1),
    ("Bash", "exclude", None, 0),
    ("Bash", "only", None, 1),
    (None, "include", {"field": "command", "pattern": "cargo", "flags": 0}, 1),
    (None, "exclude", {"field": "command", "pattern": "cargo", "flags": 0}, 0),
])
def test_nullable_tool_count_preserves_error_and_input_filters(
    tmp_path: Path, name: str | None, errors: str,
    input_regex: dict[str, Any] | None, expected: int,
) -> None:
    path = tmp_path / "counts.jsonl"
    path.write_bytes(
        event(0)
        + assistant("tools", [
            tool("Read", {"file_path": "a.py"}),
            tool("Bash", {"command": "cargo test"}),
            tool(".*", {}),
        ])
        + orjson.dumps({
            "type": "user", "uuid": "result", "sessionId": "s",
            "timestamp": "2026-01-02T03:04:07Z",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "Bash", "content": "failed", "is_error": True}]},
        }) + b"\n"
    )
    store = TranscriptStore({})
    description = acquire(store, path)
    reply = query(store, description, {"kind": "tool_count", "name": name, "input_regex": input_regex, "errors": errors})
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert reply["data"]["value"] == expected


def test_direct_sidechains_open_only_selected_children(tmp_path: Path) -> None:
    path = tmp_path / "root.jsonl"
    path.write_bytes(event(0) + assistant("dispatch", [tool("Agent", {"description": "selected", "prompt": "work", "subagent_type": "worker"})]))
    children = tmp_path / "root" / "subagents"
    children.mkdir(parents=True)
    child = children / "agent-Agent.jsonl"
    child.write_bytes(event(0, "selected child"))
    (children / "agent-other.jsonl").write_bytes(b"not a transcript\n")
    descendants = children / "agent-Agent" / "subagents"
    descendants.mkdir(parents=True)
    (descendants / "agent-grandchild.jsonl").write_bytes(b"not a transcript\n")
    store = TranscriptStore({})
    description = acquire(store, path)
    reply = query(
        store, description, {"kind": "direct_sidechains", "order": "forward", "dispatch_ids": ["Agent"]},
        attachments=[str(tmp_path / "must-not-open.jsonl")],
    )
    records: list[dict[str, Any]] = []
    source_opens = 0
    for _ in range(100):
        source_opens += reply["usage"]["source_opens"]
        if reply["data"] is not None and reply["data"]["kind"] == "records":
            assert reply["data"]["record_schema"] == "cc-transcript.sidechain/1"
            records.extend(orjson.loads(raw) for raw in reply["data"]["records_json"])
        if reply["complete"]:
            break
        assert reply["cursor"], reply
        reply = dict(store.request(request("resume", cursor=reply["cursor"]), context=context(), cancellation=CancellationToken()))
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert source_opens == 1
    assert len(records) == 1
    assert records[0]["path"] == str(child)
    assert records[0]["depth"] == 1
    assert records[0]["spawned_by"] == "Agent"
    assert records[0]["description"]["canonical_path"] == str(child)


def test_empty_direct_sidechains_do_not_discover_or_open_sources(tmp_path: Path) -> None:
    path = tmp_path / "root.jsonl"
    path.write_bytes(event(0))
    store = TranscriptStore({})
    description = acquire(store, path)
    reply = query(
        store, description, {"kind": "direct_sidechains", "order": "forward", "dispatch_ids": []},
        attachments=[str(tmp_path / "must-not-open.jsonl")],
    )
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert reply["data"]["records_json"] == []
    assert reply["usage"]["source_opens"] == 0
    assert reply["usage"]["discovery_entries_examined"] == 0


def test_named_tool_calls_omit_unrelated_large_results_before_encoding(tmp_path: Path) -> None:
    path = tmp_path / "selected-tools.jsonl"
    path.write_bytes(
        event(0)
        + assistant("tools", [
            tool("Read", {"file_path": "a.py"}),
            tool("Agent", {"description": "selected", "prompt": "work", "subagent_type": "worker"}),
        ])
        + orjson.dumps({
            "type": "user", "uuid": "results", "sessionId": "s",
            "timestamp": "2026-01-02T03:04:07Z",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "Read", "content": "x" * 128_000},
                {"type": "tool_result", "tool_use_id": "Agent", "content": "failed", "is_error": True},
            ]},
        }) + b"\n"
    )
    store = TranscriptStore({})
    description = acquire(store, path)
    limits = {**LIMITS, "max_output_bytes": 4096}
    reply = query(store, description, {"kind": "tool_calls", "order": "forward", "name": "Task"}, limits=limits)
    assert reply["status"] == "ok", reply
    assert reply["complete"] is True
    assert len(reply["data"]["records_json"]) == 1
    [dispatch] = decode_projection(reply["data"]["record_schema"], reply["data"]["records_json"])
    assert dispatch.call.name == "Agent"
    assert dispatch.result is not None
    assert dispatch.result.is_error is True
    unfiltered = query(store, description, {"kind": "tool_calls", "order": "forward"}, limits=limits)
    assert unfiltered["status"] == "output_limit"
    assert unfiltered["complete"] is False
    bounded = query(
        store, description, {"kind": "tool_calls", "order": "forward", "name": "Task"},
        limits={**limits, "max_read_bytes": 4096},
    )
    assert bounded["status"] == "incomplete"
    assert bounded["complete"] is False
    assert bounded["data"] is None
