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
    """Split ``text`` into lowercased maximal runs of alphabetic characters.

    The shared, deterministic tokenizer: each run is a maximal span of
    ``str.isalpha`` characters, lowercased whole-run so context-sensitive cases
    (Greek final-sigma, the German sharp-s) resolve correctly. Executes in Rust
    over a pinned Unicode ``isalpha`` table.

    Example:
        >>> tokenize("LOST losing — can't")
        ['lost', 'losing', 'can', 't']
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

        ``<= -FLOOR`` when ``want_negative`` else ``>= FLOOR``. Tokenizes and
        scores each surface — no lemmatization, no model, fully deterministic.
        Executes in Rust.
        """
        return _parser_rs.lexicon_has_hit(text, want_negative)
