"""Composable builder fragments for :class:`~cc_transcript.FilterSpec`.

Each fragment returns a frozen :class:`~cc_transcript.Clause` (or a tuple of them),
so a consumer declares its filtering policy as a readable composition rather than a
hand-written clause tuple. :func:`build_spec` flattens fragments into a spec.

Because :func:`~cc_transcript.keep` is a pure existential OR over DROP clauses,
fragment order never affects the keep/drop result — only which clauses are present.

Example:
    >>> from cc_transcript import build_spec, keep_only, drop_junk, drop_short
    >>> spec = build_spec(keep_only("user"), drop_junk("structural"), drop_short(2))
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from cc_transcript.filterspec import (
    ASSISTANTS,
    JUNK_CATEGORIES,
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

if TYPE_CHECKING:
    from collections.abc import Iterable

    from cc_transcript.filterspec import EventKind, MetaFlagName


def keep_only(*kinds: EventKind) -> Clause:
    """Drops every event whose kind is not in ``kinds``."""
    return Clause(KindIs(frozenset(kinds)), negate=True)


def drop_synthetic() -> Clause:
    """Drops assistant events with the ``<synthetic>`` model."""
    return Clause(ModelIs(frozenset({"<synthetic>"})), applies_to=ASSISTANTS)


def drop_empty(*, only_from: frozenset[EventKind]) -> Clause:
    """Drops blank events of one kind.

    Assistant turns with a tool-use block are not blank; user turns have no such
    rescue, so ``consider_tool_use`` is keyed off ``only_from``. Defined for the
    single-kind ``USERS`` / ``ASSISTANTS`` sets.
    """
    return Clause(TextEmpty(consider_tool_use=only_from != USERS), applies_to=only_from)


def drop_sidechain(*, except_assistants: bool = False) -> Clause:
    """Drops sidechain events; ``except_assistants`` keeps assistant sidechains."""
    return Clause(MetaFlag("is_sidechain"), applies_to=USERS if except_assistants else frozenset())


def drop_meta_flag(flag: MetaFlagName, *, only_from: frozenset[EventKind] = frozenset()) -> Clause:
    """Drops events whose ``EntryMeta`` boolean ``flag`` is set."""
    return Clause(MetaFlag(flag), applies_to=only_from)


def drop_compacted() -> tuple[Clause, Clause]:
    """Drops compaction-summary and transcript-only entries."""
    return (Clause(MetaFlag("is_compact_summary")), Clause(MetaFlag("is_visible_in_transcript_only")))


def drop_entrypoints(entrypoints: Iterable[str]) -> Clause:
    """Drops events whose ``meta.entrypoint`` is in ``entrypoints``."""
    return Clause(EntrypointIn(frozenset(entrypoints)))


def drop_junk(*categories: str, only_from: frozenset[EventKind] = USERS) -> Clause:
    """Drops events matching any group in the named :data:`JUNK_CATEGORIES`."""
    return Clause(
        TextMatchesAny(tuple(group for category in categories for group in JUNK_CATEGORIES[category])),
        applies_to=only_from,
    )


def drop_phrases(phrases: frozenset[str], *, only_from: frozenset[EventKind] = USERS) -> Clause:
    """Drops events whose normalized text is one of ``phrases``."""
    return Clause(TextInSet(phrases), applies_to=only_from)


def drop_short(max_words: int, *, only_from: frozenset[EventKind] = USERS) -> Clause:
    """Drops events with at most ``max_words`` whitespace-split words."""
    return Clause(WordCountAtMost(max_words), applies_to=only_from)


def build_spec(*fragments: Clause | tuple[Clause, ...]) -> FilterSpec:
    """Flattens ``Clause`` / ``tuple[Clause, ...]`` fragments into a :class:`FilterSpec`."""
    return FilterSpec(
        clauses=tuple(
            clause for fragment in fragments for clause in (fragment if isinstance(fragment, tuple) else (fragment,))
        )
    )


NOISE_SPEC = build_spec(drop_junk("structural"))
