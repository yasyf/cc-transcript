"""Durable context windows: refs plus labeled previews, never bake-truncated prose.

A :class:`ContextWindow` captures the turns around an anchor event as
:class:`TurnRef` objects — resolvable references plus render-time previews —
so consumers persist pointers back into the transcript and re-render at full
fidelity while it lives. Once the transcript expires, the previews are the
fallback, always labeled as summary fidelity.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from cc_transcript.activity import SessionActivity, meta_of
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolDigest, ToolUseId
from cc_transcript.render import Budget, clip, render_turn

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

    from cc_transcript.activity import Turn

SCHEMA = "cc-transcript.context/2"
SUMMARY_LABEL = "[summary fidelity — transcript unavailable]"

type Fidelity = Literal["full", "summary"]
type Role = Literal["user", "assistant"]


class SchemaError(ValueError):
    """Persisted context data does not carry a known schema version."""


@dataclass(frozen=True, slots=True)
class TurnRef:
    """A reference to one turn: resolvable refs plus a capture-time preview.

    Attributes:
        role: Whether a user prompt opened the turn or it is assistant-only
            preamble.
        refs: References to the turn's events.
        preview: The turn rendered at capture time, at the preview budget.
        tool_digests: Content digests of the turn's tool calls, in order.
    """

    role: Role
    refs: tuple[EventRef, ...]
    preview: str
    tool_digests: tuple[ToolDigest, ...]


@dataclass(frozen=True, slots=True)
class ContextWindow:
    """The turns around an anchor event, persisted as refs plus previews.

    Attributes:
        anchor: The event the window centers on.
        before: Turns preceding the anchor's turn, oldest first.
        trigger: The anchor's own turn; None for rows converted from pre-2.0
            stores that recorded no trigger turn.
        after: Turns following the anchor's turn, oldest first.
        fidelity: ``'full'`` while the transcript backs the window;
            ``'summary'`` once only the previews remain.
        preview_chars: The preview budget the window was captured at.

    Example:
        >>> window = capture_window(activity, anchor)
        >>> hydrated = await window.hydrate()
        >>> text = hydrated.render(budget=Budget()) if hydrated else window.render_preview(budget=Budget())
    """

    anchor: EventRef
    before: tuple[TurnRef, ...]
    trigger: TurnRef | None
    after: tuple[TurnRef, ...]
    fidelity: Fidelity
    preview_chars: int

    def render_preview(self, *, budget: Budget) -> str:
        """Render the persisted previews, never touching the transcript.

        Summary-fidelity windows always lead with the
        ``[summary fidelity — transcript unavailable]`` label.
        """
        return "\n\n".join(
            (
                *((SUMMARY_LABEL,) if self.fidelity == "summary" else ()),
                *(clip(ref.preview, budget.turn_chars) for ref in window_refs(self) if ref.preview),
            )
        )

    async def hydrate(self) -> HydratedWindow | None:
        """Resolve every ref back to real turns for full-fidelity rendering.

        Returns:
            None when the transcript has expired or any ref was compacted
            away — callers fall back to :meth:`render_preview`, never
            hydrate-or-fail.
        """
        try:
            activity = await SessionActivity.from_session(self.anchor.session_id)
        except TranscriptExpiredError:
            return None
        turns: list[Turn] = []
        for turn_ref in window_refs(self):
            resolved = [turn for ref in turn_ref.refs if (turn := activity.turn_of(ref)) is not None]
            if len(resolved) != len(turn_ref.refs) or not resolved:
                return None
            turns.append(resolved[0])
        return HydratedWindow(window=self, turns=tuple(turns))

    def to_json(self) -> str:
        """Serialize to the ``cc-transcript.context/2`` wire schema, byte-stably."""
        return json.dumps(
            {
                "schema": SCHEMA,
                "anchor": ref_payload(self.anchor),
                "before": [turn_ref_payload(ref) for ref in self.before],
                "trigger": None if self.trigger is None else turn_ref_payload(self.trigger),
                "after": [turn_ref_payload(ref) for ref in self.after],
                "fidelity": self.fidelity,
                "preview_chars": self.preview_chars,
            },
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )

    @classmethod
    def from_json(cls, data: str) -> ContextWindow:
        """Deserialize a window persisted by :meth:`to_json`.

        Raises:
            SchemaError: When ``data`` does not carry the literal
                ``cc-transcript.context/2`` schema.
        """
        payload = json.loads(data)
        if not isinstance(payload, dict) or payload.get("schema") != SCHEMA:
            raise SchemaError(f"expected schema {SCHEMA!r}, got: {data[:120]}")
        return cls(
            anchor=ref_from(payload["anchor"]),
            before=tuple(turn_ref_from(item) for item in payload["before"]),
            trigger=None if payload["trigger"] is None else turn_ref_from(payload["trigger"]),
            after=tuple(turn_ref_from(item) for item in payload["after"]),
            fidelity=payload["fidelity"],
            preview_chars=payload["preview_chars"],
        )


@dataclass(frozen=True, slots=True)
class HydratedWindow:
    """A context window resolved back to its real turns.

    Attributes:
        window: The capture this hydration came from.
        turns: The resolved turns, in window order — before, trigger, after.
    """

    window: ContextWindow
    turns: tuple[Turn, ...]

    def render(self, *, budget: Budget) -> str:
        """Render the resolved turns at full fidelity; truncation happens only here."""
        return "\n\n".join(rendered for turn in self.turns if (rendered := render_turn(turn, budget=budget)))


def capture_window(
    activity: SessionActivity,
    anchor: EventRef,
    *,
    before: int = 6,
    after: int = 2,
    preview_chars: int = 200,
) -> ContextWindow:
    """Capture the turns around ``anchor`` as a live, full-fidelity window.

    Builds a :class:`TurnRef` per turn with a preview rendered now via
    :func:`~cc_transcript.render.render_turn` at the explicit, persisted
    preview budget.

    Args:
        activity: The lifted session containing the anchor.
        anchor: The event the window centers on.
        before: How many turns before the anchor's turn to capture.
        after: How many turns after the anchor's turn to capture.
        preview_chars: The per-chunk preview budget, persisted on the window.

    Raises:
        ValueError: When ``anchor`` does not resolve within ``activity``.
    """
    if (trigger := activity.turn_of(anchor)) is None:
        raise ValueError(f"anchor {anchor.event_uuid} not found in session {activity.session_id}")
    budget = Budget(turn_chars=preview_chars, tool_chars=preview_chars)
    return ContextWindow(
        anchor=anchor,
        before=tuple(
            turn_ref(turn, budget) for turn in activity.turns[max(0, trigger.index - before) : trigger.index]
        ),
        trigger=turn_ref(trigger, budget),
        after=tuple(turn_ref(turn, budget) for turn in activity.turns[trigger.index + 1 : trigger.index + 1 + after]),
        fidelity="full",
        preview_chars=preview_chars,
    )


def turn_ref(turn: Turn, budget: Budget) -> TurnRef:
    return TurnRef(
        role="user" if turn.prompt else "assistant",
        refs=tuple(
            EventRef(meta.session_id, meta.uuid) for event in turn.events if (meta := meta_of(event)) is not None
        ),
        preview=render_turn(turn, budget=budget),
        tool_digests=tuple(use.call.digest for use in turn.tool_uses),
    )


def window_refs(window: ContextWindow) -> tuple[TurnRef, ...]:
    return (*window.before, *(() if window.trigger is None else (window.trigger,)), *window.after)


def ref_payload(ref: EventRef) -> dict[str, str | None]:
    return {"session_id": ref.session_id, "event_uuid": ref.event_uuid, "tool_use_id": ref.tool_use_id}


def ref_from(payload: Mapping[str, Any]) -> EventRef:
    return EventRef(
        session_id=SessionId(payload["session_id"]),
        event_uuid=EventUuid(payload["event_uuid"]),
        tool_use_id=None if payload["tool_use_id"] is None else ToolUseId(payload["tool_use_id"]),
    )


def turn_ref_payload(ref: TurnRef) -> dict[str, Any]:
    return {
        "role": ref.role,
        "refs": [ref_payload(item) for item in ref.refs],
        "preview": ref.preview,
        "tool_digests": list(ref.tool_digests),
    }


def turn_ref_from(payload: Mapping[str, Any]) -> TurnRef:
    return TurnRef(
        role=payload["role"],
        refs=tuple(ref_from(item) for item in payload["refs"]),
        preview=payload["preview"],
        tool_digests=tuple(ToolDigest(digest) for digest in payload["tool_digests"]),
    )
