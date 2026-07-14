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

from cc_transcript.activity import SessionActivity
from cc_transcript.discovery import TranscriptExpiredError
from cc_transcript.filterspec import event_meta
from cc_transcript.ids import EventRef, EventUuid, SessionId, ToolDigest, ToolUseId
from cc_transcript.models import AssistantEvent, Question, TextBlock, ToolUseBlock
from cc_transcript.render import Budget, clip, render_tool_call, render_turn
from cc_transcript.tools import AskUserQuestionResult

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping
    from typing import Any

    from cc_transcript.activity import ToolUse, Turn
    from cc_transcript.models import ContentBlock

SCHEMA = "cc-transcript.context/2"
PREVIEW_SCHEMA = "cc-transcript.preview/1"
SUMMARY_LABEL = "[summary fidelity — transcript unavailable]"
ASK_USER_QUESTION = "AskUserQuestion"

type Fidelity = Literal["full", "summary"]
type Role = Literal["user", "assistant"]


@dataclass(frozen=True, slots=True)
class TextPreview:
    """A prose part of a turn's preview: a user prompt or assistant text, clipped.

    Attributes:
        text: The prose, clipped to the capture-time turn budget.
    """

    text: str


@dataclass(frozen=True, slots=True)
class ToolCallPreview:
    """A tool call reduced to its name, content digest, and rendered summary.

    Attributes:
        name: The tool name exactly as invoked.
        digest: The cross-language content digest of the call.
        summary: The call rendered at the capture-time tool budget.
    """

    name: str
    digest: ToolDigest
    summary: str


@dataclass(frozen=True, slots=True)
class AskUserQuestionPreview:
    """An AskUserQuestion turn: the rounds asked, the answers picked, and any notes.

    The structured replacement for scraping the clipped tool-call repr: every field
    a consumer needs — question text, header, options, and which option was
    recommended (the label ending in ``" (Recommended)"``) — is carried verbatim.

    Attributes:
        questions: The rounds lifted from the tool input, in presentation order.
        selections: Each round's question text mapped to the chosen answer.
        notes: Each annotated round's question text mapped to the reviewer's note.
    """

    questions: tuple[Question, ...]
    selections: Mapping[str, str]
    notes: Mapping[str, str]


type Preview = TextPreview | ToolCallPreview | AskUserQuestionPreview


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
        previews: The turn's typed previews (``cc-transcript.preview/1``), or None
            for a legacy window persisted before typed previews existed.
    """

    role: Role
    refs: tuple[EventRef, ...]
    preview: str
    tool_digests: tuple[ToolDigest, ...]
    previews: tuple[Preview, ...] | None = None


@dataclass(frozen=True, slots=True)
class ContextWindow:
    """The turns around an anchor event, persisted as refs plus previews.

    Attributes:
        anchor: The event the window centers on.
        before: Turns preceding the anchor's turn, oldest first.
        trigger: The anchor's own turn, or None when the window carries no
            trigger preview.
        after: Turns following the anchor's turn, oldest first.
        fidelity: ``'full'`` while the transcript backs the window;
            ``'summary'`` once only the previews remain.
        preview_chars: The preview budget the window was captured at.
        preview_schema: The typed-preview version (``cc-transcript.preview/1``)
            when the turn refs carry typed previews, or None for a legacy window.

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
    preview_schema: str | None = None

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
        """Serialize to the ``cc-transcript.context/2`` wire schema, byte-stably.

        Typed previews (``cc-transcript.preview/1``) ride alongside the rendered
        string previews when present; a legacy window omits both, so it stays
        readable by every reader.
        """
        return json.dumps(
            {
                "schema": SCHEMA,
                "anchor": ref_payload(self.anchor),
                "before": [turn_ref_payload(ref) for ref in self.before],
                "trigger": None if self.trigger is None else turn_ref_payload(self.trigger),
                "after": [turn_ref_payload(ref) for ref in self.after],
                "fidelity": self.fidelity,
                "preview_chars": self.preview_chars,
            }
            | ({} if self.preview_schema is None else {"preview_schema": self.preview_schema}),
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
        if (preview_schema := payload.get("preview_schema")) is not None and preview_schema != PREVIEW_SCHEMA:
            raise SchemaError(f"expected preview schema {PREVIEW_SCHEMA!r}, got: {preview_schema!r}")
        return cls(
            anchor=ref_from(payload["anchor"]),
            before=tuple(turn_ref_from(item) for item in payload["before"]),
            trigger=None if payload["trigger"] is None else turn_ref_from(payload["trigger"]),
            after=tuple(turn_ref_from(item) for item in payload["after"]),
            fidelity=payload["fidelity"],
            preview_chars=payload["preview_chars"],
            preview_schema=preview_schema,
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
        preview_schema=PREVIEW_SCHEMA,
    )


def turn_ref(turn: Turn, budget: Budget) -> TurnRef:
    return TurnRef(
        role="user" if turn.prompt else "assistant",
        refs=tuple(
            EventRef(meta.session_id, meta.uuid) for event in turn.events if (meta := event_meta(event)) is not None
        ),
        preview=render_turn(turn, budget=budget),
        tool_digests=tuple(use.call.digest for use in turn.tool_uses),
        previews=build_previews(turn, budget),
    )


def build_previews(turn: Turn, budget: Budget) -> tuple[Preview, ...]:
    calls = iter(turn.tool_uses)
    return (
        *((TextPreview(text=clip(turn.prompt, budget.turn_chars)),) if turn.prompt else ()),
        *(
            preview
            for event in turn.events
            if isinstance(event, AssistantEvent)
            for block in event.blocks
            for preview in preview_block_parts(block, calls, budget=budget)
        ),
    )


def preview_block_parts(block: ContentBlock, calls: Iterator[ToolUse], *, budget: Budget) -> tuple[Preview, ...]:
    match block:
        case TextBlock(text=text) if text.strip():
            return (TextPreview(text=clip(text, budget.turn_chars)),)
        case ToolUseBlock():
            return (preview_of_call(block, next(calls), budget),)
        case _:
            return ()


def preview_of_call(block: ToolUseBlock, use: ToolUse, budget: Budget) -> Preview:
    if block.name == ASK_USER_QUESTION:
        return ask_preview(block, use)
    return ToolCallPreview(name=block.name, digest=block.digest, summary=render_tool_call(use.call, budget=budget))


def ask_preview(block: ToolUseBlock, use: ToolUse) -> AskUserQuestionPreview:
    result = use.typed_result
    if isinstance(result, AskUserQuestionResult):
        selections = dict(result.answers)
        notes = {question: note for question, ann in result.annotations.items() if (note := ann.notes) is not None}
    else:
        selections, notes = {}, {}
    return AskUserQuestionPreview(questions=block.questions or (), selections=selections, notes=notes)


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
    } | ({} if ref.previews is None else {"previews": [preview_payload(preview) for preview in ref.previews]})


def turn_ref_from(payload: Mapping[str, Any]) -> TurnRef:
    return TurnRef(
        role=payload["role"],
        refs=tuple(ref_from(item) for item in payload["refs"]),
        preview=payload["preview"],
        tool_digests=tuple(ToolDigest(digest) for digest in payload["tool_digests"]),
        previews=None if "previews" not in payload else tuple(preview_from(item) for item in payload["previews"]),
    )


def preview_payload(preview: Preview) -> dict[str, Any]:
    match preview:
        case TextPreview(text=text):
            return {"kind": "text", "text": text}
        case ToolCallPreview(name=name, digest=digest, summary=summary):
            return {"kind": "tool_call", "name": name, "digest": digest, "summary": summary}
        case AskUserQuestionPreview(questions=questions, selections=selections, notes=notes):
            return {
                "kind": "ask_user_question",
                "questions": [question_payload(question) for question in questions],
                "selections": dict(selections),
                "notes": dict(notes),
            }


def preview_from(payload: Mapping[str, Any]) -> Preview:
    match payload["kind"]:
        case "text":
            return TextPreview(text=payload["text"])
        case "tool_call":
            return ToolCallPreview(name=payload["name"], digest=ToolDigest(payload["digest"]), summary=payload["summary"])
        case "ask_user_question":
            return AskUserQuestionPreview(
                questions=tuple(question_from(question) for question in payload["questions"]),
                selections=payload["selections"],
                notes=payload["notes"],
            )
        case kind:
            raise SchemaError(f"unknown preview kind: {kind!r}")


def question_payload(question: Question) -> dict[str, Any]:
    return {
        "question": question.question,
        "header": question.header,
        "multi_select": question.multi_select,
        "labels": list(question.labels),
    }


def question_from(payload: Mapping[str, Any]) -> Question:
    return Question(
        question=payload["question"],
        header=payload["header"],
        multi_select=payload["multi_select"],
        labels=tuple(payload["labels"]),
    )
