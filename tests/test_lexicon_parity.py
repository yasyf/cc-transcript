"""Hermetic parity + well-formedness guards for the surface-only lexicon.

Zero models, zero network, zero lemmatizers: the Python :class:`Lexicon` and the
Rust fast path both tokenize identically (a pinned ``str.isalpha`` table) and look
up token surfaces in the same two TSVs. These tests assert the two backends produce
byte-identical tokenizations and polarities, pin the two historical bugs the
redesign fixes, and validate the checked-in override table.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from cc_transcript.sentiment.lexicon import AFINN, DOMAIN_OVERRIDES, Lexicon, tokenize
from tests.support import requires_rust

TOKENIZER_FIXTURE = Path(__file__).resolve().parent / "testdata" / "tokenizer_fixture.json"
LEXICON_DATA = Path(__file__).resolve().parent.parent / "cc_transcript" / "sentiment" / "data"


def fixture_cases() -> list[tuple[str, str, list[str]]]:
    cases = json.loads(TOKENIZER_FIXTURE.read_text(encoding="utf-8"))
    return [
        (case["id"], "".join(chr(int(cp[2:], 16)) for cp in case["input_codepoints"]), case["expected_tokens"])
        for case in cases
    ]


FIXTURE_CASES = fixture_cases()


def parse_tsv(name: str) -> dict[str, int]:
    text = (LEXICON_DATA / name).read_text(encoding="utf-8")
    return {
        word: int(score)
        for line in text.splitlines()
        for word, sep, score in [line.partition("\t")]
        if sep and not word.startswith("#")
    }


# ---- tokenizer parity (source: testdata/tokenizer_fixture.json) --------------------


@pytest.mark.parametrize(("case_id", "text", "expected"), FIXTURE_CASES, ids=[c[0] for c in FIXTURE_CASES])
def test_python_tokenizer_matches_fixture(case_id: str, text: str, expected: list[str]) -> None:
    assert tokenize(text) == expected


@requires_rust
@pytest.mark.parametrize(("case_id", "text", "expected"), FIXTURE_CASES, ids=[c[0] for c in FIXTURE_CASES])
def test_rust_tokenizer_matches_python_and_fixture(case_id: str, text: str, expected: list[str]) -> None:
    from cc_transcript import _parser_rs

    assert list(_parser_rs.lexicon_tokenize(text)) == tokenize(text) == expected


# ---- the two historical bugs the redesign fixes (per DELIVERABLE_5) -----------------


@requires_rust
def test_bug_broken_is_hostile_both_backends() -> None:
    # Was a live spaCy(-2, miss) vs UDPipe(-3, hit) split; the surface path pins -3 both sides.
    from cc_transcript import _parser_rs

    assert Lexicon.polarity("broken") == -3
    assert _parser_rs.lexicon_polarity("broken") == -3
    assert Lexicon.has_hit("this is broken", want_negative=True) is True
    assert _parser_rs.lexicon_has_hit("this is broken", True) is True


@requires_rust
def test_bug_lost_recovers_signal_both_backends() -> None:
    # Both old lemmatizers collapsed 'lost'/'losing' to 'lose' (0); the surface keeps AFINN -3.
    from cc_transcript import _parser_rs

    for surface in ("lost", "losing"):
        assert Lexicon.polarity(surface) == -3
        assert _parser_rs.lexicon_polarity(surface) == -3
    assert Lexicon.has_hit("we lost the data", want_negative=True) is True
    assert _parser_rs.lexicon_has_hit("we lost the data", True) is True


# ---- full-lexicon polarity + has_hit equality, both backends -----------------------


@requires_rust
def test_embedded_overrides_match_source() -> None:
    from cc_transcript import _parser_rs

    assert dict(_parser_rs.lexicon_overrides()) == DOMAIN_OVERRIDES


@requires_rust
def test_full_lexicon_polarity_and_has_hit_parity() -> None:
    from cc_transcript import _parser_rs

    # Every override key plus a deterministic AFINN slice, through polarity and both
    # has_hit axes on both backends — exact equality, no sampling luck.
    keys = list(DOMAIN_OVERRIDES) + sorted(AFINN)[::97]
    assert len(keys) > len(DOMAIN_OVERRIDES)
    for key in keys:
        assert _parser_rs.lexicon_polarity(key) == Lexicon.polarity(key), key
        for want_negative in (True, False):
            assert _parser_rs.lexicon_has_hit(key, want_negative) == Lexicon.has_hit(
                key, want_negative=want_negative
            ), (key, want_negative)


# ---- every override key must be tokenizer-reachable --------------------------------


def test_every_override_key_is_tokenizer_reachable() -> None:
    unreachable = [key for key in DOMAIN_OVERRIDES if tokenize(key) != [key]]
    assert unreachable == []


# ---- TSV well-formedness (the validation the old build script owned) ---------------


def test_domain_overrides_tsv_wellformed() -> None:
    overrides = parse_tsv("domain_overrides.tsv")
    assert overrides == DOMAIN_OVERRIDES
    assert len(overrides) == 61
    assert list(overrides) == sorted(overrides), "domain_overrides.tsv must stay word-sorted"
    assert all(1 <= abs(score) <= 5 for score in overrides.values())
    header = [line for line in (LEXICON_DATA / "domain_overrides.tsv").read_text(encoding="utf-8").splitlines() if line.startswith("#")]
    assert header and any("ODbL" in line and "AFINN" in line for line in header)


def test_afinn_tsv_wellformed() -> None:
    afinn = parse_tsv("afinn-en-165.tsv")
    assert afinn == AFINN
    assert len(afinn) > 3000
    assert all(1 <= abs(score) <= 5 for score in afinn.values())
    assert all(" " not in word for word in afinn)
