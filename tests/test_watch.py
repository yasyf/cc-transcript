from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import anyio
import orjson

from cc_transcript import _parser_rs
from cc_transcript.cli import watch_dict, watch_line
from cc_transcript.ids import EventUuid, SessionId
from cc_transcript.watch import WatchEvent, tick, watch
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


async def drain_tick(root: Path) -> list[WatchEvent]:
    return await tick(_parser_rs.WatchTailer(), [root], from_start=True)


def test_tick_from_start_yields_every_valid_event(tmp_path: Path) -> None:
    # Guards the regression where production watch silently discarded every event.
    write_transcript(
        tmp_path,
        "sess.jsonl",
        testkit.user_line("e1", "hello"),
        testkit.assistant_line("e2", "hi there", stop_reason="end_turn"),
        testkit.user_line("e3", "thanks"),
    )
    events = anyio.run(drain_tick, tmp_path)
    assert [event.event.meta.uuid for event in events] == [EventUuid("e1"), EventUuid("e2"), EventUuid("e3")]
    assert all(isinstance(event, WatchEvent) for event in events)


async def first_watch_events(root: Path, count: int) -> list[WatchEvent]:
    out: list[WatchEvent] = []
    async for event in watch([root], poll=0.01, from_start=True):
        out.append(event)
        if len(out) >= count:
            return out
    return out


def test_watch_generator_yields_appended_events(tmp_path: Path) -> None:
    # The cc-transcript watch CLI command iterates this generator; drive it directly.
    write_transcript(tmp_path, "s.jsonl", testkit.user_line("w1", "hi"))
    events = anyio.run(first_watch_events, tmp_path, 1)
    assert [event.event.meta.uuid for event in events] == [EventUuid("w1")]
