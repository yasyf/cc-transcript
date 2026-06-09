"""Descriptive categories for mined feedback, shared by the fact-detectors."""

from __future__ import annotations

from typing import NewType

SourceKind = NewType("SourceKind", str)
"""A descriptive category for a mined feedback fact.

The four module constants are the common categories the fact-detectors emit; apps
may define their own ``SourceKind`` values for categories the core does not name.
"""

TRANSCRIPT_MESSAGE = SourceKind("transcript_message")
PLAN_REVIEW = SourceKind("plan_review")
INTERRUPT_REJECTION = SourceKind("interrupt_rejection")
REVIEW_COMMENT = SourceKind("review_comment")
