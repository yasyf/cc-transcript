from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING, NamedTuple, NewType

from cc_transcript import _native
from cc_transcript.filterspec import JUNK_USER_MESSAGE_RE as JUNK_USER_MESSAGE_RE
from cc_transcript.models import AssistantEvent, SessionId, UserEvent

if TYPE_CHECKING:
    from collections.abc import Iterable

    from cc_transcript.models import TranscriptEvent

BucketIndex = NewType("BucketIndex", int)
SentimentScore = NewType("SentimentScore", int)

ConversationEvent = UserEvent | AssistantEvent
"""The conversational subset of the event spine that sentiment scoring consumes."""

BUCKET_MINUTES = 3
MIN_USER_TURNS_PER_SESSION = 2
MIN_USER_CHARS = 5


class ConversationBucket(NamedTuple):
    """A session's conversational events grouped into one fixed-width time window — the unit that gets scored."""

    session_id: SessionId
    bucket_index: BucketIndex
    bucket_start: datetime
    events: tuple[ConversationEvent, ...]


class BucketKey(NamedTuple):
    """Stable identity of a :class:`ConversationBucket`: its session and bucket index."""

    session_id: SessionId
    bucket_index: BucketIndex


class ConversationBucketer:
    """Groups conversational transcript events into per-session, time-aligned buckets worth scoring.

    User and assistant events are selected from the stream; system, mode, and
    other events are ignored, and user turns that are protocol noise
    (``JUNK_USER_MESSAGE_RE`` — slash-command wrappers, interrupt markers,
    stop-hook feedback, bash-mode echoes) are dropped before counting so they
    neither reach the model nor pad bucket eligibility. Sessions below
    ``MIN_USER_TURNS_PER_SESSION`` and windows lacking a substantive user turn
    or any assistant turn are dropped. The bucketing runs in the Rust core over
    borrowed event views; this facade rehydrates each window back into the
    caller's own events.
    """

    @classmethod
    def bucket_events(cls, events: Iterable[TranscriptEvent]) -> list[ConversationBucket]:
        """Lifts the conversational events in ``events`` into scorable :class:`ConversationBucket` windows.

        Example:
            >>> bucket_events(parse_events_from_bytes(raw))
            [ConversationBucket(session_id='s', bucket_index=0, ...)]
        """
        events = list(events)
        by_key = {(e.meta.session_id, e.meta.uuid): e for e in events if isinstance(e, UserEvent | AssistantEvent)}
        return [
            ConversationBucket(
                session_id=SessionId(bucket["session_id"]),
                bucket_index=BucketIndex(bucket["bucket_index"]),
                bucket_start=datetime.fromtimestamp(bucket["bucket_start_ms"] / 1000, tz=UTC),
                events=tuple(by_key[bucket["session_id"], uuid] for uuid in bucket["uuids"]),
            )
            for bucket in _native.bucket_events_from_events(events)
        ]


def extract_bucket_keys(events: Iterable[TranscriptEvent]) -> list[BucketKey]:
    """Returns the :class:`BucketKey` of every scorable bucket in ``events``."""
    return [
        BucketKey(session_id=SessionId(bucket["session_id"]), bucket_index=BucketIndex(bucket["bucket_index"]))
        for bucket in _native.bucket_events_from_events(list(events))
    ]
