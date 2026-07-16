"""The de-noising confidence primitive carried alongside mined feedback facts."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, NewType

from cc_transcript.literals import literal_float

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

Confidence = NewType("Confidence", float)
"""A de-noising score in the closed interval [0, 1]; higher is more trustworthy."""

NONE = Confidence(literal_float("mining.NONE"))
LOW = Confidence(literal_float("mining.LOW"))
MEDIUM = Confidence(0.5)
HIGH = Confidence(0.75)
VERY_HIGH = Confidence(0.95)
NOISE_FLOOR = LOW


@dataclass(frozen=True, slots=True)
class CandidateSignal:
    """A confidence verdict on a mined fact, with the reasons that produced it.

    Attributes:
        confidence: The de-noising score in [0, 1].
        reasons: The short reason codes that justify the score.
        durable: Whether the signal should persist across re-derivation.
    """

    confidence: Confidence
    reasons: tuple[str, ...] = ()
    durable: bool = True


def strong(*reasons: str, durable: bool = True) -> CandidateSignal:
    """Returns a :data:`HIGH`-confidence signal carrying ``reasons``."""
    return CandidateSignal(HIGH, reasons, durable)


def firm(*reasons: str, durable: bool = True) -> CandidateSignal:
    """Returns a :data:`MEDIUM`-confidence signal carrying ``reasons``."""
    return CandidateSignal(MEDIUM, reasons, durable)


def weak(*reasons: str, durable: bool = True) -> CandidateSignal:
    """Returns a :data:`LOW`-confidence signal carrying ``reasons``."""
    return CandidateSignal(LOW, reasons, durable)


def noise(*reasons: str, durable: bool = True) -> CandidateSignal:
    """Returns a :data:`NONE`-confidence signal carrying ``reasons``."""
    return CandidateSignal(NONE, reasons, durable)


def to_payload(signal: CandidateSignal) -> dict[str, Any]:
    return {"confidence": signal.confidence, "reasons": list(signal.reasons), "durable": signal.durable}


def from_payload(data: Mapping[str, Any]) -> CandidateSignal:
    return CandidateSignal(
        confidence=Confidence(data["confidence"]),
        reasons=tuple(data["reasons"]),
        durable=data["durable"],
    )
