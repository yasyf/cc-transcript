"""Generic infrastructure for parsing structured code-review messages.

The concrete review formats are app policy; an app injects its own
:class:`ReviewFormat` sequence into :func:`extract_all`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import re
    from collections.abc import Callable, Iterator, Mapping, Sequence
    from typing import Any

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

    def extract(self, payload: Any) -> tuple[ReviewComment, ...]:
        return tuple(
            review_comment(obj, self.file_keys, self.line_keys, self.comment_keys, self.fix_keys)
            for obj in findings(payload, FINDING_KEYS + self.finding_keys)
            if isinstance(obj, dict)
            if first(obj, self.comment_keys) is not None
        )


def first(obj: Mapping[str, Any], keys: tuple[str, ...]) -> Any:
    return next((obj[key] for key in keys if key in obj and obj[key] is not None), None)


def line_bounds(value: Any) -> tuple[int | None, int | None]:
    match value:
        case int():
            return value, value
        case str() if "-" in value:
            start, _, end = value.partition("-")
            return int(start.strip()), int(end.strip())
        case str() if value.strip().isdigit():
            return (n := int(value.strip())), n
        case _:
            return None, None


def review_comment(
    obj: Mapping[str, Any],
    file_keys: tuple[str, ...],
    line_keys: tuple[str, ...],
    comment_keys: tuple[str, ...],
    fix_keys: tuple[str, ...] = (),
) -> ReviewComment:
    start, end = line_bounds(first(obj, line_keys))
    return ReviewComment(
        file=(file := first(obj, file_keys)) and str(file),
        line_start=start,
        line_end=end,
        comment=" ".join(str(part) for part in (first(obj, comment_keys), first(obj, fix_keys)) if part is not None),
    )


def findings(payload: Any, keys: tuple[str, ...]) -> Iterator[Any]:
    match payload:
        case list():
            yield from payload
        case dict():
            if (nested := first(payload, keys)) is not None and isinstance(nested, list):
                yield from nested
            elif isinstance(result := payload.get("result"), dict):
                yield from findings(result, keys)
            else:
                yield from (
                    item
                    for key, value in payload.items()
                    if key.startswith("confirmed")
                    if isinstance(value, list)
                    for item in value
                )


def extract_structured(
    text: str, structured_formats: Sequence[StructuredFormat]
) -> Iterator[tuple[StructuredFormat, ReviewComment]]:
    """Yields every ``(format, comment)`` JSON-parsed from a finding payload.

    Args:
        text: The raw message text, expected to be a JSON document.
        structured_formats: The structured formats to apply, in order.

    Yields:
        One pair per finding, across all formats. Non-JSON text yields nothing.
    """
    try:
        payload = json.loads(text)
    except (ValueError, TypeError):
        return
    yield from ((fmt, comment) for fmt in structured_formats for comment in fmt.extract(payload))
