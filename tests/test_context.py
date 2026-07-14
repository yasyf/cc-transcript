from __future__ import annotations

import json
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from functools import partial
from typing import TYPE_CHECKING, Any

import anyio
import pytest

from cc_transcript.activity import SessionActivity
from cc_transcript.context import (
    PREVIEW_SCHEMA,
    SUMMARY_LABEL,
    AskUserQuestionPreview,
    ContextWindow,
    SchemaError,
    TextPreview,
    ToolCallPreview,
    TurnRef,
    capture_window,
)
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId, tool_digest
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.render import Budget
from tests import testkit
from tests.support import assistant as _assistant
from tests.support import user as _user

if TYPE_CHECKING:
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 2, 1, 9, 0, 0, tzinfo=UTC)
SESSION = SessionId("22222222-2222-2222-2222-222222222222")
LONG_OLD = "o" * 120
LONG_NEW = "n" * 120
EDIT_INPUT = {"file_path": "/a.py", "old_string": LONG_OLD, "new_string": LONG_NEW}


user = partial(_user, session=SESSION, base=BASE)
assistant = partial(_assistant, session=SESSION, base=BASE)


def ref(uuid: str, tool_use_id: str | None = None) -> EventRef:
    return EventRef(SESSION, EventUuid(uuid), ToolUseId(tool_use_id) if tool_use_id else None)


def session_events() -> tuple[TranscriptEvent, ...]:
    return (
        user("u0", "one"),
        assistant(
            "a0",
            "working",
            blocks=(testkit.tool_use("t1", "Edit", {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"}),),
            secs=1,
        ),
        user("u1", "two", secs=2),
        assistant("a1", "", blocks=(testkit.tool_use("t2", "Bash", {"command": "uv run pytest"}),), secs=3),
        user("u2", "three", secs=4),
        assistant("a2", "", blocks=(testkit.tool_use("t3", "Edit", EDIT_INPUT),), secs=5),
        user("u3", "four", secs=6),
        assistant("a3", "done", secs=7),
    )


def in_memory_window(**overrides: Any) -> ContextWindow:
    activity = SessionActivity.from_events(SESSION, session_events())
    window = capture_window(activity, ref("a2", "t3"), before=2, after=1, preview_chars=50)
    return replace(window, **overrides) if overrides else window


def transcript_line(uuid: str, secs: int, **overrides: Any) -> str:
    return json.dumps(
        {
            "uuid": uuid,
            "parentUuid": None,
            "sessionId": str(SESSION),
            "timestamp": (BASE + timedelta(seconds=secs)).isoformat(),
            "cwd": "/repo",
            "gitBranch": "main",
            "version": "1.2.3",
            "isSidechain": False,
        }
        | overrides
    )


def assistant_line(uuid: str, secs: int, content: list[dict[str, Any]]) -> str:
    return transcript_line(
        uuid, secs, type="assistant", message={"role": "assistant", "model": "claude-opus-4-7", "content": content}
    )


def user_line(uuid: str, secs: int, text: str) -> str:
    return transcript_line(uuid, secs, type="user", message={"role": "user", "content": text})


def write_transcript(root: Path) -> Path:
    path = root / "proj" / f"{SESSION}.jsonl"
    path.parent.mkdir(parents=True)
    lines = [
        user_line("u0", 0, "one"),
        assistant_line(
            "a0",
            1,
            [
                {"type": "text", "text": "working"},
                {
                    "type": "tool_use",
                    "id": "t1",
                    "name": "Edit",
                    "input": {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2"},
                },
            ],
        ),
        user_line("u1", 2, "two"),
        assistant_line(
            "a1", 3, [{"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "uv run pytest"}}]
        ),
        user_line("u2", 4, "three"),
        assistant_line("a2", 5, [{"type": "tool_use", "id": "t3", "name": "Edit", "input": EDIT_INPUT}]),
        user_line("u3", 6, "four"),
        assistant_line("a3", 7, [{"type": "text", "text": "done"}]),
    ]
    path.write_text("\n".join(lines) + "\n")
    return path


@pytest.fixture
def projects_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setattr("cc_transcript.discovery.CLAUDE_PROJECTS_DIR", tmp_path)
    return tmp_path


def test_capture_window_builds_refs_previews_and_digests() -> None:
    window = in_memory_window()
    assert window.anchor == ref("a2", "t3")
    assert (window.fidelity, window.preview_chars) == ("full", 50)
    assert window.trigger is not None
    assert window.trigger.role == "user"
    assert window.trigger.refs == (ref("u2"), ref("a2"))
    assert window.trigger.preview == (f"user: three\nEdit /a.py\n- {'o' * 50}…(+70ch)\n+ {'n' * 50}…(+70ch)")
    assert window.trigger.tool_digests == (tool_digest("Edit", EDIT_INPUT),)
    assert [turn_ref.preview.splitlines()[0] for turn_ref in window.before] == ["user: one", "user: two"]
    assert [turn_ref.refs for turn_ref in window.after] == [(ref("u3"), ref("a3"))]


def test_capture_window_clamps_at_session_edges() -> None:
    activity = SessionActivity.from_events(SESSION, session_events())
    window = capture_window(activity, ref("a2", "t3"))
    assert (len(window.before), len(window.after)) == (2, 1)


def test_capture_window_raises_for_unknown_anchor() -> None:
    activity = SessionActivity.from_events(SESSION, session_events())
    with pytest.raises(ValueError, match="anchor"):
        capture_window(activity, ref("compacted-away"))


def test_render_preview_full_fidelity_joins_previews_without_label() -> None:
    window = in_memory_window()
    assert window.trigger is not None
    assert window.render_preview(budget=Budget()) == "\n\n".join(
        turn_ref.preview for turn_ref in (*window.before, window.trigger, *window.after)
    )


def test_render_preview_summary_fidelity_always_labeled() -> None:
    window = in_memory_window(fidelity="summary")
    assert window.render_preview(budget=Budget()).splitlines()[0] == SUMMARY_LABEL
    empty = ContextWindow(
        anchor=ref("a2", "t3"), before=(), trigger=None, after=(), fidelity="summary", preview_chars=200
    )
    assert empty.render_preview(budget=Budget()) == SUMMARY_LABEL


def test_render_preview_clips_each_preview_to_budget() -> None:
    window = in_memory_window()
    first = window.render_preview(budget=Budget(turn_chars=9)).split("\n\n")[0]
    assert first == "user: one…(+46ch)"
    assert all("…(+" in part for part in window.render_preview(budget=Budget(turn_chars=5)).split("\n\n"))


def test_capture_to_json_from_json_round_trip_byte_stable() -> None:
    window = in_memory_window()
    data = window.to_json()
    payload = json.loads(data)
    assert payload["schema"] == "cc-transcript.context/2"
    assert "origin" not in payload
    assert data == json.dumps(payload, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    restored = ContextWindow.from_json(data)
    assert restored == window
    assert restored.to_json() == data


def test_round_trip_preserves_null_trigger_and_empty_refs() -> None:
    window = ContextWindow(
        anchor=ref("a2", "t3"),
        before=(TurnRef(role="user", refs=(), preview="converted prose", tool_digests=()),),
        trigger=None,
        after=(),
        fidelity="summary",
        preview_chars=200,
    )
    data = window.to_json()
    assert "preview_schema" not in data
    assert '"previews"' not in data
    restored = ContextWindow.from_json(data)
    assert restored == window
    assert restored.to_json() == data


def ask_transcript_bytes() -> bytes:
    questions = [
        {
            "question": "Which adapter?",
            "header": "Adapter",
            "multiSelect": True,
            "options": [{"label": "Storage (Recommended)"}, {"label": "Memory"}],
        }
    ]
    result = {
        "questions": questions,
        "answers": {"Which adapter?": "Storage (Recommended), Memory"},
        "annotations": {"Which adapter?": {"preview": "Storage (Recommended)", "notes": "and never the memory one"}},
    }
    lines = [
        user_line("u0", 0, "set up the adapter"),
        assistant_line("a0", 1, [{"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {"questions": questions}}]),
        transcript_line(
            "u1",
            2,
            type="user",
            message={"role": "user", "content": [{"type": "tool_result", "tool_use_id": "q1", "content": "ok", "is_error": False}]},
            toolUseResult=result,
        ),
        user_line("u2", 3, "thanks"),
    ]
    return ("\n".join(lines) + "\n").encode("utf-8")


def test_capture_attaches_typed_previews() -> None:
    window = in_memory_window()
    assert window.preview_schema == PREVIEW_SCHEMA
    assert window.trigger is not None
    previews = window.trigger.previews
    assert previews is not None
    assert previews[0] == TextPreview(text="three")
    assert isinstance(previews[1], ToolCallPreview)
    assert previews[1].name == "Edit"
    assert previews[1].digest == tool_digest("Edit", EDIT_INPUT)
    assert previews[1].summary.startswith("Edit /a.py")
    # The rendered string preview still rides alongside for legacy readers.
    assert window.trigger.preview.startswith("user: three")


def test_ask_user_question_preview_covers_cc_steer_fields() -> None:
    activity = SessionActivity.from_events(SESSION, parse_events_from_bytes(ask_transcript_bytes()))
    window = capture_window(activity, ref("a0", "q1"), before=0, after=0, preview_chars=200)
    assert window.trigger is not None
    ask = next(p for p in window.trigger.previews or () if isinstance(p, AskUserQuestionPreview))
    round_ = ask.questions[0]
    assert (round_.question, round_.header, round_.multi_select) == ("Which adapter?", "Adapter", True)
    assert round_.labels == ("Storage (Recommended)", "Memory")
    # cc-steer derives "recommended" from the label suffix; it survives verbatim.
    assert [label for label in round_.labels if label.endswith(" (Recommended)")] == ["Storage (Recommended)"]
    assert ask.selections == {"Which adapter?": "Storage (Recommended), Memory"}
    assert ask.notes == {"Which adapter?": "and never the memory one"}


def test_from_json_rejects_unknown_preview_schema() -> None:
    tampered = in_memory_window().to_json().replace(PREVIEW_SCHEMA, "cc-transcript.preview/2")
    with pytest.raises(SchemaError):
        ContextWindow.from_json(tampered)


@pytest.mark.parametrize(
    "data",
    [
        pytest.param("[]", id="non-object"),
        pytest.param('{"schema":"cc-transcript.context/3"}', id="wrong-version"),
        pytest.param('{"anchor":null}', id="missing-schema"),
    ],
)
def test_from_json_rejects_unknown_schema(data: str) -> None:
    with pytest.raises(SchemaError):
        ContextWindow.from_json(data)


def test_hydrate_resolves_full_turns_beyond_preview_budget(projects_root: Path) -> None:
    write_transcript(projects_root)
    activity = anyio.run(partial(SessionActivity.from_session, SESSION))
    window = capture_window(activity, activity.turns[2].tool_uses[0].ref, before=2, after=1, preview_chars=50)
    assert window.trigger is not None
    assert LONG_OLD not in window.trigger.preview
    hydrated = anyio.run(window.hydrate)
    assert hydrated is not None
    assert hydrated.window == window
    assert [turn.prompt for turn in hydrated.turns] == ["one", "two", "three", "four"]
    full = hydrated.render(budget=Budget())
    assert LONG_OLD in full
    assert LONG_NEW in full
    assert "user: three" in full


def test_hydrate_none_once_transcript_deleted_and_previews_survive(projects_root: Path) -> None:
    path = write_transcript(projects_root)
    activity = anyio.run(partial(SessionActivity.from_session, SESSION))
    window = capture_window(activity, activity.turns[2].tool_uses[0].ref, before=2, after=1, preview_chars=50)
    path.unlink()
    assert anyio.run(window.hydrate) is None
    preview = replace(window, fidelity="summary").render_preview(budget=Budget())
    assert preview.splitlines()[0] == SUMMARY_LABEL
    assert "user: three" in preview


@pytest.mark.parametrize(
    "tampered",
    [
        pytest.param(
            TurnRef(role="user", refs=(EventRef(SESSION, EventUuid("compacted")),), preview="gone", tool_digests=()),
            id="ref-compacted-away",
        ),
        pytest.param(TurnRef(role="user", refs=(), preview="refless", tool_digests=()), id="empty-refs"),
    ],
)
def test_hydrate_none_when_any_ref_unresolvable(projects_root: Path, tampered: TurnRef) -> None:
    write_transcript(projects_root)
    activity = anyio.run(partial(SessionActivity.from_session, SESSION))
    window = capture_window(activity, activity.turns[2].tool_uses[0].ref, before=2, after=1, preview_chars=50)
    assert anyio.run(replace(window, before=(*window.before, tampered)).hydrate) is None
