"""Parity + drift guards for the dual-backend lexicon.

- The Rust (udpipe) path reproduces the spaCy path's *filter decisions* over a
  frozen adversarial fixture — the gate that proves sentiment scores don't shift.
  The fixture is routed through the same ``JUNK_USER_MESSAGE_RE`` the sentiment
  engine applies, so protocol wrappers drop before scoring and only genuine prose
  reaches the comparison. Skips when the UDPipe model can't be fetched (offline) or
  spaCy isn't installed.
- Single source: the Rust-embedded overrides equal `Lexicon.DOMAIN_OVERRIDES`, and
  the generated `cc_transcript/sentiment/data/*.tsv` still match the installed
  `afinn` + overrides.
"""

from __future__ import annotations

import importlib.util
import warnings
from pathlib import Path

import anyio
import pytest

from cc_transcript.filterspec import (
    JUNK_USER_MESSAGE_RE,
    MILD_IMPATIENCE_GROUPS,
    SHORT_MESSAGE_MAX_WORDS,
    compile_groups,
)
from cc_transcript.sentiment.lexicon import NLP, Lexicon, rust_lexicon

POSITIVE_FLOOR = 3
HOSTILE_FLOOR = 3
MILD_IMPATIENCE_RE = compile_groups(MILD_IMPATIENCE_GROUPS, True)
LEXICON_DATA = Path(__file__).resolve().parent.parent / "cc_transcript" / "sentiment" / "data"

sentiment_extra = importlib.util.find_spec("spacy") is not None and importlib.util.find_spec("afinn") is not None
requires_sentiment = pytest.mark.skipif(not sentiment_extra, reason="spaCy/afinn ([sentiment]) not installed")

# Genuine-prose fixture for the spaCy/Rust filter-parity comparison, keyed by the
# tokenizer/lemmatizer divergence source each group probes. Every entry survives
# JUNK_USER_MESSAGE_RE; the short subset drives the positive-axis check and the mild
# subset the negative-axis check, mirroring the PositiveClamp and mild-irritation stages.
ADVERSARIAL_USER_TEXTS: dict[str, tuple[str, ...]] = {
    "positive": (
        "this is exactly the fix I wanted",
        "this is incredible work",
        "that was an amazing and wonderful solution",
        "the résumé looks incredible",
    ),
    "negative": (
        "the whole build is completely broken and useless",
        "this is a terrible and horrible regression",
        "the deploy is garbage and the server keeps crashing",
        "the migration script is an absolute nightmare",
    ),
    "neutral": (
        "please update the readme and ship it",
        "add a new column to the users table",
        "the parser reads the transcript into events",
    ),
    "short_boundary": (
        "finally it works",
        "this is broken",
        "great crisp work",
        "incredible smooth ship",
        "absolutely terrible garbage",
        "it is amazing",
        "totally useless nonsense",
        "done",
    ),
    "unicode": (
        "café finally works",
        "naïve fix broke everything",
        "café works perfectly",
    ),
    "mild_impatience": (
        "it's broken yet again",
        "the deploy failed once again",
        "flaky and broken for the third time",
        "this terrible bug is back yet again",
        "still broken once again after the fix",
    ),
    "empty_ish": (
        "",
        "   ",
        "...",
    ),
}

# Protocol wrappers the sentiment junk filter drops before scoring. The tag-glued
# 'Login successful' case diverges between spaCy and udpipe, so it must never score.
WRAPPER_NOISE_TEXTS: tuple[str, ...] = (
    "<local-command-stdout>Login successful</local-command-stdout>",
    "<local-command-stderr>fatal: not a git repo</local-command-stderr>",
    "<command-name>commit</command-name>",
    "<bash-input>uv run pytest</bash-input>",
)

GENUINE_USER_TEXTS: tuple[str, ...] = tuple(t for group in ADVERSARIAL_USER_TEXTS.values() for t in group)
FIXTURE_USER_TEXTS: tuple[str, ...] = GENUINE_USER_TEXTS + WRAPPER_NOISE_TEXTS


def parse_tsv(name: str) -> dict[str, int]:
    text = (LEXICON_DATA / name).read_text(encoding="utf-8")
    return {w: int(s) for w, _, s in (line.partition("\t") for line in text.splitlines()) if s}


def test_embedded_overrides_match_python_source() -> None:
    rust = rust_lexicon()
    if rust is None:
        pytest.skip("Rust lexicon (udpipe model) unavailable")
    assert dict(rust.lexicon_overrides()) == Lexicon.DOMAIN_OVERRIDES


def test_generated_overrides_tsv_matches_source() -> None:
    assert parse_tsv("domain_overrides.tsv") == Lexicon.DOMAIN_OVERRIDES


@requires_sentiment
def test_generated_afinn_tsv_matches_installed_package() -> None:
    warnings.simplefilter("ignore", SyntaxWarning)
    from afinn import Afinn

    afinn = Afinn(language="en", emoticons=False)
    expected = {word: int(score) for word, score in afinn._dict.items() if " " not in word}  # noqa: SLF001
    assert parse_tsv("afinn-en-165.tsv") == expected


@pytest.mark.parametrize("text", WRAPPER_NOISE_TEXTS)
def test_wrapper_noise_filtered_before_scoring(text: str) -> None:
    assert JUNK_USER_MESSAGE_RE.search(text) is not None


@requires_sentiment
def test_rust_filter_decisions_match_spacy(monkeypatch: pytest.MonkeyPatch) -> None:
    rust = rust_lexicon()
    if rust is None:
        pytest.skip("Rust lexicon (udpipe model) unavailable")
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    try:
        anyio.run(NLP.ensure_ready)
        anyio.run(Lexicon.ensure_ready)
    except RuntimeError:
        pytest.skip("en_core_web_sm not available")

    nlp = NLP.get()
    assert nlp is not None

    def spacy_hit(text: str, floor: int, *, want_negative: bool) -> bool:
        if want_negative:
            return any(Lexicon.polarity(t.lemma_) <= -floor for t in nlp(text) if t.is_alpha)
        return any(Lexicon.polarity(t.lemma_) >= floor for t in nlp(text) if t.is_alpha)

    prose = [t for t in FIXTURE_USER_TEXTS if not JUNK_USER_MESSAGE_RE.search(t)]
    assert prose == list(GENUINE_USER_TEXTS), "junk filter must drop the wrappers and keep every genuine-prose entry"

    short = [t for t in prose if len(t.split()) <= SHORT_MESSAGE_MAX_WORDS]
    mild = [t for t in prose if MILD_IMPATIENCE_RE.search(t)]
    assert short and mild, "frozen fixture must exercise both the short and mild subsets"

    pos_bad = [
        t
        for t in short
        if rust.lexicon_has_hit(t, POSITIVE_FLOOR, False) != spacy_hit(t, POSITIVE_FLOOR, want_negative=False)
    ]
    hos_bad = [
        t
        for t in mild
        if rust.lexicon_has_hit(t, HOSTILE_FLOOR, True) != spacy_hit(t, HOSTILE_FLOOR, want_negative=True)
    ]
    assert not pos_bad, f"positive(short) divergences: {pos_bad[:5]}"
    assert not hos_bad, f"hostile(mild) divergences: {hos_bad[:5]}"
