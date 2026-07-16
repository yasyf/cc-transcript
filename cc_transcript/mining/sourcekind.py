"""Descriptive categories for mined feedback, shared by the fact-detectors."""

from __future__ import annotations

from typing import NewType

from cc_transcript.literals import literal_str

SourceKind = NewType("SourceKind", str)
"""A descriptive category for a mined feedback fact.

The five module constants are the common categories the fact-detectors emit; apps
may define their own ``SourceKind`` values for categories the core does not name.
"""

TRANSCRIPT_MESSAGE = SourceKind(literal_str("mining.TRANSCRIPT_MESSAGE"))
PLAN_REVIEW = SourceKind(literal_str("mining.PLAN_REVIEW"))
INTERRUPT_REJECTION = SourceKind(literal_str("mining.INTERRUPT_REJECTION"))
REVIEW_COMMENT = SourceKind(literal_str("mining.REVIEW_COMMENT"))
QUESTION_ANSWER = SourceKind(literal_str("mining.QUESTION_ANSWER"))
