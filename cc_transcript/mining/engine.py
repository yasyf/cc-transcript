"""Dual-backend mining entry — the mining analogue of
:func:`~cc_transcript.sentiment.engine.rust_score_backend`.

:func:`rust_mine_backend` resolves the Rust executor when the extension is built and
the :class:`~cc_transcript.mining.spec.MiningSpec` is portable, else None to fall
back to the Python reference. :func:`mine_signals` is the public dual-backend entry:
it takes RAW transcript bytes plus a spec and returns
:class:`~cc_transcript.mining.signals.MiningSignal` objects, routing to the Rust
parse+detect fast path or the Python :func:`~cc_transcript.mining.signals.mine` over
parsed events. The Rust path rehydrates the returned dicts into ``MiningSignal``
objects byte-identical to the Python path.
"""

from __future__ import annotations

import os
from datetime import datetime
from typing import TYPE_CHECKING, Any

from cc_transcript.mining.confidence import CandidateSignal, Confidence
from cc_transcript.mining.signals import MiningSignal, mine
from cc_transcript.mining.sourcekind import SourceKind
from cc_transcript.mining.spec import mining_spec_is_portable, mining_spec_to_json
from cc_transcript.models import CcVersion, EventUuid, SessionId
from cc_transcript.parser import parse_events_from_bytes

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping
    from types import ModuleType

    from cc_transcript.mining.spec import MiningSpec


def rust_mine_backend(spec: MiningSpec) -> ModuleType | None:
    """The Rust mining executor when built and the spec is portable; else None → Python."""
    if os.environ.get("CC_TRANSCRIPT_DISABLE_RUST"):
        return None
    try:
        from cc_transcript import _parser_rs
    except ImportError:
        return None
    if not hasattr(_parser_rs, "mine_signals") or not mining_spec_is_portable(spec):
        return None
    return _parser_rs


def mine_signals(raw: bytes, spec: MiningSpec) -> Iterator[MiningSignal]:
    """Mines every :class:`MiningSignal` from raw transcript bytes via the active backend.

    The Rust backend parses and detects over ``raw`` in one pass when the extension is
    built and ``spec`` is portable; otherwise the bytes are parsed to events in Python
    and the reference :func:`~cc_transcript.mining.signals.mine` runs. Both paths yield
    byte-identical ``MiningSignal`` objects.

    Args:
        raw: The raw bytes of a ``.jsonl`` transcript.
        spec: The mining policy: which detectors run, with which scoring, provenance,
            and review-format policy.

    Yields:
        Neutral mined facts, one per recognized transcript shape, in detector order.
    """
    rust = rust_mine_backend(spec)
    if rust is None:
        yield from mine(parse_events_from_bytes(raw), spec)
        return
    yield from (rehydrate_signal(payload) for payload in rust.mine_signals(raw, mining_spec_to_json(spec)))


def rehydrate_signal(payload: Mapping[str, Any]) -> MiningSignal:
    """Rebuilds a :class:`MiningSignal` from a Rust ``mine_signals`` dict.

    ``occurred_at`` is parsed back from its RFC3339 string and the branded primitives
    are re-wrapped, producing an object byte-identical to the Python reference path.
    """
    return MiningSignal(
        kind=SourceKind(payload["kind"]),
        detector=payload["detector"],
        session_id=SessionId(payload["session_id"]),
        event_index=payload["event_index"],
        event_uuid=EventUuid(payload["event_uuid"]),
        occurred_at=datetime.fromisoformat(payload["occurred_at"]),
        text=payload["text"],
        cc_version=None if (version := payload["cc_version"]) is None else CcVersion(version),
        trigger_index=payload["trigger_index"],
        signal=CandidateSignal(
            Confidence((signal := payload["signal"])["confidence"]), tuple(signal["reasons"]), signal["durable"]
        ),
        lower_bound=payload["lower_bound"],
        evidence=payload["evidence"],
    )
