from __future__ import annotations

from cc_transcript.builders import (
    NOISE_SPEC,
    build_spec,
    drop_compacted,
    drop_empty,
    drop_entrypoints,
    drop_junk,
    drop_meta_flag,
    drop_phrases,
    drop_short,
    drop_sidechain,
    drop_synthetic,
    keep_only,
)
from cc_transcript.filterspec import (
    ASSISTANTS,
    CONVERSATIONAL,
    RESUME_PHRASE_SET,
    SENTIMENT_JUNK_GROUPS,
    STRUCTURAL_GROUPS,
    STRUCTURAL_NOISE_GROUPS,
    TRIVIAL_ACK_SET,
    USERS,
    Clause,
    EntrypointIn,
    FilterSpec,
    KindIs,
    MetaFlag,
    ModelIs,
    TextEmpty,
    TextInSet,
    TextMatchesAny,
    WordCountAtMost,
)


def test_keep_only() -> None:
    assert keep_only("user") == Clause(KindIs(frozenset({"user"})), negate=True)
    assert keep_only("user", "assistant") == Clause(KindIs(CONVERSATIONAL), negate=True)


def test_drop_synthetic() -> None:
    assert drop_synthetic() == Clause(ModelIs(frozenset({"<synthetic>"})), applies_to=ASSISTANTS)


def test_drop_empty_keys_tool_use_off_only_from() -> None:
    assert drop_empty(only_from=ASSISTANTS) == Clause(TextEmpty(consider_tool_use=True), applies_to=ASSISTANTS)
    assert drop_empty(only_from=USERS) == Clause(TextEmpty(consider_tool_use=False), applies_to=USERS)


def test_drop_sidechain_asymmetry() -> None:
    assert drop_sidechain() == Clause(MetaFlag("is_sidechain"))
    assert drop_sidechain(except_assistants=True) == Clause(MetaFlag("is_sidechain"), applies_to=USERS)


def test_drop_meta_flag() -> None:
    assert drop_meta_flag("is_meta") == Clause(MetaFlag("is_meta"))


def test_drop_compacted() -> None:
    assert drop_compacted() == (
        Clause(MetaFlag("is_compact_summary")),
        Clause(MetaFlag("is_visible_in_transcript_only")),
    )


def test_drop_entrypoints() -> None:
    assert drop_entrypoints({"sdk-cli"}) == Clause(EntrypointIn(frozenset({"sdk-cli"})))


def test_drop_junk_concatenates_named_categories_in_order() -> None:
    assert drop_junk("structural", "interrupt", "stop_hook") == Clause(
        TextMatchesAny(SENTIMENT_JUNK_GROUPS), applies_to=USERS
    )
    assert drop_junk("structural", "agent_injection") == Clause(
        TextMatchesAny(STRUCTURAL_NOISE_GROUPS), applies_to=USERS
    )


def test_drop_phrases() -> None:
    phrases = TRIVIAL_ACK_SET | RESUME_PHRASE_SET
    assert drop_phrases(phrases) == Clause(TextInSet(phrases), applies_to=USERS)


def test_drop_short_uses_the_literal_max_words() -> None:
    assert drop_short(2) == Clause(WordCountAtMost(2), applies_to=USERS)


def test_build_spec_flattens_clauses_and_tuples() -> None:
    assert build_spec(keep_only("user"), drop_compacted()) == FilterSpec(
        clauses=(
            Clause(KindIs(frozenset({"user"})), negate=True),
            Clause(MetaFlag("is_compact_summary")),
            Clause(MetaFlag("is_visible_in_transcript_only")),
        )
    )


def test_noise_spec_is_structural_only() -> None:
    assert NOISE_SPEC == FilterSpec(clauses=(Clause(TextMatchesAny(STRUCTURAL_GROUPS), applies_to=USERS),))
