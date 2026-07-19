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

from cc_transcript import _native
from cc_transcript.discovery import TranscriptExpiredError, resolve
from cc_transcript.filterspec import event_meta
from cc_transcript.ids import EventRef
from cc_transcript.models import AssistantEvent, ToolResultBlock, ToolUseBlock, UserEvent
from cc_transcript.parser import parse
from cc_transcript.tools import edits_of, parse_tool_call, parse_tool_result

if TYPE_CHECKING:
    from collections.abc import Sequence
    from datetime import datetime
    from pathlib import Path

    from cc_transcript.ids import SessionId, ToolUseId
    from cc_transcript.models import TranscriptEvent
    from cc_transcript.tools import FallbackCall, FallbackResult, Hunk, ToolCall, ToolResult

UserClassifier = Callable[["UserEvent"], bool]
"""Decides which :class:`~cc_transcript.models.UserEvent` objects open a turn."""


def native_user_classifier(event: UserEvent) -> bool:
    """Whether a user event is a real prompt under native Claude Code semantics.

    A prompt is non-meta, non-sidechain, not a compact summary, not an
    interruption marker, not an agent-injected relay banner, and not
    tool-result-only — it must carry real text.
    """
    return not (
        event.meta.is_meta
        or event.meta.is_sidechain
        or event.meta.is_compact_summary
        or event.interrupted
        or event.is_agent_injected
    ) and bool(event.text.strip())


@dataclass(frozen=True, slots=True)
class ToolUse:
    """One tool invocation lifted from a turn's assistant events.

    Attributes:
        ref: The resolvable reference to the tool-use block.
        call: The typed tool call, constructed once at lift time.
        result: The matching result block, or None when none ever arrived.
        result_ts: The timestamp of the user entry carrying the result, or
            None when no result ever arrived.
        edits: The call's lowered ``(file_path, hunks)`` entries, one per
            edited file.
        turn_index: The index of the turn the call fired in.
        ts: The timestamp of the assistant entry carrying the call.
    """

    ref: EventRef
    call: ToolCall | FallbackCall
    result: ToolResultBlock | None
    result_ts: datetime | None
    edits: tuple[tuple[str, tuple[Hunk, ...]], ...]
    turn_index: int
    ts: datetime

    @property
    def duration_ms(self) -> int | None:
        """Milliseconds from the call to its result, or None without a result."""
        return None if self.result_ts is None else round((self.result_ts - self.ts).total_seconds() * 1000)

    @property
    def typed_result(self) -> ToolResult | FallbackResult | None:
        """The result parsed into the typed result hierarchy, or None without a result.

        The join point for :func:`~cc_transcript.tools.parse_tool_result`: pairs
        this use's tool name with its result block's ``tool_use_result`` payload,
        degrading to :class:`~cc_transcript.tools.OtherResult` on any shape drift.
        """
        if self.result is None:
            return None
        return parse_tool_result(self.call.name, self.result.tool_use_result, on_error="other")


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
        """The turn's file modifications: one entry per edited file (every file of an
        apply_patch), in order."""
        return tuple(
            Edit(file_path=path, hunks=hunks, tool=use.call.name, ref=use.ref, turn_index=use.turn_index, ts=use.ts)
            for use in self.tool_uses
            for path, hunks in use.edits
        )


@dataclass(frozen=True, slots=True)
class SessionActivity:
    """A session's transcript lifted into turns, tool uses, and edits.

    Attributes:
        session_id: The Claude session UUID, the only session key.
        turns: The session's turns, indexed by position.

    Example:
        >>> activity = SessionActivity.from_session(session_id)
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
        evs = list(events)
        opener_flags = (
            None
            if user_classifier is native_user_classifier
            else [bool(user_classifier(event)) if isinstance(event, UserEvent) else False for event in evs]
        )
        tool_blocks = {
            event_idx: tuple(block for block in event.blocks if isinstance(block, ToolUseBlock))
            for event_idx, event in enumerate(evs)
            if isinstance(event, AssistantEvent)
        }
        tool_block_positions: dict[int, int] = {}
        turns: list[Turn] = []
        for index, skeleton in enumerate(_native.activity_lift_from_events(evs, opener_flags)):
            tool_uses: list[ToolUse] = []
            for use in skeleton["tool_uses"]:
                event_idx = use["event_idx"]
                event = evs[event_idx]
                block_position = tool_block_positions.get(event_idx, 0)
                block = tool_blocks[event_idx][block_position]
                tool_block_positions[event_idx] = block_position + 1
                result_event_idx = use["result_event_idx"]
                if result_event_idx is None:
                    result = None
                    result_ts = None
                else:
                    result_event = evs[result_event_idx]
                    result = next(
                        candidate
                        for candidate in reversed(result_event.blocks)
                        if isinstance(candidate, ToolResultBlock) and candidate.tool_use_id == use["tool_use_id"]
                    )
                    result_ts = result_event.meta.timestamp
                call = parse_tool_call(block.name, block.input, on_error="other")
                tool_uses.append(
                    ToolUse(
                        ref=EventRef(session_id, event.meta.uuid, block.id),
                        call=call,
                        result=result,
                        result_ts=result_ts,
                        edits=edits_of(call),
                        turn_index=index,
                        ts=event.meta.timestamp,
                    )
                )
            started_idx = skeleton["started_idx"]
            ended_idx = skeleton["ended_idx"]
            turns.append(
                Turn(
                    index=index,
                    prompt=skeleton["prompt"],
                    started_at=None if started_idx is None else evs[started_idx].meta.timestamp,
                    ended_at=None if ended_idx is None else evs[ended_idx].meta.timestamp,
                    events=tuple(evs[skeleton["start"] : skeleton["end"]]),
                    tool_uses=tuple(tool_uses),
                )
            )
        return cls(session_id=session_id, turns=tuple(turns))

    @classmethod
    def from_session(
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
        if (path := resolve(session_id, root=root)) is None:
            raise TranscriptExpiredError(session_id)
        try:
            transcript = parse(path)
        except OSError:
            raise TranscriptExpiredError(session_id) from None
        return cls.from_events(session_id, transcript.events, user_classifier=user_classifier)

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
                if (meta := event_meta(event)) is not None and meta.uuid == ref.event_uuid
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
    return _native.activity_hunk_overlap(a.old, a.new, b.old, b.new)


def result_index(events: Sequence[TranscriptEvent]) -> dict[ToolUseId, tuple[ToolResultBlock, datetime | None]]:
    """Indexes tool results by the id of the tool use they answer.

    Pairs each :class:`~cc_transcript.models.ToolResultBlock` with the
    timestamp of the ``UserEvent`` carrying it, keyed by the block's
    ``tool_use_id`` — the single source for joining a call to its result and
    deriving the call's duration.

    Returns:
        A mapping from tool-use id to a ``(result block, result timestamp)``
        pair.
    """
    return {
        block.tool_use_id: (block, event.meta.timestamp)
        for event in events
        if isinstance(event, UserEvent)
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
    }


def event_stamps(events: Sequence[TranscriptEvent]) -> tuple[datetime | None, datetime | None]:
    stamps = [meta.timestamp for event in events if (meta := event_meta(event)) is not None]
    return (stamps[0], stamps[-1]) if stamps else (None, None)


def position_in(turn: Turn, ref: EventRef) -> tuple[int, int]:
    event_pos, event = next(
        (i, event)
        for i, event in enumerate(turn.events)
        if (meta := event_meta(event)) is not None and meta.uuid == ref.event_uuid
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
