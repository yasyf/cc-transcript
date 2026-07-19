"""Test-only re-export shim over :mod:`cc_transcript.synthetic` and the parser facade.

The synthetic transcript builders and :func:`~cc_transcript.parser.parse_events` now
live in the package; this module keeps the historical ``tests.testkit`` import surface
pointing at them, adds ``parse_bytes`` (an alias of
:func:`~cc_transcript.parser.parse_events_from_bytes`), and keeps the local
assert-exactly-one :func:`parse_event` whose drop-is-an-error semantics differ from
:func:`cc_transcript.parser.parse_event`'s None-on-drop.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from cc_transcript.parser import parse_events, parse_events_from_bytes
from cc_transcript.synthetic import (
    BASE,
    Content,
    assistant_line,
    meta_fields,
    mode_line,
    other_line,
    system_line,
    text_block,
    thinking_block,
    tool_result,
    tool_use,
    user_line,
)

if TYPE_CHECKING:
    from collections.abc import Mapping

    from cc_transcript.models import TranscriptEvent

__all__ = [
    "BASE",
    "Content",
    "assistant_line",
    "meta_fields",
    "mode_line",
    "other_line",
    "parse_bytes",
    "parse_event",
    "parse_events",
    "parse_events_from_bytes",
    "system_line",
    "text_block",
    "thinking_block",
    "tool_result",
    "tool_use",
    "user_line",
]

parse_bytes = parse_events_from_bytes


def parse_event(line: Mapping[str, Any]) -> TranscriptEvent:
    """Parses one raw transcript-line dict into its single view event."""
    events = parse_events(line)
    assert len(events) == 1, f"expected 1 parsed event, got {len(events)} for {line!r}"
    return events[0]
