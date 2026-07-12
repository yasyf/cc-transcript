"""Hermetic contract + well-formedness guards for the surface-only lexicon.

Zero models, zero network, zero lemmatizers: :class:`Lexicon` and :func:`tokenize`
delegate to the Rust fast path (a pinned ``str.isalpha`` table plus the two vendored
TSVs). These tests pin the Rust tokenizer against a fixture, check the Rust polarity
and ``has_hit`` against the TSV data and the fixed magnitude floors, pin the two
historical bugs the redesign fixes, and validate the checked-in override table.
"""

from __future__ import annotations

import json
from pathlib import Path

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


def expected_polarity(token: str) -> int:
    """The polarity contract, computed from the TSV data — the Rust executor's oracle."""
    if (override := DOMAIN_OVERRIDES.get(token)) is not None:
        return override
    return score if abs(score := AFINN.get(token, 0)) >= Lexicon.MIN_MAGNITUDE else 0


def expected_has_hit(text: str, *, want_negative: bool) -> bool:
    polarities = [expected_polarity(token) for token in tokenize(text)]
    if want_negative:
        return any(p <= -Lexicon.FLOOR for p in polarities)
    return any(p >= Lexicon.FLOOR for p in polarities)


# ---- tokenizer fixture (source: testdata/tokenizer_fixture.json) -------------------


@requires_rust
def test_tokenizer_matches_fixture() -> None:
    for case_id, text, expected in FIXTURE_CASES:
        assert tokenize(text) == expected, case_id


# ---- the two historical bugs the redesign fixes (per DELIVERABLE_5) -----------------


@requires_rust
def test_bug_broken_is_hostile() -> None:
    # Was a live spaCy(-2, miss) vs UDPipe(-3, hit) split; the surface path pins -3.
    assert Lexicon.polarity("broken") == -3
    assert Lexicon.has_hit("this is broken", want_negative=True) is True


@requires_rust
def test_bug_lost_recovers_signal() -> None:
    # Both old lemmatizers collapsed 'lost'/'losing' to 'lose' (0); the surface keeps AFINN -3.
    for surface in ("lost", "losing"):
        assert Lexicon.polarity(surface) == -3
    assert Lexicon.has_hit("we lost the data", want_negative=True) is True


# ---- polarity + has_hit against the TSV data, one backend --------------------------


@requires_rust
def test_embedded_overrides_match_source() -> None:
    from cc_transcript import _parser_rs

    assert dict(_parser_rs.lexicon_overrides()) == DOMAIN_OVERRIDES


@requires_rust
def test_polarity_and_has_hit_match_tsv_data() -> None:
    # Every override key plus a deterministic AFINN slice, through polarity and both
    # has_hit axes, against the TSV-derived contract — exact equality, no sampling luck.
    keys = list(DOMAIN_OVERRIDES) + sorted(AFINN)[::97]
    assert len(keys) > len(DOMAIN_OVERRIDES)
    for key in keys:
        assert Lexicon.polarity(key) == expected_polarity(key), key
        for want_negative in (True, False):
            assert Lexicon.has_hit(key, want_negative=want_negative) == expected_has_hit(
                key, want_negative=want_negative
            ), (key, want_negative)


# ---- every override key must be tokenizer-reachable --------------------------------


@requires_rust
def test_every_override_key_is_tokenizer_reachable() -> None:
    unreachable = [key for key in DOMAIN_OVERRIDES if tokenize(key) != [key]]
    assert unreachable == []


@requires_rust
def test_afinn_unreachable_keys_are_the_audited_inventory() -> None:
    unreachable = {key for key in AFINN if tokenize(key) != [key]}
    assert len(unreachable) == 28
    assert all("-" in key or any(ch.isdigit() for ch in key) for key in unreachable)


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
