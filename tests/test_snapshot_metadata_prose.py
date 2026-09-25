from __future__ import annotations

from pathlib import Path

import orjson
import pytest

from cc_transcript.models import AssistantEvent, UserEvent
from cc_transcript.parser import parse
from cc_transcript.snapshots import CancellationToken, SnapshotIncomplete, TranscriptStore
from tests.test_snapshots import LIMITS, acquire, context, deadline, event


def record(kind: str, uuid: str, cwd: str, content: str | list[dict[str, str]], **flags: bool) -> bytes:
    return orjson.dumps({
        "type": kind, "uuid": uuid, "sessionId": "s", "cwd": cwd,
        "timestamp": "2026-01-02T03:04:05Z", **flags,
        **({"subtype": "test"} if kind == "system" else {}),
        "message": {"model": "m", "content": content},
    }) + b"\n"


def test_large_unrelated_tool_result_does_not_block_narrow_facts_or_prose(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(
        record("assistant", "a", "/first", [{"type": "text", "text": " left "}, {"type": "thinking", "thinking": "private"}, {"type": "text", "text": "right"}])
        + record("user", "result", "/tools", [{"type": "tool_result", "tool_use_id": "t", "content": "x" * 128_000}], isMeta=True, isSidechain=True)
        + record("user", "u", "/later", "REVIEWER_MARKER later prompt", isMeta=True)
        + record("system", "system", "/system", "not prose")
        + record("assistant", "b", "/first", [{"type": "text", "text": ""}, {"type": "text", "text": "é\n日"}, {"type": "text", "text": ""}], isSidechain=True)
    )
    expected = [
        {"event_index": index, "role": "user" if isinstance(item, UserEvent) else "assistant", "text": item.text,
         "is_sidechain": item.meta.is_sidechain, "is_meta": item.meta.is_meta}
        for index, item in enumerate(parse(path).events)
        if isinstance(item, UserEvent | AssistantEvent) and item.text.strip()
    ]
    store = TranscriptStore({"max_read_bytes_per_step": 256 * 1024, "max_entry_bytes": 256 * 1024})
    description = acquire(store, path)
    limits = {**LIMITS, "max_read_bytes": 4096, "max_output_bytes": 4096}
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=limits, deadline_unix_ms=deadline()) as snapshot:
        def forbidden_getter(*args: object) -> None:
            raise AssertionError("narrow projection materialized a full event")

        monkeypatch.setattr(type(snapshot.events), "__getitem__", forbidden_getter)
        assert snapshot.source_facts(first_user_contains="REVIEWER_MARKER") == {
            "cwds": ["/first", "/tools", "/later", "/system"], "first_user_contains": False,
        }
        assert list(snapshot.prose_rows()) == expected
        assert 0 < snapshot.work["read_bytes"] < 4096
        assert snapshot.usage["source_opens"] == 0
    monkeypatch.undo()
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=limits, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete):
            snapshot.events[1]


@pytest.mark.parametrize(("token", "expected"), [("ab  é日", True), ("ab é日", False), ("", False), ("later", False)])
def test_first_user_search_preserves_flags_and_exact_text_separators(tmp_path: Path, token: str, expected: bool) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(
        record("user", "first", "/repo", [{"type": "text", "text": "ab"}, {"type": "text", "text": ""}, {"type": "text", "text": "é日"}], isMeta=True, isSidechain=True)
        + record("user", "second", "/repo", "later")
    )
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        assert snapshot.source_facts(first_user_contains=token) == {"cwds": ["/repo"], "first_user_contains": expected}


def test_source_facts_requires_complete_metadata_scan(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(record("user", "first", "/first", "plain") + record("assistant", "last", "/last", []))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_events": 1}, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            snapshot.source_facts(first_user_contains="absent")
        assert failure.value.status == "entry_limit"


def test_source_facts_without_a_user_preserves_empty_cwd(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(record("assistant", "only", "", [{"type": "text", "text": "token"}]))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        assert snapshot.source_facts(first_user_contains="token") == {"cwds": [""], "first_user_contains": False}


def test_source_facts_materialization_budget_is_cumulative(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(record("user", "first", "/repo", "plain"))
    store = TranscriptStore({})
    description = acquire(store, path)
    expected = {"cwds": ["/repo"], "first_user_contains": False}
    cap = 2 * len(orjson.dumps(expected)) - 1
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_output_bytes": cap}, deadline_unix_ms=deadline()) as snapshot:
        assert snapshot.source_facts(first_user_contains="absent") == expected
        with pytest.raises(SnapshotIncomplete):
            snapshot.source_facts(first_user_contains="absent")
        assert snapshot.work["materialized_output_bytes"] <= cap


def test_prose_pages_stop_on_cancellation_between_pages(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index, f"line {index}") for index in range(300)))
    store = TranscriptStore({"max_read_bytes_per_step": 256 * 1024, "max_events_per_step": 256})
    description = acquire(store, path)
    cancellation = CancellationToken()
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=cancellation, limits={**LIMITS, "max_items": 1000}, deadline_unix_ms=deadline()) as snapshot:
        rows = snapshot.prose_rows()
        assert [next(rows)["event_index"] for _ in range(256)] == list(range(256))
        cancellation.cancel()
        with pytest.raises(SnapshotIncomplete) as failure:
            next(rows)
        assert failure.value.status == "cancelled"


def test_prose_pages_share_the_scope_event_budget(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index, f"line {index}") for index in range(300)))
    store = TranscriptStore({"max_read_bytes_per_step": 256 * 1024, "max_events_per_step": 256})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_events": 257, "max_items": 1000}, deadline_unix_ms=deadline()) as snapshot:
        rows = snapshot.prose_rows()
        assert [next(rows)["event_index"] for _ in range(256)] == list(range(256))
        with pytest.raises(SnapshotIncomplete) as failure:
            next(rows)
        assert failure.value.status == "entry_limit"
        assert snapshot.work["events"] == 257


def test_prose_omits_python_whitespace_without_normalizing_nonblank_text(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "\u001c \t") + event(1, "  keep\n exact  "))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        assert list(snapshot.prose_rows()) == [{"event_index": 1, "role": "user", "text": "  keep\n exact  ", "is_sidechain": False, "is_meta": False}]


def test_prose_byte_pages_preserve_rows_beyond_one_wire_page(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(b"".join(event(index, f"{index}:" + "x" * 80_000) for index in range(20)))
    store = TranscriptStore({"max_read_bytes_per_step": 256 * 1024, "max_entry_bytes": 256 * 1024, "max_events_per_step": 256})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_output_bytes": 4 * 1024 * 1024}, deadline_unix_ms=deadline()) as snapshot:
        rows = list(snapshot.prose_rows())
        assert [row["event_index"] for row in rows] == list(range(20))
        assert all(row["text"] == f"{index}:" + "x" * 80_000 for index, row in enumerate(rows))
        assert 1024 * 1024 < snapshot.work["materialized_output_bytes"] < 4 * 1024 * 1024


def test_prose_item_page_exposes_early_match_before_exhausted_continuation(tmp_path: Path) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "MARKER") + b"".join(event(index, f"line {index}") for index in range(1, 256)))
    store = TranscriptStore({"max_read_bytes_per_step": 256 * 1024, "max_events_per_step": 256})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        assert snapshot.source_facts(first_user_contains="MARKER")["first_user_contains"]
        rows = snapshot.prose_rows()
        assert next(rows)["text"] == "MARKER"
        assert [next(rows)["event_index"] for _ in range(254)] == list(range(1, 255))
        assert snapshot.work["items"] == 256
        with pytest.raises(SnapshotIncomplete) as failure:
            next(rows)
        assert failure.value.status == "output_limit"
        assert snapshot.work["items"] == 256
