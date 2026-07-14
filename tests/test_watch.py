from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import orjson

from cc_transcript.cli import watch_dict, watch_line
from cc_transcript.ids import EventUuid, SessionId
from cc_transcript.watch import Watcher, WatchEvent
from tests import testkit

SESSION = SessionId("44444444-4444-4444-4444-444444444444")


def test_watch_dict_and_line_render_the_event() -> None:
    event = testkit.parse_event(
        testkit.user_line("e1", "hello world", is_sidechain=True, timestamp=datetime(2026, 2, 1, 9, 0, 0, tzinfo=UTC))
    )
    item = WatchEvent(path=Path("/t.jsonl"), session_id=SESSION, is_sidechain=True, event=event)
    payload = watch_dict(item)
    assert (payload["uuid"], payload["kind"], payload["role"]) == ("e1", "user", "user")
    assert (payload["session_id"], payload["is_sidechain"]) == (SESSION, True)
    assert "hello world" in payload["preview"]
    rendered = watch_line(item)
    assert rendered.startswith("09:00:00 44444444 user*")
    assert "hello world" in rendered


def write_transcript(root: Path, name: str, *lines: dict) -> None:
    (root / name).write_bytes(b"\n".join(orjson.dumps(line) for line in lines) + b"\n")


def test_tick_from_start_yields_every_valid_event(tmp_path: Path) -> None:
    # Guards the regression where production watch silently discarded every event.
    write_transcript(
        tmp_path,
        "sess.jsonl",
        testkit.user_line("e1", "hello"),
        testkit.assistant_line("e2", "hi there", stop_reason="end_turn"),
        testkit.user_line("e3", "thanks"),
    )
    events = Watcher([tmp_path], from_start=True).tick()
    assert [event.event.meta.uuid for event in events] == [EventUuid("e1"), EventUuid("e2"), EventUuid("e3")]
    assert all(isinstance(event, WatchEvent) for event in events)


def test_watcher_primes_at_eof_then_drains_appends(tmp_path: Path) -> None:
    # The cc-transcript watch CLI command loops this tick; drive it directly.
    transcript = tmp_path / "s.jsonl"
    write_transcript(tmp_path, "s.jsonl", testkit.user_line("w0", "preexisting"))
    watcher = Watcher([tmp_path])
    assert watcher.tick() == []
    with transcript.open("ab") as handle:
        handle.write(orjson.dumps(testkit.user_line("w1", "hi")) + b"\n")
    events = watcher.tick()
    assert [event.event.meta.uuid for event in events] == [EventUuid("w1")]
