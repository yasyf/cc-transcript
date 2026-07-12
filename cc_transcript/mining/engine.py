"""The mining entry point over the Rust executor.

:func:`mine_signals` takes RAW transcript bytes plus a
:class:`~cc_transcript.mining.spec.MiningSpec` and returns
:class:`~cc_transcript.mining.signals.MiningSignal` objects. The Rust backend parses
and detects in one pass — invoking any
:class:`~cc_transcript.mining.spec.CallableReviewFormat` through a pyo3 callback
side-channel — and :func:`rehydrate_signal` rebuilds the returned dicts.
"""

from __future__ import annotations

from datetime import datetime
from typing import TYPE_CHECKING, Any

from cc_transcript import _parser_rs
from cc_transcript.mining.confidence import CandidateSignal, Confidence
from cc_transcript.mining.signals import MiningSignal
from cc_transcript.mining.sourcekind import SourceKind
from cc_transcript.mining.spec import mining_spec_to_json
from cc_transcript.models import CcVersion, EventUuid, SessionId

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping

    from cc_transcript.mining.spec import MiningSpec


def mine_signals(raw: bytes, spec: MiningSpec) -> Iterator[MiningSignal]:
    """Mines every :class:`MiningSignal` from raw transcript bytes via the Rust executor.

    The Rust backend parses and detects over ``raw`` in one pass, invoking each
    :class:`~cc_transcript.mining.spec.CallableReviewFormat`'s Python pattern and
    extractor through a positional side-channel. The Rust call runs eagerly, so a
    malformed spec pattern raises here rather than mid-stream.

    Args:
        raw: The raw bytes of a ``.jsonl`` transcript.
        spec: The mining policy: which detectors run, with which scoring, provenance,
            and review-format policy.

    Yields:
        Neutral mined facts, one per recognized transcript shape, in detector order.
    """
    callable_formats = [(fmt.name, fmt.pattern, fmt.extract) for fmt in spec.review.callable_formats]
    payloads = _parser_rs.mine_signals(raw, mining_spec_to_json(spec), callable_formats)
    return (rehydrate_signal(payload) for payload in payloads)


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
