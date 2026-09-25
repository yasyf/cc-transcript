from __future__ import annotations

from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import pytest

from cc_transcript import _native
from cc_transcript.activity import parse
from cc_transcript.context import ContextWindow, capture_windows, hydrate_windows
from cc_transcript.ids import EventRef, EventUuid, SessionId
from cc_transcript.render import Budget
from tests.test_context import SESSION, ref, session_bytes, write_transcript


@pytest.fixture
def projects_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr("cc_transcript.discovery.CLAUDE_PROJECTS_DIR", tmp_path)
    return tmp_path


@pytest.mark.parametrize("preview_chars", [0, 50, 200])
def test_capture_windows_matches_individual_windows(preview_chars: int) -> None:
    anchors = [ref("a3"), ref("a0", "t1"), ref("a2", "t3"), ref("a0", "t1")]
    raw = session_bytes()
    expected = [
        ContextWindow.from_json(
            _native.context_capture_window(
                raw, anchor.session_id, anchor.event_uuid, anchor.tool_use_id, 6, 2, preview_chars
            )
        )
        for anchor in anchors
    ]
    with patch.object(_native, "context_capture_windows", wraps=_native.context_capture_windows) as capture:
        actual = capture_windows(raw, anchors, preview_chars=preview_chars)
    assert actual == expected
    assert [window.anchor for window in actual] == anchors
    capture.assert_called_once_with(
        raw, [(anchor.session_id, anchor.event_uuid, anchor.tool_use_id) for anchor in anchors], 6, 2, preview_chars
    )


def test_capture_windows_skips_parse_for_empty_batch() -> None:
    assert capture_windows(b"not a transcript", []) == []


def test_capture_windows_rejects_any_missing_anchor() -> None:
    with pytest.raises(ValueError, match="anchor missing"):
        capture_windows(session_bytes(), [ref("a0"), ref("missing"), ref("a2")])


def test_capture_windows_preserves_session_identity() -> None:
    other = EventRef(SessionId("other"), EventUuid("a2"))
    windows = capture_windows(session_bytes(), [ref("a0"), other, ref("a2")])
    assert [window.anchor.session_id for window in windows] == [SESSION, "other", SESSION]
    assert windows[1].trigger is not None
    assert windows[1].trigger.refs[0].session_id == SESSION


def test_hydrate_windows_reads_each_session_once_and_preserves_order(projects_root: Path) -> None:
    path = write_transcript(projects_root)
    other_session = SessionId("33333333-3333-3333-3333-333333333333")
    other_path = path.with_name(f"{other_session}.jsonl")
    other_raw = session_bytes().replace(str(SESSION).encode(), str(other_session).encode())
    other_path.write_bytes(other_raw)
    first = capture_windows(session_bytes(), [ref("a0"), ref("a2", "t3")], before=0, after=0)
    other = capture_windows(other_raw, [EventRef(other_session, EventUuid("a1"))], before=0, after=0)[0]
    windows = [first[1], other, first[0], first[1]] * 10
    expected = [window.hydrate() for window in windows]
    with patch("cc_transcript.activity.parse", wraps=parse) as load:
        actual = hydrate_windows(windows)
    assert actual == expected
    assert [call.args[0] for call in load.call_args_list] == [path, other_path]
    assert actual[0] is not None and actual[3] is not None
    assert actual[0].turns[0] is actual[3].turns[0]
    assert [result.turns[0].prompt if result else None for result in actual] == ["three", "two", "one", "three"] * 10


def test_hydrate_windows_checks_expired_session_once(projects_root: Path) -> None:
    windows = capture_windows(session_bytes(), [ref("a0"), ref("a2")])
    with patch("cc_transcript.activity.resolve", return_value=None) as resolve:
        assert hydrate_windows(windows) == [None, None]
    resolve.assert_called_once_with(SESSION, root=None)


def test_hydrate_windows_isolates_unresolvable_refs(projects_root: Path) -> None:
    write_transcript(projects_root)
    window = capture_windows(session_bytes(), [ref("a0")], before=0, after=0)[0]
    assert window.trigger is not None
    missing = replace(window, trigger=replace(window.trigger, refs=(ref("missing"),)))
    empty = replace(window, trigger=replace(window.trigger, refs=()))
    with patch("cc_transcript.activity.parse", wraps=parse) as load:
        actual = hydrate_windows([missing, window, empty, window])
    assert actual[0] is None
    assert actual[2] is None
    assert actual[1] is not None
    assert actual[3] == actual[1]
    load.assert_called_once()


def test_hydrate_windows_does_not_reuse_reads_across_batches(projects_root: Path) -> None:
    path = write_transcript(projects_root)
    window = capture_windows(session_bytes(), [ref("a0")], before=0, after=0)[0]
    initial = hydrate_windows([window])[0]
    assert initial is not None
    assert "user: one" in initial.render(budget=Budget())
    path.write_bytes(session_bytes().replace(b'"one"', b'"changed"'))
    updated = hydrate_windows([window])[0]
    assert updated is not None
    assert "user: changed" in updated.render(budget=Budget())
    path.unlink()
    assert hydrate_windows([window]) == [None]


def test_hydrate_windows_empty_batch_does_not_read() -> None:
    with patch("cc_transcript.activity.parse", side_effect=AssertionError("unexpected read")):
        assert hydrate_windows([]) == []
