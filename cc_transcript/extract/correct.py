"""The canonical, LLM-grounded correction extractor.

:func:`extract_correction` harvests the candidate incorrect edits around a
feedback anchor (deterministically, via :mod:`cc_transcript.evidence`), then picks
the single edit the feedback faults and appends that one row to the shared
ledger. The pick is an LLM call by default — lifted from cc-pushback's enrich
prompt — and degrades to the best-overlap candidate when no LLM backend is ready
(:func:`usable_backend` returns None). The pick keys to the candidate's index, so
the row carries the real cross-language ``incorrect_digest`` from the harvested
tool call, never reconstructed content.

Behind the ``[llm]`` extra: this module imports pydantic at definition time and
``spawnllm`` lazily, so only LLM-capable consumers import it. A hook that merely
reads :mod:`cc_transcript.corrections` pays nothing.
"""

from __future__ import annotations

import asyncio
from dataclasses import replace
from typing import TYPE_CHECKING

from pydantic import BaseModel

from cc_transcript.evidence import GitFix, harvest_pairs, lower_pair
from cc_transcript.render import Budget, hunk_lines

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path

    from spawnllm import LlmBackend, TModel

    from cc_transcript.activity import SessionActivity
    from cc_transcript.corrections import Correction, CorrectionLog
    from cc_transcript.evidence import CandidatePair
    from cc_transcript.ids import EventRef
    from cc_transcript.tools import Hunk

HUNK_BUDGET = Budget(tool_chars=600)

PICK_PROMPT = """\
You are grounding one piece of developer FEEDBACK on an AI coding assistant's work
in the concrete code change it faults.

Below are candidate edits the assistant made shortly before the feedback, newest
first. Each shows the edit's before (-) and after (+) content and, when one was
found, the correction that later overwrote it — from the same session, or from git
history (marked "git <sha>"). The correction ranked most likely by content overlap
is tagged [likely fix].

Decide which single candidate edit THIS feedback faults, if any:
- candidate: the NUMBER of the candidate the feedback is about, or null when the
  feedback is not about any of these edits (it may fault the approach, a command,
  or work outside this window).
- note: one short clause explaining the call.

=== CANDIDATE EDITS ===
{candidates}
=== FEEDBACK (verbatim) ===
{feedback}"""


class CorrectionPick(BaseModel):
    """The candidate a piece of feedback faults.

    Attributes:
        candidate: The 1-based number of the faulted candidate, or None when the
            feedback is not about any harvested edit.
        note: One short clause explaining the call.
    """

    candidate: int | None
    note: str


def hunk_block(hunks: Sequence[Hunk]) -> str:
    return "\n".join(line for hunk in hunks for line in hunk_lines(hunk.old, hunk.new, budget=HUNK_BUDGET))


def likely_fix(pairs: Sequence[CandidatePair]) -> CandidatePair | None:
    best = max(pairs, key=lambda pair: pair.overlap)
    return best if best.correction is not None and best.overlap > 0 else None


def correction_header(pair: CandidatePair, *, likely: bool) -> str:
    match pair.correction:
        case None:
            return "no correction found"
        case GitFix(commit=commit):
            head = f"correction (git {commit}, overlap {pair.overlap:.2f})"
        case correction:
            turns = correction.turn_index - pair.incorrect.turn_index
            head = f"correction (same session, {turns} turn(s) later, overlap {pair.overlap:.2f})"
    return f"{head} [likely fix]:" if likely else f"{head}:"


def candidate_block(index: int, pair: CandidatePair, *, anchor_turn: int, likely: bool) -> str:
    return "\n".join(
        (
            f"--- candidate {index}: {pair.incorrect.file_path} "
            f"({pair.incorrect.tool}, {anchor_turn - pair.incorrect.turn_index} turn(s) before the feedback) ---",
            hunk_block(pair.incorrect.hunks),
            correction_header(pair, likely=likely),
            *(() if pair.correction is None else (hunk_block(pair.correction.hunks),)),
        )
    )


def build_pick_prompt(feedback: str, pairs: Sequence[CandidatePair], *, anchor_turn: int) -> str:
    likely = likely_fix(pairs)
    return PICK_PROMPT.format(
        candidates="\n\n".join(
            candidate_block(index, pair, anchor_turn=anchor_turn, likely=pair is likely)
            for index, pair in enumerate(pairs, 1)
        ),
        feedback=feedback,
    )


def usable_backend() -> LlmBackend | None:
    """The first ready spawnllm backend for the ``review`` specialty, or None when none is ready.

    Probe once per pass and pass the result to :func:`extract_correction`; the
    status check spawns a subprocess per call.
    """
    from spawnllm import BackendUnavailable, select_backend

    try:
        return select_backend(specialty="review")
    except BackendUnavailable:
        return None


async def choose_pair(
    pairs: Sequence[CandidatePair], *, feedback: str, anchor_turn: int, tier: TModel, backend: LlmBackend | None
) -> CandidatePair | None:
    from spawnllm import extract

    if backend is None:
        return max(pairs, key=lambda pair: pair.overlap)
    pick = await extract(
        build_pick_prompt(feedback, pairs, anchor_turn=anchor_turn), CorrectionPick, backend=backend, model=tier
    )
    return pairs[pick.candidate - 1] if pick.candidate is not None and 1 <= pick.candidate <= len(pairs) else None


async def extract_correction(
    log: CorrectionLog,
    activity: SessionActivity,
    anchor: EventRef,
    *,
    source: str,
    feedback: str,
    repo: Path | None = None,
    tier: TModel = "medium",
    backend: LlmBackend | None = None,
) -> Correction | None:
    """Harvests around ``anchor`` and appends the one correction ``feedback`` faults.

    The faulted candidate is the LLM's pick when ``backend`` is given, else the
    best-overlap candidate. Idempotent per anchor: a no-op when this anchor
    already has a row, so re-runs and overlapping producers never duplicate.
    Returns the appended correction, or None when nothing is harvested or picked.
    """
    if log.for_anchor(anchor.session_id, anchor.event_uuid):
        return None
    pairs = await asyncio.to_thread(harvest_pairs, activity, anchor, repo=repo)
    if not pairs or (turn := activity.turn_of(anchor)) is None:
        return None
    pick = await choose_pair(pairs, feedback=feedback, anchor_turn=turn.index, tier=tier, backend=backend)
    if pick is None or (row := lower_pair(activity, anchor, pick, source=source)) is None:
        return None
    if repo is not None:
        row = replace(row, detail={"repo": str(repo)})
    log.append(row)
    return row
