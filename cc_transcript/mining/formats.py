"""Generic infrastructure for parsing structured code-review messages.

The concrete review formats are app policy; an app declares its formats as
:class:`~cc_transcript.mining.spec.RegexReviewFormat`,
:class:`~cc_transcript.mining.spec.CallableReviewFormat`, and
:class:`StructuredFormat` on a :class:`~cc_transcript.mining.spec.ReviewSpec`, which
:func:`~cc_transcript.mining.mine` interprets.
"""

from __future__ import annotations

from dataclasses import dataclass, field

FINDING_KEYS: tuple[str, ...] = ("findings", "bugs", "improvements", "issues", "items", "verdicts")


@dataclass(frozen=True, slots=True)
class ReviewComment:
    """A single inline review comment parsed from a code-review message.

    Attributes:
        file: The file the comment targets, when cited.
        line_start: The first line the comment targets, when cited.
        line_end: The last line the comment targets, when a range is cited.
        comment: The comment's text.
    """

    file: str | None
    line_start: int | None
    line_end: int | None
    comment: str


@dataclass(frozen=True, slots=True)
class StructuredFormat:
    """A named JSON code-review format keyed by caller-supplied field aliases.

    The aliases let an app map its own finding schema onto :class:`ReviewComment`
    without hardcoding key names here. Each alias tuple is tried in order against
    every finding object.

    Attributes:
        name: The format's identifier.
        file_keys: Aliases for the cited file path.
        line_keys: Aliases for the cited line (int, ``"96"``, or ``"24-51"``).
        comment_keys: Aliases for the comment text.
        fix_keys: Aliases for a suggested fix appended to the comment, when present.
        finding_keys: Extra aliases for the finding array, beyond the defaults.
    """

    name: str
    file_keys: tuple[str, ...] = ("file", "path", "file_path")
    line_keys: tuple[str, ...] = ("line", "line_start", "lines")
    comment_keys: tuple[str, ...] = ("comment", "message", "text", "description")
    fix_keys: tuple[str, ...] = field(default=())
    finding_keys: tuple[str, ...] = field(default=())
