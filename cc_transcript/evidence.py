"""Evidence harvest around a feedback anchor — the enrich mechanics.

Pairs candidate "incorrect" edits before an anchor with the corrections that
follow them: same-file session edits ranked by hunk overlap, with a read-only
git-pickaxe fallback for corrections that happened outside the session. An
empty harvest is the legitimate no-code outcome, distinct from
:class:`~cc_transcript.discovery.TranscriptExpiredError`.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from cc_transcript.activity import hunk_overlap
from cc_transcript.corrections import Correction
from cc_transcript.ids import tool_digest
from cc_transcript.models import AssistantEvent, ToolUseBlock
from cc_transcript.tools import Hunk

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path

    from cc_transcript.activity import Edit, SessionActivity
    from cc_transcript.corrections import CorrectionLog, Origin
    from cc_transcript.ids import EventRef

EXTRACTOR_VERSION = 1
"""The deterministic-extraction version; part of every derived artifact's UNIQUE key."""

GIT_TIMEOUT_S = 15


@dataclass(frozen=True, slots=True)
class GitFix:
    """A correction found in git history rather than the session.

    Deliberately not an :class:`~cc_transcript.activity.Edit` — a commit has
    no honest ``EventRef`` or turn coordinates.

    Attributes:
        file_path: The file the commit touched.
        hunks: The commit's before/after hunks for that file.
        commit: The full commit hash.
        committed_at: The commit time, timezone-aware.
    """

    file_path: str
    hunks: tuple[Hunk, ...]
    commit: str
    committed_at: datetime


@dataclass(frozen=True, slots=True)
class CandidatePair:
    """One incorrect-edit candidate and its best-matching correction.

    Provenance is read off the correction's type: an
    :class:`~cc_transcript.activity.Edit` came from the session, a
    :class:`GitFix` from git history, and None means no correction was found.

    Attributes:
        incorrect: The candidate edit under suspicion.
        correction: The best-matching correction, if any.
        overlap: The hunk-overlap score linking the two; 0.0 when
            ``correction`` is None.
    """

    incorrect: Edit
    correction: Edit | GitFix | None
    overlap: float


def match_corrections(activity: SessionActivity, edit: Edit, *, lookahead_turns: int) -> tuple[CandidatePair, ...]:
    """Session corrections for ``edit``, ranked by overlap descending.

    Scans same-file edits after ``edit`` within ``lookahead_turns`` and keeps
    only candidates whose old side overlaps what ``edit`` wrote
    (overlap > 0).
    """
    return tuple(
        sorted(
            (
                CandidatePair(incorrect=edit, correction=candidate, overlap=overlap)
                for candidate in activity.edits_after(
                    edit.ref, file_path=edit.file_path, lookahead_turns=lookahead_turns
                )
                if (overlap := overlap_between(edit.hunks, candidate.hunks)) > 0
            ),
            key=lambda pair: pair.overlap,
            reverse=True,
        )
    )


def git_corrections(repo: Path, hunk: Hunk, *, path: str, since: datetime, max_commits: int = 5) -> tuple[GitFix, ...]:
    """Corrections to ``hunk`` found in ``repo``'s git history.

    Pickaxes (``git log -S``) for the longest non-empty line of ``hunk.new``
    in ``path`` since ``since``, then parses each hit's ``git show`` unified
    diff into hunks. Read-only by construction — ``rev-parse``, ``log``, and
    ``show`` only — and fully guarded: any failure, including git being
    absent, a nonzero exit, or a timeout, yields ``()``.
    """
    line = max((stripped for raw in hunk.new.splitlines() if (stripped := raw.strip())), key=len, default="")
    if not line:
        return ()
    if (inside := run_git(repo, "rev-parse", "--is-inside-work-tree")) is None or inside.strip() != "true":
        return ()
    log = run_git(
        repo,
        "log",
        f"--since={since.isoformat()}",
        f"--max-count={max_commits}",
        f"-S{line}",
        "--format=%H %ct",
        "--",
        path,
    )
    if log is None:
        return ()
    fixes: list[GitFix] = []
    for row in log.splitlines():
        commit, _, committed = row.partition(" ")
        if (diff := run_git(repo, "show", "--format=", "--unified=0", commit, "--", path)) is None:
            return ()
        if hunks := parse_show_hunks(diff):
            fixes.append(
                GitFix(
                    file_path=path,
                    hunks=hunks,
                    commit=commit,
                    committed_at=datetime.fromtimestamp(int(committed), tz=UTC),
                )
            )
    return tuple(fixes)


def harvest_pairs(
    activity: SessionActivity,
    anchor: EventRef,
    *,
    lookback_turns: int = 40,
    lookahead_turns: int = 120,
    max_candidates: int = 12,
    repo: Path | None = None,
) -> tuple[CandidatePair, ...]:
    """Harvests incorrect-edit/correction pairs around ``anchor``.

    Takes up to ``max_candidates`` edits before the anchor, newest first, and
    pairs each with its best session correction. When ``repo`` is given and a
    candidate has no session correction, falls back to git pickaxe on the
    candidate's most distinctive hunk.

    Returns:
        One pair per candidate, newest candidate first. An empty tuple is the
        legitimate no-code outcome — the anchor's window contains no edits —
        distinct from :class:`~cc_transcript.discovery.TranscriptExpiredError`.
    """
    return tuple(
        harvest_one(activity, edit, lookahead_turns=lookahead_turns, repo=repo)
        for edit in activity.edits_before(anchor, lookback_turns=lookback_turns)[:max_candidates]
    )


def overlap_between(incorrect: tuple[Hunk, ...], correction: tuple[Hunk, ...]) -> float:
    return max((hunk_overlap(a, b) for a in incorrect for b in correction), default=0.0)


def harvest_one(activity: SessionActivity, edit: Edit, *, lookahead_turns: int, repo: Path | None) -> CandidatePair:
    if matches := match_corrections(activity, edit, lookahead_turns=lookahead_turns):
        return matches[0]
    if repo is not None and (fixes := git_corrections(repo, pickaxe_hunk(edit), path=edit.file_path, since=edit.ts)):
        overlap, fix = max(((overlap_between(edit.hunks, fix.hunks), fix) for fix in fixes), key=lambda s: s[0])
        return CandidatePair(incorrect=edit, correction=fix, overlap=overlap)
    return CandidatePair(incorrect=edit, correction=None, overlap=0.0)


def pickaxe_hunk(edit: Edit) -> Hunk:
    return max(
        edit.hunks,
        key=lambda hunk: max((len(line.strip()) for line in hunk.new.splitlines() if line.strip()), default=0),
    )


def run_git(repo: Path, *args: str) -> str | None:
    try:
        proc = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            errors="replace",
            timeout=GIT_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return proc.stdout if proc.returncode == 0 else None


def record_harvest(
    log: CorrectionLog,
    activity: SessionActivity,
    anchor: EventRef,
    pairs: Sequence[CandidatePair],
    *,
    source: str,
    extractor_version: int = EXTRACTOR_VERSION,
) -> int:
    """Records ``pairs`` harvested around ``anchor`` into the shared ledger.

    Lowers each :class:`CandidatePair` to a :class:`~cc_transcript.corrections.Correction`
    — resolving the incorrect edit's raw tool call to the cross-language
    ``incorrect_digest`` — and appends it. Idempotent: re-recording the same
    harvest writes nothing new.

    Returns:
        The number of corrections appended this call.
    """
    rows = [
        correction
        for pair in pairs
        if (correction := lower_pair(activity, anchor, pair, source=source, extractor_version=extractor_version))
    ]
    for row in rows:
        log.append(row)
    return len(rows)


def lower_pair(
    activity: SessionActivity, anchor: EventRef, pair: CandidatePair, *, source: str, extractor_version: int
) -> Correction | None:
    if (block := incorrect_block(activity, pair.incorrect)) is None:
        return None
    incorrect_old, incorrect_new = joined_hunks(pair.incorrect.hunks)
    origin, correction_file, correction_old, correction_new, commit = correction_columns(pair.correction)
    return Correction(
        ts_ms=int(pair.incorrect.ts.timestamp() * 1000),
        session_id=anchor.session_id,
        source=source,
        anchor_uuid=anchor.event_uuid,
        incorrect_digest=tool_digest(block.name, block.input),
        incorrect_file=pair.incorrect.file_path,
        incorrect_old=incorrect_old,
        incorrect_new=incorrect_new,
        extractor_version=extractor_version,
        correction_origin=origin,
        correction_file=correction_file,
        correction_old=correction_old,
        correction_new=correction_new,
        correction_commit=commit,
        overlap=pair.overlap,
    )


def incorrect_block(activity: SessionActivity, edit: Edit) -> ToolUseBlock | None:
    if (turn := activity.turn_of(edit.ref)) is None:
        return None
    return next(
        (
            block
            for event in turn.events
            if isinstance(event, AssistantEvent) and event.meta.uuid == edit.ref.event_uuid
            for block in event.blocks
            if isinstance(block, ToolUseBlock) and block.id == edit.ref.tool_use_id
        ),
        None,
    )


def joined_hunks(hunks: Sequence[Hunk]) -> tuple[str, str]:
    return "\n".join(hunk.old for hunk in hunks), "\n".join(hunk.new for hunk in hunks)


def correction_columns(
    correction: Edit | GitFix | None,
) -> tuple[Origin | None, str | None, str | None, str | None, str | None]:
    match correction:
        case None:
            return None, None, None, None, None
        case GitFix(file_path=file_path, hunks=hunks, commit=commit):
            old, new = joined_hunks(hunks)
            return "git", file_path, old, new, commit
        case edit:
            old, new = joined_hunks(edit.hunks)
            return "session", edit.file_path, old, new, None


def parse_show_hunks(diff: str) -> tuple[Hunk, ...]:
    sections: list[tuple[list[str], list[str]]] = []
    for line in diff.splitlines():
        if line.startswith("@@"):
            sections.append(([], []))
        elif not sections or line.startswith(("---", "+++")):
            continue
        elif line.startswith("-"):
            sections[-1][0].append(line[1:])
        elif line.startswith("+"):
            sections[-1][1].append(line[1:])
    return tuple(Hunk("\n".join(old), "\n".join(new)) for old, new in sections)
