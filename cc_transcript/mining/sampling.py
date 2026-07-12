"""Deterministic negative-window sampling over a session's completed turns.

:func:`sample_windows` draws "the user did not steer here" negatives: durable
:class:`~cc_transcript.context.ContextWindow` captures anchored on completed
turns, reproducible for a given seed and session, kept clear of known
positives by an exclusion radius. Negative windows carry no trigger — the
sampled turn folds into ``before`` — so they render byte-compatibly with
positive steering windows, whose user-steer trigger is likewise excluded from
model input: both shapes read as the turns up to the moment being judged.
"""

from __future__ import annotations

import random
from dataclasses import replace
from typing import TYPE_CHECKING

from cc_transcript.context import capture_window
from cc_transcript.filterspec import event_meta
from cc_transcript.ids import EventRef

if TYPE_CHECKING:
    from collections.abc import Iterable

    from cc_transcript.activity import SessionActivity, Turn
    from cc_transcript.context import ContextWindow


def sample_windows(
    activity: SessionActivity,
    *,
    n: int,
    exclude: Iterable[EventRef] = (),
    exclusion_radius: int = 6,
    seed: int = 0,
    before: int = 6,
    after: int = 2,
    preview_chars: int = 200,
) -> list[ContextWindow]:
    """Sample up to ``n`` triggerless context windows as steering negatives.

    Each window anchors on a completed turn — the agent acted and the user's
    next prompt was not a steer we know about — and folds that turn into the
    window's context: ``trigger`` is None, ``before`` ends at the sampled
    turn and keeps at most ``before`` turns, and ``after`` is unchanged. The
    ``anchor`` stays the sampled turn's first meta-bearing event, so
    consumers key negatives exactly like positives.

    Candidates are every turn carrying at least one event with resolvable
    meta (the anchor), except the session's final turn, which may still be in
    flight. Every candidate in the ``exclusion_radius`` turns leading up to an
    ``exclude`` ref's turn is dropped; refs that no longer resolve are
    ignored. Sampling is deterministic:
    ``random.Random(f"{seed}:{session_id}")`` draws from the candidates in
    turn order, so one seed always yields the same windows for a session.

    Args:
        activity: The lifted session to sample from.
        n: The maximum number of windows to return.
        exclude: Anchors of known positives to keep clear of.
        exclusion_radius: How many turns before each excluded turn to drop
            (the excluded turn itself included). Turns after an excluded turn
            stay eligible — once the user has steered, letting the agent run
            again is a genuine negative; only the pre-steer approach, which
            positive-window rewinds occupy, is label-conflicted.
        seed: The determinism seed, mixed with the session id.
        before: How many turns each window's folded ``before`` keeps, ending
            at the sampled turn.
        after: How many turns after each sampled turn to capture.
        preview_chars: The per-chunk preview budget persisted on each window.

    Returns:
        The sampled windows, sorted by sampled turn index.
    """
    excluded = {turn.index for ref in exclude if (turn := activity.turn_of(ref)) is not None}
    candidates = [
        (turn.index, anchor)
        for turn in activity.turns[:-1]
        if all(not (0 <= index - turn.index <= exclusion_radius) for index in excluded)
        if (anchor := turn_anchor(turn)) is not None
    ]
    rng = random.Random(f"{seed}:{activity.session_id}")
    chosen = rng.sample(candidates, min(n, len(candidates)))
    return [
        fold_trigger(
            capture_window(activity, anchor, before=before, after=after, preview_chars=preview_chars),
            keep=before,
        )
        for _, anchor in sorted(chosen, key=lambda pair: pair[0])
    ]


def fold_trigger(window: ContextWindow, *, keep: int) -> ContextWindow:
    """Fold ``window``'s trigger into ``before`` — the negative shape has none.

    Returns:
        The window with ``trigger`` None and ``before`` ending at the old
        trigger, truncated to the last ``keep`` turns.
    """
    folded = (*window.before, *(() if window.trigger is None else (window.trigger,)))
    return replace(window, before=folded[-keep:] if keep > 0 else (), trigger=None)


def turn_anchor(turn: Turn) -> EventRef | None:
    """The reference to ``turn``'s first meta-bearing event, or None without one."""
    return next(
        (EventRef(meta.session_id, meta.uuid) for event in turn.events if (meta := event_meta(event)) is not None),
        None,
    )
