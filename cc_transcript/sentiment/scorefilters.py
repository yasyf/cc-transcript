# The lexicon filters call into optional, untyped spaCy/afinn (the [lexicon]
# extra); their unknown-type noise is suppressed here, not project-wide.
# pyright: reportUnknownMemberType=false, reportUnknownVariableType=false, reportUnknownArgumentType=false
from __future__ import annotations

from typing import ClassVar

import anyio

from cc_transcript.filterspec import (
    FRUSTRATION_GROUPS,
    MILD_IMPATIENCE_GROUPS,
    RESUME_PHRASE_SET,
    SHORT_MESSAGE_MAX_WORDS,
    TRAILING_PUNCT,
    compile_groups,
)
from cc_transcript.sentiment.buckets import ConversationBucket, SentimentScore
from cc_transcript.sentiment.engine import ScoreFilter
from cc_transcript.sentiment.lexicon import NLP, Lexicon

FRUSTRATION_PATTERN = compile_groups(FRUSTRATION_GROUPS, True)
MILD_IMPATIENCE_PATTERN = compile_groups(MILD_IMPATIENCE_GROUPS, True)


class FrustrationFilter(ScoreFilter):
    @staticmethod
    def matches_text(text: str) -> bool:
        return FRUSTRATION_PATTERN.search(text) is not None

    @classmethod
    def matched_user_message(cls, bucket: ConversationBucket) -> str | None:
        return next(
            (msg.content for msg in bucket.messages if msg.role == "user" and cls.matches_text(msg.content)),
            None,
        )

    @staticmethod
    def matched_words(bucket: ConversationBucket) -> list[str]:
        return [
            match.group(1).lower().strip()
            for msg in bucket.messages
            if msg.role == "user"
            for match in FRUSTRATION_PATTERN.finditer(msg.content)
        ]

    @classmethod
    def check_frustration(cls, bucket: ConversationBucket) -> bool:
        return cls.matched_user_message(bucket) is not None

    def short_circuit(self, bucket: ConversationBucket) -> SentimentScore | None:
        return SentimentScore(1) if self.check_frustration(bucket) else None


class SessionResumeFilter(ScoreFilter):
    RESUME_PHRASES: ClassVar[frozenset[str]] = RESUME_PHRASE_SET

    @classmethod
    def is_bare_resume(cls, text: str) -> bool:
        return text.strip().rstrip(TRAILING_PUNCT).strip().lower() in cls.RESUME_PHRASES

    @classmethod
    def should_clamp(cls, bucket: ConversationBucket) -> bool:
        return any(msg.role == "user" and cls.is_bare_resume(msg.content) for msg in bucket.messages)

    def post_process(self, bucket: ConversationBucket, score: SentimentScore) -> SentimentScore:
        return SentimentScore(3) if self.should_clamp(bucket) else score


class PositiveClampFilter(ScoreFilter):
    POSITIVE_LEXICON_FLOOR: ClassVar[int] = 3
    MAX_WORDS_FOR_CLAMP: ClassVar[int] = SHORT_MESSAGE_MAX_WORDS

    @classmethod
    def is_short(cls, text: str) -> bool:
        return len(text.split()) <= cls.MAX_WORDS_FOR_CLAMP

    @classmethod
    def has_positive_lexicon(cls, text: str) -> bool:
        if (nlp := NLP.get()) is None or Lexicon.afinn is None:
            return True
        return any(
            Lexicon.polarity(token.lemma_) >= cls.POSITIVE_LEXICON_FLOOR for token in nlp(text) if token.is_alpha
        )

    @classmethod
    def should_clamp_5(cls, bucket: ConversationBucket) -> bool:
        return any(
            msg.role == "user" and cls.is_short(msg.content) and not cls.has_positive_lexicon(msg.content)
            for msg in bucket.messages
        )

    async def prepare(self) -> None:
        async with anyio.create_task_group() as tg:
            tg.start_soon(NLP.ensure_ready)
            tg.start_soon(Lexicon.ensure_ready)

    def post_process(self, bucket: ConversationBucket, score: SentimentScore) -> SentimentScore:
        return SentimentScore(3) if int(score) == 5 and self.should_clamp_5(bucket) else score


class ImperativeMildIrritationFilter(ScoreFilter):
    HOSTILE_LEXICON_FLOOR: ClassVar[int] = -3

    @staticmethod
    def matches_trigger(text: str) -> bool:
        return MILD_IMPATIENCE_PATTERN.search(text) is not None

    @classmethod
    def has_hostile_lexicon(cls, text: str) -> bool:
        if FrustrationFilter.matches_text(text):
            return True
        if (nlp := NLP.get()) is None or Lexicon.afinn is None:
            return True
        return any(Lexicon.polarity(token.lemma_) <= cls.HOSTILE_LEXICON_FLOOR for token in nlp(text) if token.is_alpha)

    @classmethod
    def should_demote(cls, bucket: ConversationBucket) -> bool:
        return any(
            msg.role == "user" and cls.matches_trigger(msg.content) and not cls.has_hostile_lexicon(msg.content)
            for msg in bucket.messages
        )

    async def prepare(self) -> None:
        async with anyio.create_task_group() as tg:
            tg.start_soon(NLP.ensure_ready)
            tg.start_soon(Lexicon.ensure_ready)

    def post_process(self, bucket: ConversationBucket, score: SentimentScore) -> SentimentScore:
        return SentimentScore(2) if int(score) == 1 and self.should_demote(bucket) else score


DEFAULT_FILTERS: tuple[ScoreFilter, ...] = (
    FrustrationFilter(),
    PositiveClampFilter(),
    ImperativeMildIrritationFilter(),
    SessionResumeFilter(),
)


__all__ = [
    "DEFAULT_FILTERS",
    "FRUSTRATION_PATTERN",
    "MILD_IMPATIENCE_PATTERN",
    "FrustrationFilter",
    "ImperativeMildIrritationFilter",
    "PositiveClampFilter",
    "SessionResumeFilter",
]
