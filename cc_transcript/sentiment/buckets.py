# Re-exports preserve the pre-0.6 import path; moved to cc_transcript.domains.sentiment.buckets.
# pyright: reportUnusedImport=false
"""Deprecated shim: moved to :mod:`cc_transcript.domains.sentiment.buckets`."""

from __future__ import annotations

from cc_transcript.domains.sentiment.buckets import (
    BUCKET_MINUTES,
    MIN_USER_CHARS,
    MIN_USER_TURNS_PER_SESSION,
    BucketIndex,
    BucketKey,
    ConversationBucket,
    ConversationBucketer,
    SentimentScore,
    extract_bucket_keys,
)
