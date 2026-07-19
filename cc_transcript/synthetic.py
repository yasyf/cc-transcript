"""Synthetic transcript builders — hand-assembled JSONL that parses into real events.

The v14 view model has no Python constructors, so tests and tools can no longer
hand-build ``UserEvent``/``ToolUseBlock``/… objects. Instead they synthesize the
JSONL a real transcript carries and parse it through the native backend — the exact
path production uses. The ``*_line`` builders assemble the raw envelope dicts
(mirroring the fields Claude Code emits), the block builders emit the
``message.content`` entries the parser lifts into block views, and
:func:`synthetic_user_event`/:func:`synthetic_assistant_event` round-trip a single
envelope through :func:`~cc_transcript.parser.parse_events` into one native event.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Any

from cc_transcript.models import AssistantEvent, UserEvent
from cc_transcript.parser import parse_events

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)

Content = str | list[dict[str, Any]]


def text_block(text: str) -> dict[str, Any]:
    """A ``text`` content block carrying ``text``."""
    return {"type": "text", "text": text}


def thinking_block(thinking: str) -> dict[str, Any]:
    """A ``thinking`` content block carrying ``thinking``."""
    return {"type": "thinking", "thinking": thinking}


def tool_use(id: str, name: str, input: Mapping[str, Any]) -> dict[str, Any]:
    """A ``tool_use`` content block invoking ``name`` with ``input`` under id ``id``."""
    return {"type": "tool_use", "id": id, "name": name, "input": dict(input)}


def tool_result(tool_use_id: str, content: Content = "ok", *, is_error: bool = False) -> dict[str, Any]:
    """A ``tool_result`` content block for ``tool_use_id``; ``is_error`` marks a failed call."""
    return {"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error}


def meta_fields(
    uuid: str,
    *,
    session_id: str = "s",
    timestamp: datetime = BASE,
    secs: int = 0,
    parent_uuid: str | None = None,
    cwd: str | None = "/repo",
    git_branch: str | None = "main",
    version: str | None = "1.2.3",
    is_sidechain: bool = False,
    is_meta: bool = False,
    entrypoint: str | None = "cli",
    is_compact_summary: bool = False,
    is_visible_in_transcript_only: bool = False,
    user_type: str | None = None,
    slug: str | None = None,
) -> dict[str, Any]:
    """The envelope-level fields the parser lifts into :class:`~cc_transcript.models.EntryMeta`.

    Optional string fields are omitted when ``None`` so the parser's ``.get`` chains
    reproduce a ``None`` attribute; booleans always serialize.
    """
    fields: dict[str, Any] = {
        "uuid": uuid,
        "sessionId": session_id,
        "timestamp": (timestamp + timedelta(seconds=secs)).isoformat(),
        "isSidechain": is_sidechain,
        "isMeta": is_meta,
        "isCompactSummary": is_compact_summary,
        "isVisibleInTranscriptOnly": is_visible_in_transcript_only,
    }
    for key, value in (
        ("parentUuid", parent_uuid),
        ("cwd", cwd),
        ("gitBranch", git_branch),
        ("version", version),
        ("entrypoint", entrypoint),
        ("userType", user_type),
        ("slug", slug),
    ):
        if value is not None:
            fields[key] = value
    return fields


def user_line(
    uuid: str,
    text: str = "",
    *,
    blocks: Sequence[dict[str, Any]] = (),
    tool_use_result: Mapping[str, Any] | str | None = None,
    tool_denial_kind: str | None = None,
    interrupted: bool = False,
    **meta_kw: Any,
) -> dict[str, Any]:
    """A ``type: user`` envelope; ``blocks`` are ``message.content`` entries.

    With ``blocks`` the content is a list — ``text`` (when non-empty) becomes a
    leading text block, exactly as a real transcript carries it. ``interrupted``
    sets ``interruptedMessageId`` so the parser derives ``interrupted=True``.
    """
    content: Content = ([text_block(text)] if text else []) + list(blocks) if blocks else text
    line: dict[str, Any] = {"type": "user", "message": {"role": "user", "content": content}} | meta_fields(
        uuid, **meta_kw
    )
    if tool_use_result is not None:
        line["toolUseResult"] = tool_use_result
    if tool_denial_kind is not None:
        line["toolDenialKind"] = tool_denial_kind
    if interrupted:
        line["interruptedMessageId"] = f"imid-{uuid}"
    return line


def assistant_line(
    uuid: str,
    text: str = "",
    *,
    model: str = "claude-opus-4-7",
    blocks: Sequence[dict[str, Any]] = (),
    stop_reason: str | None = None,
    usage: Mapping[str, Any] | None = None,
    **meta_kw: Any,
) -> dict[str, Any]:
    """An ``type: assistant`` envelope; ``blocks`` are ``message.content`` entries."""
    content = ([text_block(text)] if text else []) + list(blocks)
    message: dict[str, Any] = {"role": "assistant", "model": model, "content": content}
    if stop_reason is not None:
        message["stop_reason"] = stop_reason
    if usage is not None:
        message["usage"] = dict(usage)
    return {"type": "assistant", "message": message} | meta_fields(uuid, **meta_kw)


def mode_line(value: str, *, session_id: str = "s", channel: str = "mode") -> dict[str, Any]:
    """A ``mode``/``permission-mode`` envelope the parser lifts into :class:`~cc_transcript.models.ModeEvent`."""
    key = "permissionMode" if channel == "permission-mode" else "mode"
    return {"type": channel, "sessionId": session_id, key: value}


def system_line(subtype: str, *, content: str | None = None, level: str | None = None, **fields: Any) -> dict[str, Any]:
    """A ``type: system`` envelope; ``fields`` carry the subtype-specific detail keys."""
    line: dict[str, Any] = {"type": "system", "subtype": subtype} | meta_fields(fields.pop("uuid", "sys")) | fields
    if content is not None:
        line["content"] = content
    if level is not None:
        line["level"] = level
    return line


def other_line(type: str, **fields: Any) -> dict[str, Any]:
    """An unmodeled-``type`` envelope lifted into :class:`~cc_transcript.models.OtherEvent`; ``raw`` is the line."""
    return {"type": type} | fields


def synthetic_user_event(
    text: str = "",
    *,
    blocks: Sequence[Mapping[str, Any]] = (),
    uuid: str = "synthetic",
    **meta: Any,
) -> UserEvent:
    """Builds a native :class:`~cc_transcript.models.UserEvent` from text or content blocks.

    Serializes a ``user`` envelope and parses it through the native backend, so the
    result is a real view accepted anywhere parsed events are — including
    :meth:`~cc_transcript.activity.SessionActivity.from_events`. Extra keywords forward
    to the envelope metadata (``session_id``, ``timestamp``, …).

    Example:
        >>> synthetic_user_event("fix the bug", session_id="s1").text
        'fix the bug'

    Raises:
        ValueError: When the envelope does not parse to exactly one ``UserEvent``.
    """
    events = parse_events(user_line(uuid, text, blocks=[dict(block) for block in blocks], **meta))
    match events:
        case [UserEvent() as event]:
            return event
    raise ValueError(f"expected one UserEvent, got {[type(event).__name__ for event in events]}")


def synthetic_assistant_event(
    text: str = "",
    *,
    blocks: Sequence[Mapping[str, Any]] = (),
    uuid: str = "synthetic",
    **meta: Any,
) -> AssistantEvent:
    """Builds a native :class:`~cc_transcript.models.AssistantEvent` from text or content blocks.

    Serializes an ``assistant`` envelope and parses it through the native backend, so
    the result is a real view accepted anywhere parsed events are — including
    :meth:`~cc_transcript.activity.SessionActivity.from_events`. Extra keywords forward
    to the envelope metadata (``session_id``, ``timestamp``, …) and message fields
    (``stop_reason``, ``usage``, ``model``).

    Example:
        >>> synthetic_assistant_event("on it", stop_reason="end_turn").text
        'on it'

    Raises:
        ValueError: When the envelope does not parse to exactly one ``AssistantEvent``.
    """
    events = parse_events(assistant_line(uuid, text, blocks=[dict(block) for block in blocks], **meta))
    match events:
        case [AssistantEvent() as event]:
            return event
    raise ValueError(f"expected one AssistantEvent, got {[type(event).__name__ for event in events]}")
