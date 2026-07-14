from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

from cc_transcript.cli import watch_dict, watch_line
from cc_transcript.ids import SessionId
from cc_transcript.watch import WatchEvent
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
