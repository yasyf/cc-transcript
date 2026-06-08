from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from cc_transcript.filterspec import (
    ASSISTANTS,
    SENTIMENT_JUNK_GROUPS,
    USERS,
    Clause,
    EntrypointIn,
    FilterSpec,
    KindIs,
    MetaFlag,
    ModelIs,
    TextEmpty,
    TextMatchesAny,
    apply_spec,
    compile_groups,
)
from cc_transcript.models import AssistantEvent, ModeEvent, OtherEvent, SystemEvent, UserEvent

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator

    from cc_transcript.filterspec import EventKind
    from cc_transcript.models import TranscriptEvent

JUNK_USER_MESSAGE_RE = compile_groups(SENTIMENT_JUNK_GROUPS, True)

KIND_BY_TYPE: dict[type[TranscriptEvent], EventKind] = {
    UserEvent: "user",
    AssistantEvent: "assistant",
    SystemEvent: "system",
    ModeEvent: "mode",
    OtherEvent: "other",
}


@dataclass(frozen=True, slots=True)
class FilterConfig:
    """Opt-in, consumer-side filtering of a transcript event stream.

    A back-compatible flag-bag that lowers to a :class:`~cc_transcript.FilterSpec`
    via :meth:`to_spec`. Every flag defaults off, so a bare ``FilterConfig()``
    passes events through untouched.

    Attributes:
        keep_types: When set, drop every event not an instance of one of these
            types; a type-level allowlist applied before the per-event rules.
        drop_sidechain: Drop events whose envelope marks a sidechain.
        drop_synthetic: Drop assistant events with model ``<synthetic>``.
        drop_compacted: Drop compaction-summary and transcript-only entries
            (envelope flags on :class:`~cc_transcript.models.EntryMeta`).
        drop_empty: Drop user events with no text and assistant events with
            neither text nor a tool use.
        drop_ephemeral_entrypoints: Drop events from these entrypoints.
        junk_pattern: Drop user events whose text matches this pattern.
    """

    keep_types: tuple[type[TranscriptEvent], ...] | None = None
    drop_sidechain: bool = False
    drop_synthetic: bool = False
    drop_compacted: bool = False
    drop_empty: bool = False
    drop_ephemeral_entrypoints: frozenset[str] = frozenset()
    junk_pattern: re.Pattern[str] | None = field(default=None)

    def to_spec(self) -> FilterSpec:
        """Lowers this flag-bag into an equivalent ordered :class:`FilterSpec`."""
        return FilterSpec(clauses=tuple(self.clauses()))

    def clauses(self) -> Iterator[Clause]:
        if self.keep_types is not None:
            yield Clause(KindIs(frozenset(KIND_BY_TYPE[kind] for kind in self.keep_types)), negate=True)
        if self.drop_synthetic:
            yield Clause(ModelIs(frozenset({"<synthetic>"})), applies_to=ASSISTANTS)
        if self.drop_empty:
            yield Clause(TextEmpty(consider_tool_use=True), applies_to=ASSISTANTS)
            yield Clause(TextEmpty(consider_tool_use=False), applies_to=USERS)
        if self.junk_pattern is not None:
            yield Clause(
                TextMatchesAny(
                    (("junk", self.junk_pattern.pattern),),
                    ignore_case=bool(self.junk_pattern.flags & re.IGNORECASE),
                ),
                applies_to=USERS,
            )
        if self.drop_sidechain:
            yield Clause(MetaFlag("is_sidechain"))
        if self.drop_compacted:
            yield Clause(MetaFlag("is_compact_summary"))
            yield Clause(MetaFlag("is_visible_in_transcript_only"))
        if self.drop_ephemeral_entrypoints:
            yield Clause(EntrypointIn(self.drop_ephemeral_entrypoints))


SENTIMENT_FILTER = FilterConfig(
    keep_types=(UserEvent, AssistantEvent),
    drop_sidechain=True,
    drop_synthetic=True,
    drop_compacted=True,
    drop_empty=True,
    drop_ephemeral_entrypoints=frozenset({"sdk-cli"}),
    junk_pattern=JUNK_USER_MESSAGE_RE,
)


def apply_filters(events: Iterable[TranscriptEvent], config: FilterConfig) -> Iterator[TranscriptEvent]:
    """Yields the events that survive ``config``.

    Args:
        events: The events to filter.
        config: The filtering rules to apply.

    Yields:
        The events for which every enabled rule holds.
    """
    return apply_spec(events, config.to_spec())
