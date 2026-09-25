from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Iterator
from pathlib import Path

import orjson
import pytest

from cc_transcript.mining import CallableReviewFormat, MiningSpec, ReviewComment, ReviewSpec, mine
from cc_transcript.mining.spec import signal_to_dict
from cc_transcript.parser import parse
from cc_transcript.snapshots import CancellationToken, SnapshotIncomplete, TranscriptStore
from tests.test_snapshots import LIMITS, acquire, context, deadline, event


def policy(extract: Callable[[str], Iterable[ReviewComment]], *, bounded: bool = True) -> MiningSpec:
    return MiningSpec(
        detectors=frozenset({"review_comment"}),
        review=ReviewSpec(callable_formats=(CallableReviewFormat("test", re.compile("review"), extract, bounded),)),
    )


def test_snapshot_mining_preserves_context_without_python_event_getters(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / "s.jsonl"
    path.write_bytes(
        event(0, "initial ask")
        + orjson.dumps({"type": "assistant", "uuid": "a", "sessionId": "s", "timestamp": "2026-01-02T03:04:06Z", "message": {"model": "m", "content": [{"type": "text", "text": "answer"}]}})
        + b"\n"
        + event(1, "No, keep the public API unchanged.")
    )
    spec = MiningSpec()
    expected = [signal_to_dict(signal) for signal in mine(parse(path).events, spec)]
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        def forbidden_getter(*args: object) -> None:
            raise AssertionError("native mining must not materialize Python event views")

        monkeypatch.setattr(type(snapshot.events), "__getitem__", forbidden_getter)
        actual = [signal_to_dict(signal) for signal in snapshot.mine(spec)]
    assert actual == expected
    assert actual[-1]["trigger_index"] == 1


def test_callback_stops_before_requesting_an_item_past_budget(tmp_path: Path) -> None:
    generated: list[int] = []

    def extract(text: str) -> Iterator[ReviewComment]:
        for index in range(1000):
            generated.append(index)
            yield ReviewComment("a.py", 1, 1, f"finding {index}")

    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "review"))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_items": 1}, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            list(snapshot.mine(policy(extract)))
    assert failure.value.status == "output_limit"
    assert generated == [0]


def test_source_preflight_precedes_callbacks(tmp_path: Path) -> None:
    def extract(text: str) -> Iterator[ReviewComment]:
        raise AssertionError("oversized input reached callback")
        yield ReviewComment(None, None, None, "unreachable")

    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "review " + "x" * 100_000))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits={**LIMITS, "max_read_bytes": 128}, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            list(snapshot.mine(policy(extract)))
    assert failure.value.status == "source_limit"


def test_unregistered_callback_fails_before_invocation(tmp_path: Path) -> None:
    def extract(text: str) -> Iterator[ReviewComment]:
        raise AssertionError("unregistered callback was invoked")
        yield ReviewComment(None, None, None, "unreachable")

    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "review"))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            list(snapshot.mine(policy(extract, bounded=False)))
    assert failure.value.status == "incomplete"


def test_bounded_callback_must_return_iterator(tmp_path: Path) -> None:
    def extract(text: str) -> tuple[ReviewComment, ...]:
        return (ReviewComment(None, None, None, "tuple"),)

    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "review"))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(SnapshotIncomplete) as failure:
            list(snapshot.mine(policy(extract)))
    assert failure.value.status == "incomplete"


def test_callback_runtime_error_keeps_original_identity(tmp_path: Path) -> None:
    callback_error = RuntimeError("output_limit", "application error")

    def extract(text: str) -> Iterator[ReviewComment]:
        raise callback_error
        yield ReviewComment(None, None, None, "unreachable")

    path = tmp_path / "s.jsonl"
    path.write_bytes(event(0, "review"))
    store = TranscriptStore({})
    description = acquire(store, path)
    with store.borrow_snapshot(description["handle"], context=context(), cancellation=CancellationToken(), limits=LIMITS, deadline_unix_ms=deadline()) as snapshot:
        with pytest.raises(RuntimeError) as failure:
            list(snapshot.mine(policy(extract)))
    assert failure.value is callback_error
