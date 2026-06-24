from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING

import anyio
import pytest

pytest.importorskip("spawnllm")

from cc_transcript.activity import SessionActivity  # noqa: E402
from cc_transcript.corrections import CorrectionLog  # noqa: E402
from cc_transcript.extract import CorrectionPick, extract_correction, usable_backend  # noqa: E402
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId  # noqa: E402
from cc_transcript.models import (  # noqa: E402
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    ToolUseBlock,
    UserEvent,
)

if TYPE_CHECKING:
    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("11111111-1111-1111-1111-111111111111")


def meta(uuid: str, *, secs: int = 0) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SESSION,
        timestamp=BASE + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=False,
        is_meta=False,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(uuid: str, text: str, *, secs: int = 0) -> UserEvent:
    return UserEvent(meta=meta(uuid, secs=secs), text=text, blocks=(), interrupted=False)


def assistant(uuid: str, *, blocks: tuple[ContentBlock, ...], secs: int) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-7", text="", blocks=blocks, stop_reason=None, usage=None
    )


def edit(id: str, path: str, old: str, new: str) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name="Edit", input={"file_path": path, "old_string": old, "new_string": new})


def ladder() -> SessionActivity:
    events: tuple[TranscriptEvent, ...] = (
        user("u0", "one"),
        assistant("a0", blocks=(edit("t1", "/a.py", "", "alpha = 1"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", blocks=(edit("t2", "/b.py", "", "zeta = 9"),), secs=3),
        user("u2", "three", secs=4),
        assistant("a2", blocks=(edit("t3", "/a.py", "alpha = 1", "alpha = 2"),), secs=5),
        user("u3", "this regression is wrong", secs=6),
    )
    return SessionActivity.from_events(SESSION, events)


def open_log(tmp_path: Path) -> CorrectionLog:
    return CorrectionLog.open(tmp_path / "corrections.db")


def anchor() -> EventRef:
    return EventRef(SESSION, EventUuid("u3"), None)


def test_deterministic_picks_best_overlap(tmp_path: Path) -> None:
    log = open_log(tmp_path)

    async def go() -> object:
        return await extract_correction(
            log, ladder(), anchor(), source="captain-hook", feedback="wrong", repo=Path("/repo"), backend=None
        )

    row = anyio.run(go)
    assert row is not None
    assert (row.incorrect_file, row.incorrect_new, row.overlap) == ("/a.py", "alpha = 1", 1.0)
    assert (row.correction_origin, row.correction_new) == ("session", "alpha = 2")
    assert row.detail == {"repo": "/repo"} and row.source == "captain-hook"
    assert len(log.for_session(SESSION)) == 1


def test_idempotent_per_anchor(tmp_path: Path) -> None:
    log = open_log(tmp_path)

    async def go() -> tuple[object, object]:
        first = await extract_correction(log, ladder(), anchor(), source="cc-pushback", feedback="x", backend=None)
        second = await extract_correction(log, ladder(), anchor(), source="captain-hook", feedback="x", backend=None)
        return first, second

    first, second = anyio.run(go)
    assert first is not None and second is None
    assert len(log.for_session(SESSION)) == 1


def test_llm_pick_selects_named_candidate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import spawnllm

    async def fake(prompt: str, response_model: object, **_: object) -> CorrectionPick:
        return CorrectionPick(candidate=2, note="the /b.py edit")

    monkeypatch.setattr(spawnllm, "extract", fake)
    log = open_log(tmp_path)

    async def go() -> object:
        return await extract_correction(log, ladder(), anchor(), source="cc-pushback", feedback="x", backend=object())

    row = anyio.run(go)
    assert row is not None and row.incorrect_file == "/b.py"


def test_llm_no_pick_writes_nothing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import spawnllm

    async def fake(prompt: str, response_model: object, **_: object) -> CorrectionPick:
        return CorrectionPick(candidate=None, note="not about a harvested edit")

    monkeypatch.setattr(spawnllm, "extract", fake)
    log = open_log(tmp_path)

    async def go() -> object:
        return await extract_correction(log, ladder(), anchor(), source="cc-pushback", feedback="x", backend=object())

    assert anyio.run(go) is None
    assert log.for_session(SESSION) == ()


def test_faked_codex_backend_picks_candidate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from spawnllm.backends.codex import CodexCliBackend
    from spawnllm.proc import RunResult

    monkeypatch.setattr(CodexCliBackend, "schema_for", lambda self, model: "{}")

    async def fake_capture(argv: list[str], **_: object) -> RunResult:
        Path(argv[argv.index("-o") + 1]).write_text('{"candidate": 2, "note": "the /b.py edit"}')
        return RunResult("log", "", 0)

    monkeypatch.setattr("spawnllm.backends.base.acapture_cli", fake_capture)
    log = open_log(tmp_path)

    async def go() -> object:
        return await extract_correction(
            log, ladder(), anchor(), source="cc-pushback", feedback="x", backend=CodexCliBackend()
        )

    row = anyio.run(go)
    assert row is not None and row.incorrect_file == "/b.py"


def test_faked_codex_backend_failure_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import spawnllm
    from spawnllm.backends.codex import CodexCliBackend
    from spawnllm.proc import RunResult

    monkeypatch.setattr(CodexCliBackend, "schema_for", lambda self, model: "{}")

    async def fake_capture(argv: list[str], **_: object) -> RunResult:
        return RunResult("", "codex: not found", 127)

    monkeypatch.setattr("spawnllm.backends.base.acapture_cli", fake_capture)
    log = open_log(tmp_path)

    async def go() -> object:
        return await extract_correction(
            log, ladder(), anchor(), source="cc-pushback", feedback="x", backend=CodexCliBackend()
        )

    with pytest.raises(spawnllm.BackendCallError):
        anyio.run(go)


def test_usable_backend_none_when_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    import spawnllm

    def raise_unavailable(**_: object) -> object:
        raise spawnllm.BackendUnavailable("none")

    monkeypatch.setattr(spawnllm, "select_backend", raise_unavailable)
    assert usable_backend() is None


def test_usable_backend_returns_ready(monkeypatch: pytest.MonkeyPatch) -> None:
    import spawnllm

    sentinel = object()
    monkeypatch.setattr(spawnllm, "select_backend", lambda **_: sentinel)
    assert usable_backend() is sentinel
