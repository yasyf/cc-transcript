"""Parity + drift guards for the dual-backend lexicon.

- The Rust (udpipe) path reproduces the spaCy path's *filter decisions* over the
  real corpus — the gate that proves sentiment scores don't shift. Skips when the
  UDPipe model can't be fetched (offline) or spaCy isn't installed.
- Single source: the Rust-embedded overrides equal `Lexicon.DOMAIN_OVERRIDES`, and
  the generated `rust/data/*.tsv` still match the installed `afinn` + overrides.
"""

from __future__ import annotations

import importlib.util
import warnings
from pathlib import Path

import anyio
import pytest

from cc_transcript.filterspec import MILD_IMPATIENCE_GROUPS, SHORT_MESSAGE_MAX_WORDS, compile_groups
from cc_transcript.models import UserEvent
from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.domains.sentiment.lexicon import NLP, Lexicon, rust_lexicon
from tests.test_backend_parity import real_corpus

POSITIVE_FLOOR = 3
HOSTILE_FLOOR = 3
MILD_IMPATIENCE_RE = compile_groups(MILD_IMPATIENCE_GROUPS, True)
RUST_DATA = Path(__file__).resolve().parent.parent / "rust" / "data"

lexicon_extra = importlib.util.find_spec("spacy") is not None and importlib.util.find_spec("afinn") is not None
requires_lexicon = pytest.mark.skipif(not lexicon_extra, reason="spaCy/afinn ([lexicon]) not installed")


def parse_tsv(name: str) -> dict[str, int]:
    text = (RUST_DATA / name).read_text(encoding="utf-8")
    return {w: int(s) for w, _, s in (line.partition("\t") for line in text.splitlines()) if s}


def corpus_user_texts() -> list[str]:
    return [
        e.text
        for path in real_corpus()
        for e in parse_events_from_bytes(path.read_bytes())
        if isinstance(e, UserEvent) and e.text.strip()
    ]


def test_embedded_overrides_match_python_source() -> None:
    rust = rust_lexicon()
    if rust is None:
        pytest.skip("Rust lexicon (udpipe model) unavailable")
    assert dict(rust.lexicon_overrides()) == Lexicon.DOMAIN_OVERRIDES


def test_generated_overrides_tsv_matches_source() -> None:
    assert parse_tsv("domain_overrides.tsv") == Lexicon.DOMAIN_OVERRIDES


@requires_lexicon
def test_generated_afinn_tsv_matches_installed_package() -> None:
    warnings.simplefilter("ignore", SyntaxWarning)
    from afinn import Afinn

    afinn = Afinn(language="en", emoticons=False)
    expected = {word: int(score) for word, score in afinn._dict.items() if " " not in word}  # noqa: SLF001
    assert parse_tsv("afinn-en-165.tsv") == expected


@requires_lexicon
def test_rust_filter_decisions_match_spacy() -> None:
    rust = rust_lexicon()
    if rust is None:
        pytest.skip("Rust lexicon (udpipe model) unavailable")
    anyio.run(NLP.ensure_ready)
    anyio.run(Lexicon.ensure_ready)
    if NLP.get() is None or Lexicon.afinn is None:
        pytest.skip("en_core_web_sm not available")

    nlp = NLP.get()

    def spacy_hit(text: str, floor: int, *, want_negative: bool) -> bool:
        if want_negative:
            return any(Lexicon.polarity(t.lemma_) <= -floor for t in nlp(text) if t.is_alpha)
        return any(Lexicon.polarity(t.lemma_) >= floor for t in nlp(text) if t.is_alpha)

    texts = corpus_user_texts()
    short = [t for t in texts if len(t.split()) <= SHORT_MESSAGE_MAX_WORDS]
    mild = [t for t in texts if MILD_IMPATIENCE_RE.search(t)]
    assert short, "corpus produced no short messages"
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
