"""The provider gateway routes codex rollouts through the same _native oracle
surfaces Claude transcripts use: the disk-only ``session_activity_probe`` and
``parse``. A codex file is sniffed, parsed with the whole-buffer fold, and
lowered into the native Entry model, so the probe reports honest lifecycle-aware
verdicts and ``parse`` yields the lowered event stream."""

from __future__ import annotations

from pathlib import Path

import pytest

from cc_transcript.activity_probe import session_activity_probe
from cc_transcript.parser import parse

CODEX = Path(__file__).resolve().parent / "testdata" / "codex"


def codex_fixture(tag: str) -> Path:
    return next(CODEX.glob(f"*{tag}.jsonl"))


def test_open_turn_dangling_call_is_mid_tool() -> None:
    probe = session_activity_probe(codex_fixture("050a"))
    assert probe.mid_tool is True
    assert probe.is_waiting is False
    assert probe.last_event_epoch == 1784220090
    assert [item.tool_use_id for item in probe.pending] == ["call_Demo0005dang0005dang0005x"]
    assert [item.kind for item in probe.pending] == ["mid_tool"]


def test_completed_turn_is_not_mid_tool() -> None:
    probe = session_activity_probe(codex_fixture("050c"))
    assert probe.mid_tool is False
    assert probe.is_waiting is False
    assert probe.pending == ()


def test_task_complete_clears_dangling_call(tmp_path: Path) -> None:
    rollout = tmp_path / "rollout.jsonl"
    rollout.write_text(
        "\n".join(
            [
                '{"type":"session_meta","payload":{"session_id":"completed-dangling"}}',
                '{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}',
                '{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"dangling","name":"exec","input":"pwd","internal_chat_message_metadata_passthrough":{"turn_id":"T"}}}',
                '{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T"}}',
            ]
        )
        + "\n"
    )
    probe = session_activity_probe(rollout)
    assert probe.mid_tool is False
    assert probe.is_waiting is False
    assert probe.pending == ()


@pytest.mark.parametrize("tag", ["050a", "050c", "303", "404", "101"])
def test_parse_yields_lowered_codex_events(tag: str) -> None:
    path = codex_fixture(tag)
    from_path = list(parse(path).events)
    from_bytes = list(parse(path.read_bytes()).events)
    assert len(from_path) > 0
    assert len(from_path) == len(from_bytes)
