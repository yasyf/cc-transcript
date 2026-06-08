"""Sentiment-scoring tier: conversation buckets and composable score filters.

Built on the core transcript model. The lexicon-dependent filters
(:class:`PositiveClampFilter`, :class:`ImperativeMildIrritationFilter`) require
the optional ``[lexicon]`` extra (spaCy + AFINN); the rest are pure.
"""

from __future__ import annotations

from cc_transcript.sentiment.buckets import (
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
from cc_transcript.sentiment.engine import NOOP_PROGRESS, FilteredEngine, InferenceEngine, ScoreFilter
from cc_transcript.sentiment.lexicon import NLP, Lexicon
from cc_transcript.sentiment.messages import AssistantMessage, BaseMessage, ToolCall, TranscriptMessage, UserMessage
from cc_transcript.sentiment.scorefilters import (
    DEFAULT_FILTERS,
    FRUSTRATION_PATTERN,
    MILD_IMPATIENCE_PATTERN,
    FrustrationFilter,
    ImperativeMildIrritationFilter,
    PositiveClampFilter,
    SessionResumeFilter,
)

__all__ = [
    "BUCKET_MINUTES",
    "DEFAULT_FILTERS",
    "FRUSTRATION_PATTERN",
    "MILD_IMPATIENCE_PATTERN",
    "MIN_USER_CHARS",
    "MIN_USER_TURNS_PER_SESSION",
    "NLP",
    "NOOP_PROGRESS",
    "AssistantMessage",
    "BaseMessage",
    "BucketIndex",
    "BucketKey",
    "ConversationBucket",
    "ConversationBucketer",
    "FilteredEngine",
    "FrustrationFilter",
    "ImperativeMildIrritationFilter",
    "InferenceEngine",
    "Lexicon",
    "PositiveClampFilter",
    "ScoreFilter",
    "SentimentScore",
    "SessionResumeFilter",
    "ToolCall",
    "TranscriptMessage",
    "UserMessage",
    "extract_bucket_keys",
]
