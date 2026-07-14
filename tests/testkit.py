"""Build parsed view events from JSON snippets, through the native parser.

The v14 view model has no Python constructors, so tests can no longer hand-build
``UserEvent``/``ToolUseBlock``/… objects. Instead they synthesize the JSONL a real
transcript carries and parse it through the native backend — the exact path
production uses. :func:`parse_events` is the seam; the ``*_line`` builders assemble
the raw envelope dicts (mirroring the fields Claude Code emits), and the block
builders emit the ``message.content`` entries the parser lifts into block views.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Any

import orjson

from cc_transcript import _native

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence

    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)

Content = str | list[dict[str, Any]]


def parse_events(*lines: Mapping[str, Any]) -> list[TranscriptEvent]:
    """Parses raw transcript-line dicts through the native backend into view events."""
    raw = b"\n".join(orjson.dumps(dict(line)) for line in lines)
    return list(_native.parse_bytes(raw).events)


def parse_event(line: Mapping[str, Any]) -> TranscriptEvent:
    """Parses one raw transcript-line dict into its single view event."""
    events = parse_events(line)
    assert len(events) == 1, f"expected 1 parsed event, got {len(events)} for {line!r}"
    return events[0]


def text_block(text: str) -> dict[str, Any]:
    return {"type": "text", "text": text}


def thinking_block(thinking: str) -> dict[str, Any]:
    return {"type": "thinking", "thinking": thinking}


def tool_use(id: str, name: str, input: Mapping[str, Any]) -> dict[str, Any]:
    return {"type": "tool_use", "id": id, "name": name, "input": dict(input)}


def tool_result(tool_use_id: str, content: Content = "ok", *, is_error: bool = False) -> dict[str, Any]:
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
    """The envelope-level fields the parser lifts into :class:`EntryMeta`.

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
    """A ``mode``/``permission-mode`` envelope the parser lifts into :class:`ModeEvent`."""
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
    """An envelope of an unmodeled ``type`` the parser lifts into :class:`OtherEvent` (``raw`` = the whole line)."""
    return {"type": type} | fields
