from __future__ import annotations

import pytest
from pydantic import ValidationError

from scripts.snapshot_contract import REQUEST


def query_with_attachments(count: int) -> dict[str, object]:
    return {
        "schema": "cc-transcript.snapshot/1",
        "id": "attachments",
        "operation": "query",
        "view": {
            "handle": {
                "owner_epoch": "owner",
                "snapshot_id": "snapshot",
                "generation": "generation",
                "lease_id": "lease",
            },
            "classifier": {"id": "native", "version": "1"},
            "selectors": [],
            "attachments": [f"/sessions/{index}.jsonl" for index in range(count)],
        },
        "query": {"kind": "has_tool", "pattern": "Read", "subagents": False},
        "deadline_unix_ms": 1,
        "limits": {
            "max_read_bytes": 1,
            "max_events": 1,
            "max_items": 1,
            "max_output_bytes": 1,
            "max_discovery_entries": 1,
            "max_sources": 1,
        },
    }


def prepare_with_threads(count: int) -> dict[str, object]:
    payload = query_with_attachments(0)
    payload["operation"] = "prepare_graph"
    payload.pop("query")
    payload["thread_ids"] = [f"thread-{index}" for index in range(count)]
    payload["roots"] = ["/sessions"]
    payload["direct_paths"] = []
    return payload


def test_prepared_registry_accepts_active_session_and_rejects_overflow() -> None:
    assert len(REQUEST.validate_python(prepare_with_threads(918)).thread_ids) == 918
    with pytest.raises(ValidationError, match="thread_ids"):
        REQUEST.validate_python(prepare_with_threads(1025))


def test_ordinary_query_cannot_carry_attachment_paths() -> None:
    with pytest.raises(ValidationError, match="attachments"):
        REQUEST.validate_python(query_with_attachments(1))


def test_query_cannot_select_background_work_class() -> None:
    payload = query_with_attachments(0)
    payload["work_class"] = "background"
    with pytest.raises(ValidationError, match="work_class"):
        REQUEST.validate_python(payload)


def test_deep_query_requires_complete_prepared_handle() -> None:
    payload = query_with_attachments(0)
    payload.pop("view")
    payload["operation"] = "query_graph"
    payload["selectors"] = []
    payload["query"] = {"kind": "has_tool", "pattern": "Read", "subagents": True}
    with pytest.raises(ValidationError, match="handle"):
        REQUEST.validate_python(payload)
    payload["handle"] = {"graph_id": "graph", "owner_epoch": "epoch", "revision": "revision", "complete": True}
    assert REQUEST.validate_python(payload).handle.graph_id == "graph"
