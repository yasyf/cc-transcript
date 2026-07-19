"""The py-visible codex surface: Transcript.provider, session_info, discovery, and
provider-aware resolution.

The native gateway already lowers codex rollouts transparently (see
``test_codex_probe``); this suite pins the identity, lifecycle, and token-usage
surface the lowered event stream drops, plus the codex fallback leg wired into
``discovery.resolve`` so ``Session.from_id`` and ``SessionActivity.from_session``
work on a codex session id with no changes of their own.
"""

from __future__ import annotations

import shutil
from pathlib import Path

import pytest

from cc_transcript import codex, discovery
from cc_transcript.codex import CodexPendingItem, CodexRollout, CodexSessionInfo, CodexUsage
from cc_transcript.parser import parse
from cc_transcript.query import Session, SessionActivity

CODEX = Path(__file__).resolve().parent / "testdata" / "codex"
CC_FIXTURE = Path(__file__).resolve().parent / "testdata" / "views_edge" / "edge_core.jsonl"

SESSION_303 = "019f67f0-2a3b-7c4d-8e5f-000000000303"


def codex_fixture(tag: str) -> Path:
    return next(CODEX.glob(f"*{tag}.jsonl"))


def test_provider_codex_from_path_and_bytes() -> None:
    fixture = codex_fixture("303")
    assert parse(fixture).provider == "codex"
    assert parse(fixture.read_bytes()).provider == "codex"


def test_provider_claude_from_path_and_bytes() -> None:
    assert parse(CC_FIXTURE).provider == "claude"
    assert parse(CC_FIXTURE.read_bytes()).provider == "claude"


def test_session_info_303_completed_with_exact_usage() -> None:
    assert codex.session_info(codex_fixture("303")) == CodexSessionInfo(
        rollout_thread_id=SESSION_303,
        session_id=SESSION_303,
        parent_thread_id=None,
        forked_from_id=None,
        cwd="/tmp/demo",
        originator="codex_exec",
        cli_version="0.144.5",
        model_provider="openai",
        lifecycle="completed",
        turn_id="019f67f0-0aa1-7000-8000-0000000000c1",
        pending=(),
        last_event_epoch=1784218935,
        usage=CodexUsage(
            input_tokens=1200,
            cached_input_tokens=800,
            output_tokens=240,
            reasoning_output_tokens=128,
            total_tokens=1440,
            model_context_window=272000,
            token_count_events=1,
        ),
    )


def test_session_info_050a_open_with_dangling_call() -> None:
    info = codex.session_info(codex_fixture("050a"))
    assert info.lifecycle == "open"
    assert info.turn_id == "019f6820-0aa1-7000-8000-0000000000e1"
    assert info.pending == (CodexPendingItem("call_Demo0005dang0005dang0005x", "exec", "mid_tool"),)
    assert info.last_event_epoch == 1784220090
    assert info.usage is None


def test_session_info_101_no_instrumentation() -> None:
    info = codex.session_info(codex_fixture("101"))
    assert info.lifecycle == "no_instrumentation"
    assert info.turn_id is None
    assert info.pending == ()
    assert info.rollout_thread_id == "019bd9c0-0a1b-7c2d-8e3f-000000000101"
    assert info.originator == "codex_cli_rs"
    assert info.cli_version == "0.42.0"
    assert info.usage == CodexUsage(None, None, None, None, None, None, 1)


def test_session_info_404_records_parentage() -> None:
    info = codex.session_info(codex_fixture("404"))
    assert info.rollout_thread_id == "019f6800-3b4c-7d5e-9f60-000000000404"
    assert info.session_id == SESSION_303
    assert info.parent_thread_id == SESSION_303
    assert info.forked_from_id == SESSION_303
    assert info.lifecycle == "completed"
    assert info.turn_id == "019f6800-0bb2-7000-8000-0000000000d2"


UUID_1 = "019b7000-0000-7000-8000-000000000001"
UUID_2 = "019b7000-0000-7000-8000-000000000002"
UUID_3 = "019b7000-0000-7000-8000-000000000003"


@pytest.fixture
def codex_tree(tmp_path: Path) -> dict[str, Path]:
    def touch(rel: str) -> Path:
        path = tmp_path / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.touch()
        return path

    tree = {
        "oldest": touch(f"2025/12/31/rollout-2025-12-31T23-59-59-{UUID_1}.jsonl"),
        "twin": touch(f"2026/01/01/rollout-2026-01-01T00-00-01-{UUID_2}.jsonl"),
        "twin_zst": touch(f"2026/01/03/rollout-2026-01-03T09-00-00-{UUID_2}.jsonl.zst"),
        "newest": touch(f"2026/01/02/rollout-2026-01-02T03-04-05-{UUID_3}.jsonl"),
    }
    touch("2026/01/02/notes.jsonl")
    touch("2026/01/02/rollout-2026-01-02T03-04-06-not-a-uuid.jsonl")
    return tree


def test_discover_orders_newest_first(tmp_path: Path, codex_tree: dict[str, Path]) -> None:
    assert codex.discover(tmp_path) == (
        CodexRollout(codex_tree["twin_zst"], UUID_2, True),
        CodexRollout(codex_tree["newest"], UUID_3, False),
        CodexRollout(codex_tree["twin"], UUID_2, False),
        CodexRollout(codex_tree["oldest"], UUID_1, False),
    )


def test_children_of_finds_direct_children(tmp_path: Path) -> None:
    root = tmp_path / "codex"
    date_dir = root / "2026" / "07" / "16"
    date_dir.mkdir(parents=True)
    parent = codex_fixture("303")
    child = codex_fixture("404")
    shutil.copy(parent, date_dir / parent.name)
    child_path = date_dir / child.name
    shutil.copy(child, child_path)

    child_id = "019f6800-3b4c-7d5e-9f60-000000000404"
    assert codex.children_of(SESSION_303, root=root) == (
        CodexRollout(child_path, child_id, False),
    )
    assert codex.children_of(child_id, root=root) == ()


def test_find_transcript_prefers_uncompressed_and_rejects_compressed_only(
    tmp_path: Path, codex_tree: dict[str, Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    assert codex.find_transcript(UUID_2, root=tmp_path) == codex_tree["twin"]
    codex_tree["twin"].unlink()
    assert codex.find_transcript(UUID_2, root=tmp_path) is None
    claude_root = tmp_path / "claude"
    claude_root.mkdir()
    monkeypatch.setattr(codex, "SESSIONS_ROOT", tmp_path)
    monkeypatch.setattr(discovery, "TRANSCRIPT_MEMO", {})
    assert discovery.resolve(UUID_2, root=claude_root) is None
    assert codex.find_transcript(UUID_1, root=tmp_path) == codex_tree["oldest"]


def test_find_transcript_miss_is_none(tmp_path: Path, codex_tree: dict[str, Path]) -> None:
    assert codex.find_transcript("019b7000-0000-7000-8000-000000000099", root=tmp_path) is None


@pytest.fixture
def fake_codex_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    root = tmp_path / "codex"
    dest = root / "2026" / "07" / "16" / codex_fixture("303").name
    dest.parent.mkdir(parents=True)
    shutil.copy(codex_fixture("303"), dest)
    monkeypatch.setattr(codex, "SESSIONS_ROOT", root)
    monkeypatch.setattr(discovery, "TRANSCRIPT_MEMO", {})
    return root


def test_resolve_falls_back_to_codex(tmp_path: Path, fake_codex_root: Path) -> None:
    cc_root = tmp_path / "cc"
    cc_root.mkdir()
    resolved = discovery.resolve(SESSION_303, root=cc_root)
    assert resolved == fake_codex_root / "2026" / "07" / "16" / codex_fixture("303").name


def test_session_from_id_lifts_codex_rollout(tmp_path: Path, fake_codex_root: Path) -> None:
    cc_root = tmp_path / "cc"
    cc_root.mkdir()
    session = Session.from_id(SESSION_303, root=cc_root)
    assert len(session.turns) == 2
    assert [use.call.name for turn in session.turns for use in turn.tool_uses] == ["exec"]


def test_session_activity_from_session_lifts_codex_rollout(tmp_path: Path, fake_codex_root: Path) -> None:
    cc_root = tmp_path / "cc"
    cc_root.mkdir()
    activity = SessionActivity.from_session(SESSION_303, root=cc_root)
    assert len(activity.turns) == 2
    assert [use.call.name for turn in activity.turns for use in turn.tool_uses] == ["exec"]
