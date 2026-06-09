# Re-exports preserve the pre-0.6 import path; the sentiment tier now lives at
# cc_transcript.domains.sentiment.
# pyright: reportUnusedImport=false
"""Deprecated shim: the sentiment tier moved to :mod:`cc_transcript.domains.sentiment`."""

from __future__ import annotations

from cc_transcript.domains.sentiment import (
    BUCKET_MINUTES,
    MIN_USER_CHARS,
    MIN_USER_TURNS_PER_SESSION,
    NLP,
    NOOP_PROGRESS,
    AssistantMessage,
    BaseMessage,
    BucketIndex,
    BucketKey,
    ConversationBucket,
    ConversationBucketer,
    FilteredEngine,
    FrustrationShortCircuit,
    InferenceEngine,
    Lexicon,
    MildIrritationDemote,
    PositiveClamp,
    ResumeClamp,
    ScoreSpec,
    ScoreStage,
    SentimentScore,
    ToolCall,
    TranscriptMessage,
    UserMessage,
    build_score_spec,
    clamp_positive,
    clamp_resume,
    demote_mild_irritation,
    extract_bucket_keys,
    flag_frustration,
)
