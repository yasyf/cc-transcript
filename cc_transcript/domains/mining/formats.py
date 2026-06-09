"""Generic infrastructure for parsing structured code-review messages.

The concrete review formats are app policy; an app injects its own
:class:`ReviewFormat` sequence into :func:`extract_all`.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import re
    from collections.abc import Callable, Iterator, Sequence


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
class ReviewFormat:
    """A named code-review text format with a detector and extractor.

    Attributes:
        name: The format's identifier.
        pattern: A pattern that matches when the format is present in a text.
        extract: Parses a matching text into its review comments.
    """

    name: str
    pattern: re.Pattern[str]
    extract: Callable[[str], tuple[ReviewComment, ...]]


def extract_all(text: str, formats: Sequence[ReviewFormat]) -> Iterator[tuple[ReviewFormat, ReviewComment]]:
    """Yields every ``(format, comment)`` extracted by any matching format.

    Args:
        text: The raw review message text.
        formats: The review formats to try, in order.

    Yields:
        One pair per extracted comment, across all formats whose pattern matches.
    """
    return (
        (fmt, comment)
        for fmt in formats
        if fmt.pattern.search(text)
        for comment in fmt.extract(text)
    )
