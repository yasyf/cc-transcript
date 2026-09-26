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


def test_attachment_contract_accepts_active_session_and_rejects_overflow() -> None:
    assert len(REQUEST.validate_python(query_with_attachments(918)).view.attachments) == 918
    with pytest.raises(ValidationError, match="attachments"):
        REQUEST.validate_python(query_with_attachments(1025))
