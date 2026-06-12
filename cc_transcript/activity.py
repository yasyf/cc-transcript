"""Session activity lifted from parsed transcript events.

The platform spine: parse JSONL into typed events, then lift them into a
:class:`SessionActivity` of :class:`Turn` objects carrying :class:`ToolUse`
and :class:`Edit` records. Every higher capability — context windows,
evidence harvest, queries — is a pure function over this object. Turn
segmentation is injectable via a :data:`UserClassifier` because not every
``UserEvent`` is a real prompt; :func:`native_user_classifier` covers plain
Claude Code sessions and product-specific classifiers live with their
products.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING

from cc_transcript.discovery import TranscriptDiscovery, TranscriptExpiredError, find_transcript
from cc_transcript.ids import EventRef
from cc_transcript.models import AssistantEvent, SystemEvent, ToolResultBlock, ToolUseBlock, UserEvent
from cc_transcript.parser import TranscriptParser
from cc_transcript.tools import file_path_of, hunks_of, parse_tool_call

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from datetime import datetime
    from pathlib import Path

    from cc_transcript.ids import SessionId, ToolUseId
    from cc_transcript.models import EntryMeta, TranscriptEvent
    from cc_transcript.tools import Hunk, ToolCall

UserClassifier = Callable[["UserEvent"], bool]
"""Decides which :class:`~cc_transcript.models.UserEvent` objects open a turn."""


def native_user_classifier(event: UserEvent) -> bool:
    """Whether a user event is a real prompt under native Claude Code semantics.

    A prompt is non-meta, non-sidechain, not an interruption marker, and not
    tool-result-only — it must carry real text.
    """
    return not (event.meta.is_meta or event.meta.is_sidechain or event.interrupted) and bool(event.text.strip())


@dataclass(frozen=True, slots=True)
class ToolUse:
    """One tool invocation lifted from a turn's assistant events.

    Attributes:
        ref: The resolvable reference to the tool-use block.
        call: The typed tool call, constructed once at lift time.
        result: The matching result block, or None when none ever arrived.
        turn_index: The index of the turn the call fired in.
        ts: The timestamp of the assistant entry carrying the call.
    """

    ref: EventRef
    call: ToolCall
    result: ToolResultBlock | None
    turn_index: int
    ts: datetime


@dataclass(frozen=True, slots=True)
class Edit:
    """A file modification lowered from an edit-shaped tool call.

    Attributes:
        file_path: The file the call targeted.
        hunks: The before/after content pairs, in application order.
        tool: The tool name exactly as invoked.
        ref: The resolvable reference to the originating tool use.
        turn_index: The index of the turn the edit happened in.
        ts: The timestamp of the assistant entry carrying the call.
    """

    file_path: str
    hunks: tuple[Hunk, ...]
    tool: str
    ref: EventRef
    turn_index: int
    ts: datetime


@dataclass(frozen=True, slots=True)
class Turn:
    """One prompt-to-prompt span of a session.

    Attributes:
        index: The turn's position in the session, starting at 0. Events
            before the first qualifying prompt form turn 0 with prompt ``""``.
        prompt: The user text that opened the turn.
        started_at: The first timestamp among the turn's events, or None when
            no event carries one.
        ended_at: The last timestamp among the turn's events, or None when no
            event carries one.
        events: Every transcript event in the turn, in file order.
        tool_uses: Every tool invocation lifted from the turn's assistant
            events, in order.
    """

    index: int
    prompt: str
    started_at: datetime | None
    ended_at: datetime | None
    events: tuple[TranscriptEvent, ...]
    tool_uses: tuple[ToolUse, ...]

    @property
    def edits(self) -> tuple[Edit, ...]:
        """The turn's file modifications: tool uses with hunks and a file path."""
        return tuple(
            Edit(file_path=path, hunks=hunks, tool=use.call.name, ref=use.ref, turn_index=use.turn_index, ts=use.ts)
            for use in self.tool_uses
            if (hunks := hunks_of(use.call)) and (path := file_path_of(use.call)) is not None
        )


@dataclass(frozen=True, slots=True)
class SessionActivity:
    """A session's transcript lifted into turns, tool uses, and edits.

    Attributes:
        session_id: The Claude session UUID, the only session key.
        turns: The session's turns, indexed by position.

    Example:
        >>> activity = await SessionActivity.from_session(session_id)
        >>> activity.edits_before(anchor, lookback_turns=40)
    """

    session_id: SessionId
    turns: tuple[Turn, ...]

    @classmethod
    def from_events(
        cls,
        session_id: SessionId,
        events: Sequence[TranscriptEvent],
        *,
        user_classifier: UserClassifier = native_user_classifier,
    ) -> SessionActivity:
        """Lifts parsed events into turns.

        Each event for which ``user_classifier`` returns True opens a new
        turn; everything else folds into the current one. Events before the
        first qualifying prompt form turn 0 with prompt ``""``.
        """
        segments: list[tuple[str, list[TranscriptEvent]]] = []
        for event in events:
            match event:
                case UserEvent() if user_classifier(event):
                    segments.append((event.text, [event]))
                case _ if segments:
                    segments[-1][1].append(event)
                case _:
                    segments.append(("", [event]))
        results = {
            block.tool_use_id: block
            for event in events
            if isinstance(event, UserEvent)
            for block in event.blocks
            if isinstance(block, ToolResultBlock)
        }
        return cls(
            session_id=session_id,
            turns=tuple(
                lift_turn(session_id, index, prompt, tuple(turn_events), results)
                for index, (prompt, turn_events) in enumerate(segments)
            ),
        )

    @classmethod
    async def from_session(
        cls,
        session_id: SessionId,
        *,
        user_classifier: UserClassifier = native_user_classifier,
        root: Path | None = None,
    ) -> SessionActivity:
        """Discovers, parses, and lifts ``session_id``'s transcript from disk.

        Args:
            session_id: The session to load.
            user_classifier: Decides which user events open turns.
            root: The projects directory to search; defaults to
                ``~/.claude/projects``.

        Raises:
            TranscriptExpiredError: When no transcript for ``session_id``
                exists on disk — Claude Code prunes them after roughly thirty
                days.
        """
        if (path := await find_transcript(session_id, root=root)) is None or (
            mtime := await TranscriptDiscovery.stat_mtime(path)
        ) is None:
            raise TranscriptExpiredError(session_id)
        async for parsed in TranscriptParser.stream_transcripts([(path, mtime)]):
            return cls.from_events(session_id, parsed.events, user_classifier=user_classifier)
        raise TranscriptExpiredError(session_id)

    @property
    def edits(self) -> tuple[Edit, ...]:
        """Every edit in the session, in chronological order."""
        return tuple(edit for turn in self.turns for edit in turn.edits)

    def turn_of(self, ref: EventRef) -> Turn | None:
        """The turn containing ``ref``'s event.

        Returns:
            The turn, or None when the event is gone — a reference compacted
            away inside a still-living transcript. Never raises for a miss.
        """
        return next(
            (
                turn
                for turn in self.turns
                for event in turn.events
                if (meta := meta_of(event)) is not None and meta.uuid == ref.event_uuid
            ),
            None,
        )

    def edits_before(self, anchor: EventRef, *, lookback_turns: int) -> tuple[Edit, ...]:
        """Edits in the lookback window before ``anchor``, newest-first.

        Covers the ``lookback_turns`` turns strictly before the anchor's turn
        plus edits earlier in the anchor turn itself. A compacted-away anchor
        yields ``()``.
        """
        if (turn := self.turn_of(anchor)) is None:
            return ()
        anchor_pos = position_in(turn, anchor)
        prior = [edit for t in self.turns[max(0, turn.index - lookback_turns) : turn.index] for edit in t.edits]
        same_turn = [edit for edit in turn.edits if position_in(turn, edit.ref) < anchor_pos]
        return tuple(reversed(prior + same_turn))

    def edits_after(self, anchor: EventRef, *, file_path: str, lookahead_turns: int) -> tuple[Edit, ...]:
        """Edits to ``file_path`` after ``anchor``, oldest-first.

        Covers the rest of the anchor turn plus the ``lookahead_turns`` turns
        after it. A compacted-away anchor yields ``()``.
        """
        if (turn := self.turn_of(anchor)) is None:
            return ()
        anchor_pos = position_in(turn, anchor)
        same_turn = [
            edit for edit in turn.edits if edit.file_path == file_path and position_in(turn, edit.ref) > anchor_pos
        ]
        later = [
            edit
            for t in self.turns[turn.index + 1 : turn.index + 1 + lookahead_turns]
            for edit in t.edits
            if edit.file_path == file_path
        ]
        return tuple(same_turn + later)


def hunk_overlap(a: Hunk, b: Hunk) -> float:
    """The fraction of ``a.new``'s non-empty lines present in ``b.old``.

    Lines are whitespace-normalized — stripped, internal runs collapsed —
    before comparison, and lines empty after normalization are ignored.

    Returns:
        A value in ``[0.0, 1.0]``; 0.0 when ``a.new`` has no non-empty lines.
    """
    lines = [normalized for line in a.new.splitlines() if (normalized := " ".join(line.split()))]
    if not lines:
        return 0.0
    olds = {normalized for line in b.old.splitlines() if (normalized := " ".join(line.split()))}
    return sum(line in olds for line in lines) / len(lines)


def meta_of(event: TranscriptEvent) -> EntryMeta | None:
    match event:
        case UserEvent() | AssistantEvent() | SystemEvent():
            return event.meta
        case _:
            return None


def lift_turn(
    session_id: SessionId,
    index: int,
    prompt: str,
    events: tuple[TranscriptEvent, ...],
    results: Mapping[ToolUseId, ToolResultBlock],
) -> Turn:
    stamps = [meta.timestamp for event in events if (meta := meta_of(event)) is not None]
    return Turn(
        index=index,
        prompt=prompt,
        started_at=stamps[0] if stamps else None,
        ended_at=stamps[-1] if stamps else None,
        events=events,
        tool_uses=tuple(
            ToolUse(
                ref=EventRef(session_id, event.meta.uuid, block.id),
                call=parse_tool_call(block.name, block.input, on_error="other"),
                result=results.get(block.id),
                turn_index=index,
                ts=event.meta.timestamp,
            )
            for event in events
            if isinstance(event, AssistantEvent)
            for block in event.blocks
            if isinstance(block, ToolUseBlock)
        ),
    )


def position_in(turn: Turn, ref: EventRef) -> tuple[int, int]:
    event_pos, event = next(
        (i, event)
        for i, event in enumerate(turn.events)
        if (meta := meta_of(event)) is not None and meta.uuid == ref.event_uuid
    )
    match ref.tool_use_id, event:
        case None, _:
            return event_pos, -1
        case tool_use_id, AssistantEvent(blocks=blocks):
            return event_pos, next(
                i for i, block in enumerate(blocks) if isinstance(block, ToolUseBlock) and block.id == tool_use_id
            )
        case _:
            return event_pos, -1
