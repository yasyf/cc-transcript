from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timedelta
from typing import NamedTuple, NewType

from cc_transcript.models import SessionId
from cc_transcript.sentiment.messages import AssistantMessage, TranscriptMessage, UserMessage

BucketIndex = NewType("BucketIndex", int)
SentimentScore = NewType("SentimentScore", int)

BUCKET_MINUTES = 3
MIN_USER_TURNS_PER_SESSION = 2
MIN_USER_CHARS = 5


class ConversationBucket(NamedTuple):
    """A session's messages grouped into one fixed-width time window — the unit that gets scored."""

    session_id: SessionId
    bucket_index: BucketIndex
    bucket_start: datetime
    messages: tuple[TranscriptMessage, ...]


class BucketKey(NamedTuple):
    """Stable identity of a :class:`ConversationBucket`: its session and bucket index."""

    session_id: SessionId
    bucket_index: BucketIndex


class ConversationBucketer:
    """Groups transcript messages into per-session, time-aligned buckets worth scoring.

    Sessions below ``MIN_USER_TURNS_PER_SESSION`` and windows lacking a substantive user turn or
    any assistant turn are dropped.
    """

    @staticmethod
    def align_to_bucket(ts: datetime) -> datetime:
        return ts.replace(
            minute=(ts.minute // BUCKET_MINUTES) * BUCKET_MINUTES,
            second=0,
            microsecond=0,
        )

    @classmethod
    def bucket_messages(cls, messages: list[TranscriptMessage]) -> list[ConversationBucket]:
        by_session: dict[SessionId, list[TranscriptMessage]] = defaultdict(list)
        for msg in messages:
            by_session[msg.session_id].append(msg)

        buckets: list[ConversationBucket] = []
        for session_id, session_msgs in by_session.items():
            if sum(1 for m in session_msgs if isinstance(m, UserMessage)) < MIN_USER_TURNS_PER_SESSION:
                continue
            session_msgs.sort(key=lambda m: m.timestamp)
            session_start = cls.align_to_bucket(session_msgs[0].timestamp)

            grouped: dict[int, list[TranscriptMessage]] = defaultdict(list)
            for msg in session_msgs:
                idx = int((msg.timestamp - session_start) // timedelta(minutes=BUCKET_MINUTES))
                grouped[idx].append(msg)

            for idx, bucket_msgs in sorted(grouped.items()):
                if not any(isinstance(m, UserMessage) and len(m.content) >= MIN_USER_CHARS for m in bucket_msgs):
                    continue
                if not any(isinstance(m, AssistantMessage) for m in bucket_msgs):
                    continue
                bucket_start = session_start + timedelta(minutes=BUCKET_MINUTES * idx)
                buckets.append(
                    ConversationBucket(
                        session_id=session_id,
                        bucket_index=BucketIndex(idx),
                        bucket_start=bucket_start,
                        messages=tuple(bucket_msgs),
                    )
                )

        return buckets


def extract_bucket_keys(messages: list[TranscriptMessage]) -> list[BucketKey]:
    """Returns the :class:`BucketKey` of every scorable bucket in ``messages``."""
    return [
        BucketKey(session_id=b.session_id, bucket_index=b.bucket_index)
        for b in ConversationBucketer.bucket_messages(messages)
    ]
