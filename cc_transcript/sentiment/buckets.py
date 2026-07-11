from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timedelta
from typing import TYPE_CHECKING, NamedTuple, NewType

from cc_transcript.filterspec import JUNK_USER_MESSAGE_RE
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
    or any assistant turn are dropped.
    """

    @staticmethod
    def align_to_bucket(ts: datetime) -> datetime:
        return ts.replace(
            minute=(ts.minute // BUCKET_MINUTES) * BUCKET_MINUTES,
            second=0,
            microsecond=0,
        )

    @classmethod
    def bucket_events(cls, events: Iterable[TranscriptEvent]) -> list[ConversationBucket]:
        """Lifts the conversational events in ``events`` into scorable :class:`ConversationBucket` windows.

        Example:
            >>> bucket_events(parse_events_from_bytes(raw))
            [ConversationBucket(session_id='s', bucket_index=0, ...)]
        """
        by_session: dict[SessionId, list[ConversationEvent]] = defaultdict(list)
        for event in events:
            match event:
                case UserEvent() if JUNK_USER_MESSAGE_RE.search(event.text):
                    continue
                case UserEvent() | AssistantEvent():
                    by_session[event.meta.session_id].append(event)

        buckets: list[ConversationBucket] = []
        for session_id, session_events in by_session.items():
            if sum(1 for e in session_events if isinstance(e, UserEvent)) < MIN_USER_TURNS_PER_SESSION:
                continue
            session_events.sort(key=lambda e: e.meta.timestamp)
            session_start = cls.align_to_bucket(session_events[0].meta.timestamp)

            grouped: dict[int, list[ConversationEvent]] = defaultdict(list)
            for event in session_events:
                idx = int((event.meta.timestamp - session_start) // timedelta(minutes=BUCKET_MINUTES))
                grouped[idx].append(event)

            for idx, window_events in sorted(grouped.items()):
                if not any(isinstance(e, UserEvent) and len(e.text.strip()) >= MIN_USER_CHARS for e in window_events):
                    continue
                if not any(isinstance(e, AssistantEvent) for e in window_events):
                    continue
                bucket_start = session_start + timedelta(minutes=BUCKET_MINUTES * idx)
                buckets.append(
                    ConversationBucket(
                        session_id=session_id,
                        bucket_index=BucketIndex(idx),
                        bucket_start=bucket_start,
                        events=tuple(window_events),
                    )
                )

        return buckets


def extract_bucket_keys(events: Iterable[TranscriptEvent]) -> list[BucketKey]:
    """Returns the :class:`BucketKey` of every scorable bucket in ``events``."""
    return [
        BucketKey(session_id=b.session_id, bucket_index=b.bucket_index)
        for b in ConversationBucketer.bucket_events(events)
    ]
