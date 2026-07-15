"""The thin plumbing under :func:`parse` and :func:`stream`.

Both entry points run the native parser: :func:`parse` turns one source — a
transcript path or raw JSONL bytes — into a :class:`~cc_transcript.models.Transcript`
view, and :func:`stream` fans a batch of paths across the native parse pool. A
``drop`` spec is applied inside the parser, so dropped events never materialize
as Python objects.
"""

from __future__ import annotations

from collections.abc import Mapping
from pathlib import Path
from typing import TYPE_CHECKING, Any

import orjson

from cc_transcript import _native
from cc_transcript.filterspec import spec_to_json

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator

    from cc_transcript.filterspec import FilterSpec
    from cc_transcript.models import PrintResult, Transcript, TranscriptEvent

STREAM_RECV_BATCH = 32


def stat_mtime(path: Path) -> float | None:
    try:
        return path.stat().st_mtime
    except OSError:
        return None


def parse(source: Path | bytes, *, drop: FilterSpec | None = None) -> Transcript:
    """Parses one transcript into a :class:`~cc_transcript.models.Transcript` view.

    Args:
        source: A transcript file path, or raw JSONL bytes. A path parse
            carries the file's path and mtime on the view; a bytes parse
            has ``path=None``.
        drop: Optional :class:`~cc_transcript.FilterSpec` applied during
            parsing; events it drops never materialize as Python objects.

    Returns:
        The parsed transcript view.

    Raises:
        OSError: When a path source cannot be read.

    Example:
        >>> transcript = parse(path, drop=NOISE_SPEC)
        >>> [event.meta.uuid for event in transcript.events]
    """
    spec_json = spec_to_json(drop) if drop is not None else None
    match source:
        case bytes():
            return _native.parse_bytes(source, spec_json)
        case Path():
            native = _native.stream_parse([(str(source), source.stat().st_mtime)], 1, spec_json)
            if (result := native.recv()) is None:
                raise OSError(f"unreadable transcript: {source}")
            return result


def stream(paths: Iterable[Path], *, drop: FilterSpec | None = None, prefetch: int = 4) -> Iterator[Transcript]:
    """Streams parsed transcripts for ``paths`` off the native parse pool.

    Files parse in parallel with ``prefetch`` results buffered ahead of the
    consumer; an unreadable file — including one pruned between discovery
    and parse — is skipped without disturbing the rest of the batch. Order
    follows parse completion, not the input order.

    Args:
        paths: The transcript files to parse.
        drop: Optional :class:`~cc_transcript.FilterSpec` applied during
            parsing; events it drops never materialize as Python objects.
        prefetch: Parsed files to hold ready ahead of the consumer.

    Yields:
        One :class:`~cc_transcript.models.Transcript` per readable input path.

    Example:
        >>> for transcript in stream(discover(), drop=NOISE_SPEC):
        ...     print(transcript.path, len(transcript.events))
    """
    spec_json = spec_to_json(drop) if drop is not None else None
    targets = [(str(path), mtime) for path in paths if (mtime := stat_mtime(path)) is not None]
    native = _native.stream_parse(targets, prefetch, spec_json)
    while batch := native.recv_many(STREAM_RECV_BATCH):
        yield from batch


def parse_event(data: Mapping[str, Any]) -> TranscriptEvent | None:
    """Parse one decoded transcript-line mapping into its typed event view.

    Serializes ``data`` and parses it through the native backend — the same path
    :func:`parse_events_from_bytes` uses — so the result is a lazy view over a
    single-entry parse. A structurally malformed line raises ``KeyError`` /
    ``ValueError`` (a missing ``type``, a missing required field); an unmodeled
    ``type`` yields an :class:`~cc_transcript.models.OtherEvent`; a line the
    tolerant parser drops (a below-``MINYEAR`` timestamp) reads as None.
    """
    events = _native.parse_bytes(orjson.dumps(data if isinstance(data, dict) else dict(data))).events
    return events[0] if events else None


def parse_events_from_bytes(raw: bytes) -> list[TranscriptEvent]:
    return list(_native.parse_bytes(raw).events)


def parse_print_result(raw: bytes) -> PrintResult:
    """Parse a 'claude -p --output-format json' payload into a :class:`~cc_transcript.models.PrintResult`.

    Args:
        raw: The raw bytes of the JSON array claude -p emits.

    Returns:
        The parsed -p (print mode) result: billing, usage, structured output, init
        snapshot, and the conversational messages.
    """
    return _native.parse_print_result(raw)
