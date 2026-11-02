from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Any

import orjson
import pytest

from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.models import UserEvent
from cc_transcript.snapshots import CancellationToken, SnapshotIncomplete, TranscriptSnapshot, TranscriptStore
from tests.test_snapshots import CLASSIFIER, LIMITS, context, deadline, event


CONFIDENCE = {"stages": [{"kind": "Base", "band": 0.9, "reason": "base"}]}
SPEC = orjson.dumps({
    "detectors": ["transcript_message"], "reentry_lookback": 40, "edit_tools": ["Edit"],
    "plan_tools": ["ExitPlanMode"], "denial_excluded_tools": [], "provenance": {"subagent_tools": []},
    "user_message": CONFIDENCE, "calibrated": CONFIDENCE, "review": {"surfaces": ["typed"]},
}).decode()
POLICY = {"id": "registered-review-test", "version": "1", "classifier": CLASSIFIER}


def marker(item: object) -> None:
    return None


def run(store: TranscriptStore, path: Path, handler: Callable[[TranscriptSnapshot], Any], **fields: object) -> Any:
    return store.run_registered_review(
        path, policy=POLICY, mining_spec_json=SPEC, callable_formats=(), first_user_marker="owner words",
        prose_marker=marker, request={"id": "registered", "limits": LIMITS, "deadline_unix_ms": deadline(), **fields},
        context=context(), cancellation=CancellationToken(), handler=handler,
    )


def test_registered_scope_preserves_words_capture_and_warm_zero(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "preserve owner words"))
    store = TranscriptStore({})
    held: list[TranscriptSnapshot] = []

    def prepare(snapshot: TranscriptSnapshot) -> dict[str, object]:
        held.append(snapshot)
        assert snapshot.source_facts(first_user_contains="owner words")["first_user_contains"]
        owner_event = snapshot.events[0]
        assert isinstance(owner_event, UserEvent)
        assert owner_event.text == "preserve owner words"
        anchor = EventRef(SessionId("s"), EventUuid("u0"))
        assert snapshot.capture([anchor])[0].trigger is not None
        assert len(snapshot.activity(CLASSIFIER, anchors=[anchor]).turns) == 1
        return {"revision": snapshot.description["source_revision"], "signals": snapshot.mine_json(SPEC, ()), "usage": dict(snapshot.usage)}

    first = run(store, path, prepare)
    for _ in range(3):
        warm = run(store, path, prepare)
        assert warm["signals"] == first["signals"]
        assert warm["usage"]["source_bytes_read"] == 0
        assert warm["usage"]["events_parsed"] == 0
    with pytest.raises(SnapshotIncomplete):
        held[0].checkpoint()
    path.write_bytes(path.read_bytes() + event(1, "new owner words"))
    frozen = run(store, path, prepare, source_revision=first["revision"])
    assert frozen["revision"] == first["revision"]
    assert frozen["signals"] == first["signals"]
    assert frozen["usage"]["events_parsed"] == 0
    current = run(store, path, prepare)
    assert len(current["signals"]) == 2
    assert current["revision"] != first["revision"]


def test_registered_publication_pages_without_raw_lease(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "owner words"))
    store = TranscriptStore({})
    limits = {**LIMITS, "max_output_bytes": 8 * 1024 * 1024}
    request = {"id": "registered", "limits": limits, "deadline_unix_ms": deadline()}
    records = [orjson.dumps({"index": index, "text": "x" * 20_000}).decode() for index in range(100)]

    def publish(snapshot: TranscriptSnapshot) -> object:
        snapshot.consume(items=len(records), output_bytes=sum(map(len, records)))
        return snapshot.publish_projection(request, metadata={"source_revision": snapshot.description["source_revision"]},
            field="records_json", records_json=records, usage=snapshot.usage, work=snapshot.work)

    page = store.run_registered_review(path, policy=POLICY, mining_spec_json=SPEC, callable_formats=(),
        first_user_marker="owner words", prose_marker=marker, request=request, context=context(),
        cancellation=CancellationToken(), handler=publish)
    actual = list(page["records_json"])
    while page["cursor"] is not None:
        page = store.resume_projection(page["cursor"], context=context(), cancellation=CancellationToken())
        assert page is not None
        actual.extend(page["records_json"])
    assert actual == records
