from __future__ import annotations

from importlib.resources import files
from typing import ClassVar

from cc_transcript import _parser_rs


def load_polarities(name: str) -> dict[str, int]:
    text = files("cc_transcript.sentiment").joinpath("data", name).read_text(encoding="utf-8")
    return {
        word: int(score)
        for line in text.splitlines()
        for word, sep, score in [line.partition("\t")]
        if sep and not word.startswith("#")
    }


DOMAIN_OVERRIDES: dict[str, int] = load_polarities("domain_overrides.tsv")
AFINN: dict[str, int] = load_polarities("afinn-en-165.tsv")


def tokenize(text: str) -> list[str]:
    """The lowercased surface of every UDPipe token in ``text``, in order.

    The shared tokenizer over the embedded UD-English-EWT model: multi-word tokens
    split (``can't`` → ``ca``, ``n't``) and punctuation surfaces as its own token.
    Executes in Rust. For POS, lemma, offsets, or negation use
    :func:`cc_transcript.nlp.analyze`.

    Example:
        >>> tokenize("LOST losing")
        ['lost', 'losing']
    """
    return _parser_rs.lexicon_tokenize(text)


class Lexicon:
    """Surface-form token polarity: coding-domain overrides layered over AFINN.

    Polarity is looked up on the token *surface* — never a lemma — so
    inflected forms AFINN scores directly (``lost``, ``broken``) keep their
    signal instead of collapsing to a neutral base. ``DOMAIN_OVERRIDES`` (the
    ``cc_transcript/sentiment/data/domain_overrides.tsv`` snapshot) pins
    context-specific terms AFINN mis-scores; AFINN magnitudes below
    ``MIN_MAGNITUDE`` collapse to neutral. Backs the lexicon-bearing score
    stages through :meth:`has_hit`.
    """

    MIN_MAGNITUDE: ClassVar[int] = 2
    FLOOR: ClassVar[int] = 3

    @classmethod
    def polarity(cls, token: str) -> int:
        """The signed polarity of ``token``.

        A domain override when present, else its AFINN score zeroed below
        ``MIN_MAGNITUDE``. ``token`` is a tokenizer surface: already lowercased
        and alphabetic. Executes in Rust.
        """
        return _parser_rs.lexicon_polarity(token)

    @classmethod
    def has_hit(cls, text: str, *, want_negative: bool) -> bool:
        """Whether any token in ``text`` reaches the polarity ``FLOOR``.

        ``<= -FLOOR`` when ``want_negative`` else ``>= FLOOR``. Surface polarity with
        negation sign-flip and no POS gate: every token's surface polarity counts, and
        a negated token's polarity is sign-flipped — so ``isn't great`` reaches the
        negative floor, not the positive one. POS-based suppression is a highlighter
        concern, not a scoring one. Executes in Rust over the UDPipe substrate.
        """
        return _parser_rs.lexicon_has_hit(text, want_negative)
