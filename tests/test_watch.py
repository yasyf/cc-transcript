from __future__ import annotations

import json
import signal
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path

import orjson

from cc_transcript.ids import EventUuid, SessionId
from cc_transcript.watch import Watcher, WatchEvent
from tests import testkit

CLI = Path(sys.executable).parent / "cc-transcript"
SESSION = SessionId("44444444-4444-4444-4444-444444444444")


def spawn_watch(tmp_path: Path, *args: str) -> subprocess.Popen[str]:
    return subprocess.Popen(
        [str(CLI), "watch", "--root", str(tmp_path), "--from-start", "--poll", "0.05", *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def first_line_then_sigint(proc: subprocess.Popen[str]) -> str:
    assert proc.stdout is not None
    line = proc.stdout.readline()
    proc.send_signal(signal.SIGINT)
    assert proc.wait(timeout=30) == 0
    return line


def test_watch_cli_renders_lines_and_exits_zero_on_sigint(tmp_path: Path) -> None:
    # The agent-*.jsonl name marks a subagent sidechain, which renders the tag star.
    write_transcript(
        tmp_path,
        "agent-toolu_w1.jsonl",
        testkit.user_line(
            "e1",
            "hello world",
            session_id=str(SESSION),
            timestamp=datetime(2026, 2, 1, 9, 0, 0, tzinfo=UTC),
        ),
    )
    line = first_line_then_sigint(spawn_watch(tmp_path))
    assert line.startswith("09:00:00 44444444 user*")
    assert "hello world" in line


def test_watch_cli_json_leg_renders_the_event_dict(tmp_path: Path) -> None:
    write_transcript(
        tmp_path,
        f"{SESSION}.jsonl",
        testkit.user_line(
            "e1",
            "hello world",
            session_id=str(SESSION),
            timestamp=datetime(2026, 2, 1, 9, 0, 0, tzinfo=UTC),
        ),
    )
    payload = json.loads(first_line_then_sigint(spawn_watch(tmp_path, "--json")))
    assert (payload["uuid"], payload["kind"], payload["role"]) == ("e1", "user", "user")
    assert (payload["session_id"], payload["is_sidechain"]) == (SESSION, False)
    assert "hello world" in payload["preview"]


def test_watch_rejects_a_negative_poll_cleanly(tmp_path: Path) -> None:
    result = subprocess.run(
        [str(CLI), "watch", "--root", str(tmp_path), "--poll", "-1"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 2
    assert "not a non-negative number" in result.stderr
    assert "panic" not in result.stderr
    assert "Traceback" not in result.stderr


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
