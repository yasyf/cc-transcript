from __future__ import annotations

import os
import re
import time
from hashlib import sha256
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
    "max_source_read_bytes": 8 * 1024 * 1024,
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
        "registry_generation": sha256(b"{}").hexdigest(),
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
        assert re.fullmatch(r"event 0 needs \d+ bytes, over the 64-byte remaining output budget", failure.value.reason)
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
            CLASSIFIER, anchors=[EventRef(SessionId("s"), EventUuid("u30"))], lookback_turns=1, lookahead_turns=1
        )
        assert [turn.index for turn in activity.turns] == [29, 30, 31]
        assert [turn.events[0].meta.uuid for turn in activity.turns] == ["u29", "u30", "u31"]
        assert snapshot.usage["source_opens"] == 0
    with pytest.raises(SnapshotIncomplete):
        len(snapshot.events)


def test_scoped_native_activity_spans_every_anchor_window(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index) for index in range(40)))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(
        description["handle"],
        context=context(),
        cancellation=CancellationToken(),
        limits=LIMITS,
        deadline_unix_ms=deadline(),
    ) as snapshot:
        activity = snapshot.activity(
            CLASSIFIER,
            anchors=[EventRef(SessionId("s"), EventUuid("u30")), EventRef(SessionId("s"), EventUuid("u10"))],
            lookback_turns=2,
            lookahead_turns=1,
        )
        assert [turn.index for turn in activity.turns] == list(range(8, 32))


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


def test_external_classifier_labels_are_generation_bound(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index) for index in range(3)))
    store = TranscriptStore({})
    original = acquire(store, path)
    token = CancellationToken()
    page = store.prepare_classifier(
        original["handle"],
        {"id": "test-policy", "version": "1"},
        context=context(),
        cancellation=token,
        limits=LIMITS,
        deadline_unix_ms=deadline(),
    )
    assert not page["complete"]
    events = decode_projection(page["record_schema"], page["records_json"])
    assert [record.meta.uuid for record in events] == ["u0", "u1", "u2"]
    result = store.submit_classifier(page["cursor"], [True, False, True], context=context(), cancellation=token)
    assert result["complete"], result
    description = result["description"]
    assert description["classifier"] != {"id": "test-policy", "version": "1"}
    assert description["handle"]["generation"] != original["handle"]["generation"]
    with store.borrow_snapshot(
        description["handle"], context=context(), cancellation=token, limits=LIMITS, deadline_unix_ms=deadline()
    ) as snapshot:
        assert len(snapshot.activity(description["classifier"]).turns) == 2
    with pytest.raises(SnapshotIncomplete):
        store.submit_classifier(page["cursor"], [True, False, True], context=context(), cancellation=token)


def test_classifier_facts_use_all_source_users_and_first_event_positions(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, " ordinary ") + event(1, "\u001c<system_instruction> instruction"))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(
        description["handle"],
        context=context(),
        cancellation=CancellationToken(),
        limits=LIMITS,
        deadline_unix_ms=deadline(),
    ) as snapshot:
        assert snapshot.classifier_facts("<system_instruction>", event_limit=1) == {
            "has_users": True,
            "all_users_sidechain": False,
            "has_user_prefix": False,
        }
        assert snapshot.classifier_facts("<system_instruction>", event_limit=2)["has_user_prefix"]
        assert snapshot.usage["source_bytes_read"] == 0


def test_classifier_facts_fit_a_one_megabyte_scope_past_large_early_tool_payloads(tmp_path: Path) -> None:
    payload = "x" * 700 * 1024
    line = {"sessionId": "s", "timestamp": "2026-01-02T03:04:05Z"}
    path = tmp_path / "s.jsonl"
    path.write_bytes(
        event(0, "ordinary")
        + orjson.dumps(
            line
            | {
                "type": "assistant",
                "uuid": "write",
                "message": {
                    "model": "m",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "w",
                            "name": "Write",
                            "input": {"file_path": "a", "content": payload},
                        }
                    ],
                },
            }
        )
        + b"\n"
        + orjson.dumps(
            line
            | {
                "type": "user",
                "uuid": "result",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "w", "content": payload}]},
            }
        )
        + b"\n"
        + event(3, "<system_instruction> lane")
    )
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(
        description["handle"],
        context=context(),
        cancellation=CancellationToken(),
        limits={**LIMITS, "max_read_bytes": 1024 * 1024},
        deadline_unix_ms=deadline(),
    ) as snapshot:
        assert snapshot.classifier_facts("<system_instruction>") == {
            "has_users": True,
            "all_users_sidechain": False,
            "has_user_prefix": True,
        }
        assert snapshot.work["read_bytes"] < 64 * 1024


def test_tool_registry_identity_is_definition_bound() -> None:
    store = TranscriptStore({})
    assert store.register_tool_registry([], context=context()) == context()["registry_generation"]
    spec = {"name": "read_source", "behaves_like": "Read", "span_edit": None}
    first = store.register_tool_registry([spec], context=context())
    assert first == store.register_tool_registry([spec], context=context())
    assert first != context()["registry_generation"]
    assert first != store.register_tool_registry([{**spec, "behaves_like": "Edit"}], context=context())


def test_tail_returns_the_newest_events_without_a_lease(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index) for index in range(40)))
    store = TranscriptStore({"max_read_bytes_per_step": 64})
    reply = store.request(
        request("tail", path=str(path), count=3, limits=LIMITS, deadline_unix_ms=deadline()),
        cancellation=CancellationToken(),
        context=context(),
    )
    assert reply["status"] == "ok", reply
    data = reply["data"]
    assert data["kind"] == "tail"
    assert data["source_bytes"] == path.stat().st_size
    assert data["window_start_byte"] == sum(len(event(index)) for index in range(37))
    events = decode_projection(data["record_schema"], data["records_json"])
    assert [record.meta.uuid for record in events] == ["u37", "u38", "u39"]
    assert reply["usage"]["source_bytes_read"] < 8 * len(event(0))
    bounded = store.request(
        request(
            "tail",
            path=str(path),
            count=3,
            limits={**LIMITS, "max_source_read_bytes": len(event(39)) + 1},
            deadline_unix_ms=deadline(),
        ),
        cancellation=CancellationToken(),
        context=context(),
    )
    assert bounded["status"] == "incomplete"
    assert bounded["reason"] == "tail read budget exhausted"
    assert bounded["cursor"] is None
    assert [record.meta.uuid for record in decode_projection("cc-transcript.event/1", bounded["data"]["records_json"])] == [
        "u39"
    ]
