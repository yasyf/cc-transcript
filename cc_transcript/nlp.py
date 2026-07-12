"""The UDPipe-backed NLP substrate: typed tokens with POS, offsets, and negation.

:func:`analyze` runs the embedded UD-English-EWT model (tokenizer, tagger,
lemmatizer, parser) over a text and returns typed :class:`Token` objects. It is
the one surface the sentiment lexicon and highlighter share — the lexicon flips
polarity by negation, the highlighter spans forms by codepoint offset and suppresses
by POS. The model is embedded in the Rust extension at compile time; there is no
runtime download, file read, or cache.
"""

from __future__ import annotations

from dataclasses import dataclass

from cc_transcript import _parser_rs


@dataclass(frozen=True, slots=True)
class Token:
    """One analyzed token: its surface, linguistics, source span, and sentiment.

    Attributes:
        form: The surface text exactly as it appears in the source.
        lower: ``form`` lowercased — the key surface polarity is looked up on.
        lemma: The dictionary form UDPipe assigns (``n't`` and ``cannot`` lemmatize to ``not``).
        upos: The universal part-of-speech tag (``ADJ``, ``VERB``, ``PUNCT``, …).
        start: Codepoint offset of ``form`` in the source text.
        end: Codepoint offset one past ``form`` in the source text.
        polarity: Surface-keyed sentiment polarity, ``0`` when the token is neutral.
        negated: Whether a preceding clause-local negator scopes this token.
    """

    form: str
    lower: str
    lemma: str
    upos: str
    start: int
    end: int
    polarity: int
    negated: bool


def analyze(text: str) -> list[Token]:
    """Analyze ``text`` into typed :class:`Token` objects via the embedded UDPipe model.

    Tokenizes, tags, and lemmatizes ``text``, resolves each token's codepoint span
    in the source, looks up its surface polarity, and flags clause-local negation.
    Negation is scoped forward within a sentence: a negator (lemma in ``not``, ``no``,
    ``never``, ``none``, ``nothing``, ``without``) flags every following content token
    until a clause boundary (punctuation or a coordinating/subordinating conjunction).
    A negator never flags itself. This is a coarse approximation — it does not handle
    ``not only``, litotes, or idioms — which is acceptable for short feedback text.

    Example:
        >>> [(t.form, t.upos) for t in analyze("this isn't broken")]
        [('this', 'PRON'), ('is', 'AUX'), ("n't", 'PART'), ('broken', 'VERB')]
    """
    return [Token(*row) for row in _parser_rs.nlp_analyze(text)]
