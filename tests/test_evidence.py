from __future__ import annotations

import shutil
import subprocess
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING

import pytest

from cc_transcript.activity import Edit, SessionActivity
from cc_transcript.corrections import CorrectionLog
from cc_transcript.evidence import (
    CandidatePair,
    GitFix,
    git_corrections,
    harvest_pairs,
    match_corrections,
    parse_show_hunks,
    record_harvest,
)
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolUseId, tool_digest
from cc_transcript.models import AssistantEvent, CcVersion, ContentBlock, EntryMeta, ToolUseBlock, UserEvent
from cc_transcript.tools import Hunk

if TYPE_CHECKING:
    from collections.abc import Callable
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("11111111-1111-1111-1111-111111111111")
INCORRECT_LINE = "total = compute_total(rows)"
FIXED_LINE = "total = compute_grand_total(rows)"

needs_git = pytest.mark.skipif(shutil.which("git") is None, reason="git not installed")


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


def assistant(uuid: str, *, blocks: tuple[ContentBlock, ...] = (), secs: int = 0) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-7", text="", blocks=blocks, stop_reason=None, usage=None
    )


def edit(id: str, path: str, old: str, new: str) -> ToolUseBlock:
    return ToolUseBlock(id=ToolUseId(id), name="Edit", input={"file_path": path, "old_string": old, "new_string": new})


def ref(uuid: str, tool_use_id: str | None = None) -> EventRef:
    return EventRef(SESSION, EventUuid(uuid), ToolUseId(tool_use_id) if tool_use_id else None)


def activity(*events: TranscriptEvent) -> SessionActivity:
    return SessionActivity.from_events(SESSION, events)


def edit_of(act: SessionActivity, tool_use_id: str) -> Edit:
    return next(e for e in act.edits if e.ref.tool_use_id == tool_use_id)


def correction_id(pair: CandidatePair) -> str | None:
    assert isinstance(pair.correction, Edit)
    return pair.correction.ref.tool_use_id


def correction_ladder() -> SessionActivity:
    return activity(
        user("u0", "write it"),
        assistant("a0", blocks=(edit("t1", "/a.py", "", "alpha = 1\nbeta = 2"),), secs=1),
        user("u1", "partial fix", secs=2),
        assistant("a1", blocks=(edit("t2", "/a.py", "alpha = 1", "alpha = 10"),), secs=3),
        user("u2", "full fix", secs=4),
        assistant("a2", blocks=(edit("t3", "/a.py", "alpha = 1\nbeta = 2", "rewritten"),), secs=5),
        user("u3", "unrelated", secs=6),
        assistant("a3", blocks=(edit("t4", "/a.py", "gamma = 3", "gamma = 4"),), secs=7),
        assistant("a4", blocks=(edit("t5", "/b.py", "alpha = 1", "alpha = 99"),), secs=8),
    )


def test_match_corrections_ranks_overlapping_same_file_edits_descending() -> None:
    act = correction_ladder()
    incorrect = edit_of(act, "t1")
    pairs = match_corrections(act, incorrect, lookahead_turns=10)
    assert [(correction_id(p), p.overlap) for p in pairs] == [("t3", 1.0), ("t2", 0.5)]
    assert all(p.incorrect == incorrect for p in pairs)


def test_match_corrections_respects_lookahead() -> None:
    act = correction_ladder()
    near = match_corrections(act, edit_of(act, "t1"), lookahead_turns=1)
    assert [correction_id(p) for p in near] == ["t2"]


def test_harvest_pairs_pairs_every_candidate_newest_first() -> None:
    act = activity(
        user("u0", "one"),
        assistant("a0", blocks=(edit("t1", "/a.py", "", "alpha = 1"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", blocks=(edit("t2", "/b.py", "", "zeta = 9"),), secs=3),
        user("u2", "three", secs=4),
        assistant("a2", blocks=(edit("t3", "/a.py", "alpha = 1", "alpha = 2"),), secs=5),
        user("u3", "anchor turn", secs=6),
    )
    pairs = harvest_pairs(act, ref("u3"))
    assert [p.incorrect.ref.tool_use_id for p in pairs] == ["t3", "t2", "t1"]
    by_candidate = {p.incorrect.ref.tool_use_id: p for p in pairs}
    assert by_candidate[ToolUseId("t3")] == CandidatePair(incorrect=edit_of(act, "t3"), correction=None, overlap=0.0)
    assert by_candidate[ToolUseId("t2")].correction is None
    assert by_candidate[ToolUseId("t1")].correction == edit_of(act, "t3")
    assert by_candidate[ToolUseId("t1")].overlap == 1.0


def test_harvest_pairs_caps_candidates_and_handles_empty_windows() -> None:
    act = activity(
        user("u0", "one"),
        assistant("a0", blocks=(edit("t1", "/a.py", "", "alpha = 1"),), secs=1),
        assistant("a1", blocks=(edit("t2", "/a.py", "alpha = 1", "alpha = 2"),), secs=2),
        user("u1", "anchor", secs=3),
    )
    assert [p.incorrect.ref.tool_use_id for p in harvest_pairs(act, ref("u1"), max_candidates=1)] == ["t2"]
    assert harvest_pairs(act, ref("u0")) == ()
    assert harvest_pairs(act, ref("compacted-away")) == ()


def test_parse_show_hunks_splits_sections_and_skips_headers() -> None:
    diff = (
        "diff --git a/src/app.py b/src/app.py\n"
        "index 1111111..2222222 100644\n"
        "--- a/src/app.py\n"
        "+++ b/src/app.py\n"
        "@@ -1,1 +1,1 @@\n"
        f"-{INCORRECT_LINE}\n"
        f"+{FIXED_LINE}\n"
        "@@ -9,0 +10,1 @@ def trailer():\n"
        "+added = True\n"
        "\\ No newline at end of file\n"
    )
    assert parse_show_hunks(diff) == (Hunk(INCORRECT_LINE, FIXED_LINE), Hunk("", "added = True"))


def git(repo: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)


def fixed_repo(tmp_path: Path) -> tuple[Path, Path]:
    repo = tmp_path / "repo"
    source = repo / "src" / "app.py"
    source.parent.mkdir(parents=True)
    git(tmp_path, "init", "-q", "repo")
    git(repo, "config", "user.email", "t@example.com")
    git(repo, "config", "user.name", "Tester")
    git(repo, "config", "commit.gpgsign", "false")
    source.write_text(f"def main():\n    {INCORRECT_LINE}\n    return total\n")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "add app")
    source.write_text(f"def main():\n    {FIXED_LINE}\n    return total\n")
    git(repo, "commit", "-q", "-am", "fix total")
    return repo, source


@needs_git
def test_git_corrections_pickaxes_the_fix_commit(tmp_path: Path) -> None:
    repo, source = fixed_repo(tmp_path)
    hunk = Hunk("", f"    {INCORRECT_LINE}")
    fixes = git_corrections(repo, hunk, path=str(source), since=datetime(2020, 1, 1, tzinfo=UTC))
    assert [len(fix.commit) for fix in fixes] == [40, 40]
    fix, added = fixes
    assert fix.file_path == str(source)
    assert fix.hunks == (Hunk(f"    {INCORRECT_LINE}", f"    {FIXED_LINE}"),)
    assert fix.committed_at.tzinfo is not None
    assert added.hunks[0].old == ""
    assert INCORRECT_LINE in added.hunks[0].new


@needs_git
@pytest.mark.parametrize(
    ("repo_of", "hunk", "since"),
    [
        pytest.param(lambda t: t / "missing", Hunk("", INCORRECT_LINE), datetime(2020, 1, 1, tzinfo=UTC), id="no_repo"),
        pytest.param(lambda t: t, Hunk("", INCORRECT_LINE), datetime(2020, 1, 1, tzinfo=UTC), id="not_a_work_tree"),
        pytest.param(lambda t: t / "repo", Hunk("", "  \n \n"), datetime(2020, 1, 1, tzinfo=UTC), id="blank_hunk"),
        pytest.param(
            lambda t: t / "repo",
            Hunk("", INCORRECT_LINE),
            datetime.now(UTC) + timedelta(days=1),
            id="since_after_all_commits",
        ),
    ],
)
def test_git_corrections_yields_empty_on_misses_and_failures(
    tmp_path: Path, repo_of: Callable[[Path], Path], hunk: Hunk, since: datetime
) -> None:
    _, source = fixed_repo(tmp_path)
    assert git_corrections(repo_of(tmp_path), hunk, path=str(source), since=since) == ()


@needs_git
def test_harvest_pairs_falls_back_to_git_when_session_has_no_correction(tmp_path: Path) -> None:
    repo, source = fixed_repo(tmp_path)
    act = activity(
        user("u0", "write it"),
        assistant("a0", blocks=(edit("t1", str(source), "", f"    {INCORRECT_LINE}"),), secs=1),
        user("u1", "anchor", secs=2),
    )
    (pair,) = harvest_pairs(act, ref("u1"), repo=repo)
    assert pair.incorrect == edit_of(act, "t1")
    assert isinstance(pair.correction, GitFix)
    assert pair.correction.hunks == (Hunk(f"    {INCORRECT_LINE}", f"    {FIXED_LINE}"),)
    assert pair.overlap == 1.0


@needs_git
def test_harvest_pairs_without_repo_leaves_correction_none(tmp_path: Path) -> None:
    _, source = fixed_repo(tmp_path)
    act = activity(
        user("u0", "write it"),
        assistant("a0", blocks=(edit("t1", str(source), "", f"    {INCORRECT_LINE}"),), secs=1),
        user("u1", "anchor", secs=2),
    )
    (pair,) = harvest_pairs(act, ref("u1"))
    assert pair == CandidatePair(incorrect=edit_of(act, "t1"), correction=None, overlap=0.0)


def session_with_correction() -> SessionActivity:
    return activity(
        user("u0", "one"),
        assistant("a0", blocks=(edit("t1", "/a.py", "", "alpha = 1"),), secs=1),
        user("u1", "two", secs=2),
        assistant("a1", blocks=(edit("t2", "/b.py", "", "zeta = 9"),), secs=3),
        user("u2", "three", secs=4),
        assistant("a2", blocks=(edit("t3", "/a.py", "alpha = 1", "alpha = 2"),), secs=5),
        user("u3", "anchor turn", secs=6),
    )


def test_record_harvest_lowers_session_pairs_with_the_cross_language_digest(tmp_path: Path) -> None:
    act = session_with_correction()
    pairs = harvest_pairs(act, ref("u3"))
    log = CorrectionLog.open(tmp_path / "corrections.db")
    assert record_harvest(log, act, ref("u3"), pairs, source="cc-pushback") == 3
    assert len(log.for_session(SESSION)) == 3

    digest = tool_digest("Edit", {"file_path": "/a.py", "old_string": "", "new_string": "alpha = 1"})
    (row,) = log.by_digest(SESSION, incorrect_digest=digest)
    assert row.incorrect_digest == digest  # parity with what a hook would digest from raw stdin
    assert row.anchor_uuid == EventUuid("u3")
    assert (row.incorrect_file, row.incorrect_old, row.incorrect_new) == ("/a.py", "", "alpha = 1")
    assert row.correction_origin == "session"
    assert (row.correction_old, row.correction_new) == ("alpha = 1", "alpha = 2")
    assert row.overlap == 1.0 and row.correction_commit is None
    assert row.source == "cc-pushback"


def test_record_harvest_is_idempotent(tmp_path: Path) -> None:
    act = session_with_correction()
    pairs = harvest_pairs(act, ref("u3"))
    log = CorrectionLog.open(tmp_path / "corrections.db")
    record_harvest(log, act, ref("u3"), pairs, source="cc-pushback")
    record_harvest(log, act, ref("u3"), pairs, source="cc-pushback")
    assert len(log.for_session(SESSION)) == 3


@needs_git
def test_record_harvest_lowers_git_corrections(tmp_path: Path) -> None:
    repo, source = fixed_repo(tmp_path)
    act = activity(
        user("u0", "write it"),
        assistant("a0", blocks=(edit("t1", str(source), "", f"    {INCORRECT_LINE}"),), secs=1),
        user("u1", "anchor", secs=2),
    )
    log = CorrectionLog.open(tmp_path / "corrections.db")
    assert record_harvest(log, act, ref("u1"), harvest_pairs(act, ref("u1"), repo=repo), source="cc-pushback") == 1
    (row,) = log.for_session(SESSION)
    assert row.correction_origin == "git"
    assert row.correction_commit is not None and len(row.correction_commit) == 40
    assert FIXED_LINE in (row.correction_new or "")
