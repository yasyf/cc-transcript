"""Contract tests for the UDPipe-backed NLP substrate (:func:`cc_transcript.nlp.analyze`).

Exercises the surface the sentiment lexicon and (Phase C) highlighter share: MWT
splitting, codepoint offsets across accents and astral emoji, POS tags, and the
forward clause-local negation flag, over the three canonical sentences.
"""

from __future__ import annotations

import pytest

from cc_transcript.nlp import Token, analyze
from tests.support import requires_rust


@requires_rust
def test_contraction_splits_into_mwt_subwords() -> None:
    tokens = analyze("can't believe this isn't broken")
    assert [t.form for t in tokens] == ["ca", "n't", "believe", "this", "is", "n't", "broken"]
    assert [t.lemma for t in tokens if t.form == "n't"] == ["not", "not"]


@requires_rust
def test_offsets_are_codepoint_indices_and_reslice_the_surface() -> None:
    # An astral emoji is one codepoint: char offsets must reslice each surface exactly.
    text = "wow 😀 amazing café"
    for token in analyze(text):
        assert text[token.start : token.end] == token.form, token.form
    emoji = next(t for t in analyze(text) if t.form == "😀")
    assert (emoji.start, emoji.end) == (4, 5)


@requires_rust
def test_pos_tags_over_a_canonical_sentence() -> None:
    by_form = {t.form: t.upos for t in analyze("the progress looks amazing")}
    assert by_form == {"the": "DET", "progress": "NOUN", "looks": "VERB", "amazing": "ADJ"}


@requires_rust
@pytest.mark.parametrize(
    ("text", "negated_forms"),
    [
        ("this is not great", {"great"}),
        ("this isn't broken", {"broken"}),
        ("the progress looks amazing", set()),
    ],
    ids=["not-great", "isnt-broken", "no-negation"],
)
def test_negation_flag_scopes_following_content(text: str, negated_forms: set[str]) -> None:
    assert {t.form for t in analyze(text) if t.negated} == negated_forms


@requires_rust
def test_polarity_is_surface_keyed_not_lemma() -> None:
    # 'broken' lemmatizes to 'break' (override -2) but its surface polarity is -3.
    broken = next(t for t in analyze("this is broken") if t.form == "broken")
    assert (broken.lemma, broken.polarity) == ("break", -3)


@requires_rust
def test_analyze_returns_typed_frozen_tokens() -> None:
    [token] = analyze("amazing")
    assert isinstance(token, Token)
    with pytest.raises(AttributeError):
        token.polarity = 0  # type: ignore[misc]


@requires_rust
@pytest.mark.parametrize("ws", ["\x85", "\x0b", "\x0c"], ids=["nel", "vt", "ff"])
def test_whitespace_char_emitted_as_token_keeps_offsets_aligned(ws: str) -> None:
    # UDPipe emits U+0085/U+000B/U+000C as standalone tokens; the length-walk must give
    # each its own span, not eat it as inter-token separator whitespace and desync the rest.
    text = f"a {ws} lost"
    tokens = analyze(text)
    assert [t.form for t in tokens] == ["a", ws, "lost"]
    spans = [(t.start, t.end) for t in tokens]
    assert spans == sorted(spans)
    for (_, prev_end), (start, _) in zip(spans, spans[1:]):
        assert start >= prev_end
    for token in tokens:
        assert text[token.start : token.end] == token.form, token.form


@requires_rust
def test_null_byte_raises_and_does_not_poison_the_model() -> None:
    # The parse fails on a NUL byte; the fix returns the error without panicking under
    # the model lock, so a later valid call still succeeds — the mutex is not poisoned.
    with pytest.raises(ValueError):
        analyze("x\0y")
    assert [t.form for t in analyze("hello")] == ["hello"]


@requires_rust
def test_negator_is_never_self_flagged_even_inside_an_active_scope() -> None:
    # 'never' sits inside the scope 'not' opened, but a negator never flags itself; the
    # content it precedes ('great') is still flagged.
    negated = {t.form for t in analyze("this is not never great") if t.negated}
    assert "never" not in negated
    assert "great" in negated
