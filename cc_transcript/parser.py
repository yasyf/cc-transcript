from __future__ import annotations

import asyncio
import os
from contextlib import suppress
from datetime import datetime
from typing import TYPE_CHECKING, Any, ClassVar, Literal

import anyio
import anyio.to_thread
import orjson

from cc_transcript.backend import Backend, ParsedTranscript
from cc_transcript.filterspec import apply_spec
from cc_transcript.models import (
    AssistantEvent,
    CcVersion,
    ContentBlock,
    EntryMeta,
    EntryUuid,
    ModeEvent,
    OtherEvent,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    TranscriptEvent,
    UserEvent,
)

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Sequence
    from pathlib import Path

    from cc_transcript.filterspec import FilterSpec

INTERRUPT_MARKER = "[Request interrupted by user"


def parse_meta(data: dict[str, Any]) -> EntryMeta:
    return EntryMeta(
        uuid=EntryUuid(data["uuid"]),
        parent_uuid=EntryUuid(parent) if (parent := data.get("parentUuid")) else None,
        session_id=SessionId(data["sessionId"]),
        timestamp=datetime.fromisoformat(data["timestamp"]),
        cwd=data.get("cwd"),
        git_branch=data.get("gitBranch"),
        cc_version=CcVersion(version) if (version := data.get("version")) else None,
        is_sidechain=bool(data.get("isSidechain")),
        is_meta=bool(data.get("isMeta")),
        entrypoint=data.get("entrypoint"),
        is_compact_summary=bool(data.get("isCompactSummary")),
        is_visible_in_transcript_only=bool(data.get("isVisibleInTranscriptOnly")),
    )


def flatten_result_content(content: str | list[dict[str, Any]]) -> str:
    match content:
        case str():
            return content
        case list():
            return "".join(block["text"] for block in content if block.get("type") == "text")


def parse_user_blocks(content: str | list[dict[str, Any]]) -> tuple[str, tuple[ContentBlock, ...]]:
    match content:
        case str():
            return content, ()
        case list():
            texts = [block["text"] for block in content if block.get("type") == "text"]
            blocks: tuple[ContentBlock, ...] = tuple(TextBlock(text) for text in texts) + tuple(
                ToolResultBlock(
                    tool_use_id=ToolUseId(block["tool_use_id"]),
                    content=flatten_result_content(block["content"]),
                    is_error=bool(block.get("is_error")),
                )
                for block in content
                if block.get("type") == "tool_result"
            )
            return " ".join(texts), blocks


def parse_assistant_blocks(content: list[dict[str, Any]]) -> tuple[str, tuple[ContentBlock, ...]]:
    return " ".join(block["text"] for block in content if block.get("type") == "text"), tuple(
        parse_assistant_block(block) for block in content
    )


def parse_assistant_block(block: dict[str, Any]) -> ContentBlock:
    match block["type"]:
        case "text":
            return TextBlock(block["text"])
        case "thinking":
            return ThinkingBlock(block["thinking"])
        case "tool_use":
            return ToolUseBlock(id=ToolUseId(block["id"]), name=block["name"], input=block["input"])
        case unknown:
            raise ValueError(f"unexpected assistant block type: {unknown}")


def build_event(data: dict[str, Any]) -> TranscriptEvent | None:
    match data["type"]:
        case "user":
            text, blocks = parse_user_blocks(data["message"]["content"])
            return UserEvent(
                meta=parse_meta(data),
                text=text,
                blocks=blocks,
                interrupted=INTERRUPT_MARKER in text,
            )
        case "assistant":
            text, blocks = parse_assistant_blocks(data["message"]["content"])
            return AssistantEvent(
                meta=parse_meta(data),
                model=data["message"]["model"],
                text=text,
                blocks=blocks,
                stop_reason=data["message"].get("stop_reason"),
            )
        case "system":
            return SystemEvent(meta=parse_meta(data), subtype=data["subtype"], content=data.get("content"))
        case "mode":
            return ModeEvent(session_id=SessionId(data["sessionId"]), channel="mode", value=data["mode"])
        case "permission-mode":
            return ModeEvent(
                session_id=SessionId(data["sessionId"]),
                channel="permission-mode",
                value=data["permissionMode"],
            )
        case _:
            return OtherEvent(type=data["type"], raw=data)


def parse_events_from_bytes(raw: bytes) -> list[TranscriptEvent]:
    return [event for line in raw.split(b"\n") if line.strip() if (event := decode_line(line)) is not None]


def decode_line(line: bytes) -> TranscriptEvent | None:
    try:
        data = orjson.loads(line)
    except orjson.JSONDecodeError:
        return None
    return build_event(data)


async def parse_events_async(path: Path) -> list[TranscriptEvent]:
    return parse_events_from_bytes(await anyio.Path(path).read_bytes())


def parse_one(path: Path, mtime: float) -> ParsedTranscript:
    return ParsedTranscript(path=path, mtime=mtime, events=tuple(parse_events_from_bytes(path.read_bytes())))


def parse_one_filtered(path: Path, mtime: float, spec: FilterSpec | None) -> ParsedTranscript:
    parsed = parse_one(path, mtime)
    if spec is None:
        return parsed
    return ParsedTranscript(path=parsed.path, mtime=parsed.mtime, events=tuple(apply_spec(parsed.events, spec)))


def load_rust_backend() -> Backend | None:
    try:
        from cc_transcript import _parser_rs
        from cc_transcript.rust import RustBackend
    except ImportError:
        return None
    return RustBackend() if hasattr(_parser_rs, "stream_parse") else None


class PythonBackend:
    """The reference pure-Python parsing backend.

    Parses each file off the event loop via :mod:`anyio` worker threads,
    keeping at most ``prefetch`` files in flight at once.
    """

    name: ClassVar[Literal["rust", "python"]] = "python"

    async def parse_batch(
        self,
        paths: Sequence[tuple[Path, float]],
        *,
        prefetch: int,
        spec: FilterSpec | None = None,
    ) -> AsyncIterator[ParsedTranscript]:
        """See :meth:`Backend.parse_batch`."""
        if not paths:
            return
        send_ch, recv_ch = anyio.create_memory_object_stream[ParsedTranscript](max_buffer_size=prefetch)
        limiter = anyio.CapacityLimiter(prefetch)

        async def worker(path: Path, mtime: float) -> None:
            async with limiter:
                try:
                    parsed = await anyio.to_thread.run_sync(parse_one_filtered, path, mtime, spec)
                except (OSError, ValueError, KeyError):
                    return
                try:
                    await send_ch.send(parsed)
                except anyio.BrokenResourceError:
                    return

        async def drive() -> None:
            try:
                async with anyio.create_task_group() as tg:
                    for path, mtime in paths:
                        tg.start_soon(worker, path, mtime)
            finally:
                await send_ch.aclose()

        driver = asyncio.ensure_future(drive())
        try:
            async with recv_ch:
                async for parsed in recv_ch:
                    yield parsed
        finally:
            driver.cancel()
            with suppress(asyncio.CancelledError):
                await driver


class TranscriptParser:
    """The public facade over the active parsing backend.

    Resolves a :class:`Backend` once and streams parsed transcripts through it.
    """

    PREFETCH: ClassVar[int] = 8
    backend_instance: ClassVar[Backend | None] = None

    @classmethod
    def backend(cls) -> Backend:
        """Returns the resolved backend, resolving it on first use."""
        if cls.backend_instance is None:
            cls.backend_instance = cls.resolve_backend()
        return cls.backend_instance

    @classmethod
    def resolve_backend(cls) -> Backend:
        """Selects the parsing backend, honoring ``CC_TRANSCRIPT_DISABLE_RUST``.

        Returns the :class:`RustBackend` when the ``_parser_rs`` extension is
        importable and exposes ``stream_parse``; otherwise the pure-Python
        :class:`PythonBackend`. Set ``CC_TRANSCRIPT_DISABLE_RUST`` to force
        Python regardless.
        """
        if os.environ.get("CC_TRANSCRIPT_DISABLE_RUST"):
            return PythonBackend()
        return load_rust_backend() or PythonBackend()

    @classmethod
    def backend_name(cls) -> Literal["rust", "python"]:
        """Returns the resolved backend's name."""
        return cls.backend().name

    @classmethod
    async def stream_transcripts(
        cls,
        paths: Sequence[tuple[Path, float]],
        *,
        prefetch: int | None = None,
        spec: FilterSpec | None = None,
    ) -> AsyncIterator[ParsedTranscript]:
        """Streams parsed transcripts for ``paths`` via the active backend.

        Args:
            paths: Pairs of ``(path, mtime)`` to parse.
            prefetch: Files to keep in flight; defaults to :attr:`PREFETCH`.
            spec: Optional :class:`~cc_transcript.FilterSpec` applied during
                parsing; events failing it are dropped from each result.

        Yields:
            One :class:`ParsedTranscript` per input path.
        """
        async for parsed in cls.backend().parse_batch(
            paths, prefetch=prefetch if prefetch is not None else cls.PREFETCH, spec=spec
        ):
            yield parsed
