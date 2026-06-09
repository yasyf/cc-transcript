# Re-exports establish the domain's public surface; pyright sees them as unused.
# pyright: reportUnusedImport=false
"""Sentiment-scoring domain: conversation buckets and a composable score spec.

Built on the core transcript model. Consumers compose a :class:`ScoreSpec` from the
builders (:func:`flag_frustration`, :func:`clamp_positive`,
:func:`demote_mild_irritation`, :func:`clamp_resume`) and run it via
:class:`FilteredEngine`. The lexicon-backed stages lemmatize with the Rust udpipe
backend when available, falling back to spaCy + AFINN (the ``[sentiment]`` extra).
"""

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
from cc_transcript.domains.sentiment.engine import NOOP_PROGRESS, FilteredEngine, InferenceEngine
from cc_transcript.domains.sentiment.lexicon import NLP, Lexicon
from cc_transcript.domains.sentiment.scorespec import (
    FrustrationShortCircuit,
    MildIrritationDemote,
    PositiveClamp,
    ResumeClamp,
    ScoreSpec,
    ScoreStage,
    build_score_spec,
    clamp_positive,
    clamp_resume,
    demote_mild_irritation,
    flag_frustration,
)
from cc_transcript.messages import AssistantMessage, BaseMessage, ToolCall, TranscriptMessage, UserMessage
