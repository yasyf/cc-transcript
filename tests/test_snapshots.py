from __future__ import annotations

import os
import time
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.snapshots import (
    CallContext,
    CancellationToken,
    SnapshotIncomplete,
    TranscriptStore,
    decode_projection,
)

CLASSIFIER = {"id": "native", "version": "1"}
LIMITS = {
    "max_read_bytes": 8 * 1024 * 1024,
    "max_events": 1000,
    "max_items": 256,
    "max_output_bytes": 1024 * 1024,
    "max_discovery_entries": 1000,
    "max_sources": 100,
}


def context(claimant: str = "test") -> CallContext:
    return {
        "claimant": claimant,
        "admission": "hook",
        "authority": {"kind": "user", "effective_uid": str(os.geteuid())},
        "registry_generation": "1",
    }


def event(index: int, text: str = "hello") -> bytes:
    return (
        orjson.dumps(
            {
                "type": "user",
                "uuid": f"u{index}",
                "sessionId": "s",
                "timestamp": "2026-01-02T03:04:05Z",
                "message": {"content": text},
            }
        )
        + b"\n"
    )


def deadline() -> int:
    return int(time.time() * 1000) + 30_000


def request(operation: str, **fields: Any) -> dict[str, Any]:
    return {"schema": "cc-transcript.snapshot/1", "id": "test", "operation": operation, **fields}


def acquire(store: TranscriptStore, path: Path) -> dict[str, Any]:
    token = CancellationToken()
    reply = store.request(
        request("acquire", path=str(path), classifier=CLASSIFIER, limits=LIMITS, deadline_unix_ms=deadline()),
        cancellation=token,
        context=context(),
    )
    for _ in range(100):
        if reply["status"] != "incomplete":
            assert reply["status"] == "ok", reply
            return dict(reply["data"]["description"])
        assert reply["cursor"], reply
        reply = store.request(request("resume", cursor=reply["cursor"]), cancellation=token, context=context())
    raise AssertionError("preparation failed to make bounded progress")


def test_scope_rejects_large_event_before_view_materializes(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "x" * 100_000))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(
        description["handle"],
        context=context(),
        cancellation=CancellationToken(),
        limits={**LIMITS, "max_output_bytes": 64},
        deadline_unix_ms=deadline(),
    ) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            snapshot.events[0]
        assert failure.value.status == "output_limit"
        assert snapshot.work["output_bytes"] == 0
        assert snapshot.usage["source_bytes_read"] == 0
        assert snapshot.usage["requests_failed"] == 1
    assert snapshot.usage["requests_failed"] == 1


def test_scoped_native_activity_keeps_absolute_turn_indexes(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index) for index in range(40)))
    store = TranscriptStore({"max_read_bytes_per_step": 512, "max_events_per_step": 3})
    description = acquire(store, path)
    with store.borrow_snapshot(
        description["handle"],
        context=context(),
        cancellation=CancellationToken(),
        limits={**LIMITS, "max_items": 3},
        deadline_unix_ms=deadline(),
    ) as snapshot:
        activity = snapshot.activity(
            CLASSIFIER, anchor=EventRef(SessionId("s"), EventUuid("u30")), lookback_turns=1, lookahead_turns=1
        )
        assert [turn.index for turn in activity.turns] == [29, 30, 31]
        assert [turn.events[0].meta.uuid for turn in activity.turns] == ["u29", "u30", "u31"]
        assert snapshot.usage["source_opens"] == 0
    with pytest.raises(SnapshotIncomplete):
        len(snapshot.events)


def test_owned_projection_survives_lease_release(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "detached content"))
    store = TranscriptStore({})
    description = acquire(store, path)
    handle = description["handle"]
    reply = store.request(
        request(
            "query",
            view={"handle": handle, "classifier": CLASSIFIER, "selectors": [], "attachments": []},
            query={"kind": "events", "order": "forward"},
            limits=LIMITS,
            deadline_unix_ms=deadline(),
        ),
        cancellation=CancellationToken(),
        context=context(),
    )
    assert reply["complete"], reply
    records = decode_projection(reply["data"]["record_schema"], reply["data"]["records_json"])
    released = store.request(
        request("release", owner_epoch=handle["owner_epoch"], kind="lease", token=handle["lease_id"]),
        cancellation=CancellationToken(),
        context=context(),
    )
    assert released["complete"], released
    assert records[0].text == "detached content"


def test_cancellation_reaches_scope_and_keeps_usage(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0))
    store = TranscriptStore({})
    description = acquire(store, path)
    token = CancellationToken()
    with store.borrow_snapshot(
        description["handle"], context=context(), cancellation=token, limits=LIMITS, deadline_unix_ms=deadline()
    ) as snapshot:
        token.cancel()
        with pytest.raises(SnapshotIncomplete) as failure:
            snapshot.checkpoint()
        assert failure.value.status == "cancelled"
        assert snapshot.usage["requests_cancelled"] == 1
