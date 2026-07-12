from __future__ import annotations

import json
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING

import anyio

from cc_transcript.cli import watch_dict, watch_line
from cc_transcript.filterspec import event_meta
from cc_transcript.ids import SessionId
from cc_transcript.parser import decode_line
from cc_transcript.watch import TailState, WatchEvent, tick, watch

if TYPE_CHECKING:
    from cc_transcript.ids import EventUuid

SESSION = SessionId("44444444-4444-4444-4444-444444444444")


def line(uuid: str, text: str, *, session: str = str(SESSION), sidechain: bool = False) -> str:
    return json.dumps(
        {
            "uuid": uuid,
            "parentUuid": None,
            "sessionId": session,
            "timestamp": "2026-02-01T09:00:00+00:00",
            "isSidechain": sidechain,
            "type": "user",
            "message": {"role": "user", "content": text},
        }
    )


def transcript(root: Path) -> Path:
    path = root / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.touch()
    return path


def append(path: Path, text: str) -> None:
    with path.open("a") as handle:
        handle.write(text)


def run_tick(state: TailState, root: Path, *, from_start: bool = False) -> list[WatchEvent]:
    return anyio.run(partial(tick, state, (root,), from_start=from_start))


def uuids(events: list[WatchEvent]) -> list[EventUuid]:
    return [meta.uuid for item in events if (meta := event_meta(item.event)) is not None]


def test_appended_complete_lines_yield_once(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    state = TailState()
    assert run_tick(state, tmp_path) == []
    append(path, line("e1", "one") + "\n" + line("e2", "two") + "\n")
    events = run_tick(state, tmp_path)
    assert uuids(events) == ["e1", "e2"]
    assert all(item.session_id == SESSION and not item.is_sidechain for item in events)
    assert run_tick(state, tmp_path) == []


def test_partial_trailing_line_waits_for_its_newline(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    state = TailState()
    run_tick(state, tmp_path)
    full = line("e2", "two")
    append(path, line("e1", "one") + "\n" + full[:10])
    assert uuids(run_tick(state, tmp_path)) == ["e1"]
    append(path, full[10:] + "\n")
    assert uuids(run_tick(state, tmp_path)) == ["e2"]
    assert run_tick(state, tmp_path) == []


def test_truncation_resets_and_replays_new_content_once(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    path.write_text(line("e1", "one") + "\n" + line("e2", "two") + "\n")
    state = TailState()
    assert uuids(run_tick(state, tmp_path, from_start=True)) == ["e1", "e2"]
    path.write_text(line("e3", "compacted") + "\n")
    assert uuids(run_tick(state, tmp_path)) == ["e3"]
    assert run_tick(state, tmp_path) == []


def test_replayed_uuids_do_not_double_fire(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    state = TailState()
    run_tick(state, tmp_path)
    append(path, line("e1", "one") + "\n")
    assert uuids(run_tick(state, tmp_path)) == ["e1"]
    append(path, line("e1", "one, replayed") + "\n")
    assert run_tick(state, tmp_path) == []


def test_from_start_controls_preexisting_replay(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    path.write_text(line("e1", "one") + "\n")
    assert run_tick(TailState(), tmp_path) == []
    assert uuids(run_tick(TailState(), tmp_path, from_start=True)) == ["e1"]


def test_file_created_after_priming_reads_from_byte_zero(tmp_path: Path) -> None:
    state = TailState()
    assert run_tick(state, tmp_path) == []
    path = transcript(tmp_path)
    path.write_text(line("e1", "one") + "\n")
    assert uuids(run_tick(state, tmp_path)) == ["e1"]


def test_sidechain_files_are_flagged(tmp_path: Path) -> None:
    sidecar = tmp_path / "proj" / str(SESSION) / "subagents" / "agent-abc123.jsonl"
    sidecar.parent.mkdir(parents=True)
    sidecar.write_text(line("s1", "subagent work", sidechain=True) + "\n")
    events = run_tick(TailState(), tmp_path, from_start=True)
    assert uuids(events) == ["s1"]
    assert events[0].is_sidechain
    assert events[0].session_id == SESSION


def test_envelope_less_events_fall_back_to_path_session_and_garbage_skips(tmp_path: Path) -> None:
    path = transcript(tmp_path)
    path.write_text("not json\n" + json.dumps({"type": "summary", "summary": "t"}) + "\n")
    events = run_tick(TailState(), tmp_path, from_start=True)
    assert [item.session_id for item in events] == [SESSION]


def test_watch_generator_streams_appends(tmp_path: Path) -> None:
    async def scenario() -> WatchEvent:
        path = transcript(tmp_path)
        stream = watch((tmp_path,), poll=0.01)

        async def append_later() -> None:
            await anyio.sleep(0.05)
            append(path, line("e1", "one") + "\n")

        with anyio.fail_after(10):
            async with anyio.create_task_group() as tg:
                tg.start_soon(append_later)
                item = await anext(stream)
        await stream.aclose()
        return item

    item = anyio.run(scenario)
    assert uuids([item]) == ["e1"]
    assert item.session_id == SESSION


def test_watch_dict_and_line_render_the_event() -> None:
    event = decode_line(line("e1", "hello world", sidechain=True).encode())
    assert event is not None
    item = WatchEvent(path=Path("/t.jsonl"), session_id=SESSION, is_sidechain=True, event=event)
    payload = watch_dict(item)
    assert (payload["uuid"], payload["kind"], payload["role"]) == ("e1", "user", "user")
    assert (payload["session_id"], payload["is_sidechain"]) == (SESSION, True)
    assert "hello world" in payload["preview"]
    rendered = watch_line(item)
    assert rendered.startswith("09:00:00 44444444 user*")
    assert "hello world" in rendered
