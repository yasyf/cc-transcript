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
    CacheCreation,
    CcVersion,
    ContentBlock,
    EntryMeta,
    EventUuid,
    FallbackBlock,
    InitInfo,
    McpServer,
    ModeEvent,
    ModelUsage,
    OtherBlock,
    OtherEvent,
    Plugin,
    PrintMessage,
    PrintResult,
    ServerToolUse,
    SessionId,
    SystemEvent,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    ToolUseId,
    TranscriptEvent,
    Usage,
    UserEvent,
)

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Mapping, Sequence
    from pathlib import Path

    from cc_transcript.filterspec import FilterSpec

INTERRUPT_MARKER = "[Request interrupted by user"


def parse_meta(data: Mapping[str, Any]) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(data["uuid"]),
        parent_uuid=EventUuid(parent) if (parent := data.get("parentUuid")) else None,
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
        case _:
            raise ValueError(f"unexpected result content shape: {type(content).__name__}")


def parse_user_blocks(
    content: str | list[dict[str, Any]], *, is_async: bool = False
) -> tuple[str, tuple[ContentBlock, ...]]:
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
                    is_async=is_async,
                )
                for block in content
                if block.get("type") == "tool_result"
            )
            return " ".join(texts), blocks
        case _:
            raise ValueError(f"unexpected user content shape: {type(content).__name__}")


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
        case "fallback":
            return FallbackBlock(from_model=block["from"]["model"], to_model=block["to"]["model"])
        case unknown:
            return OtherBlock(type=unknown, raw=block)


def parse_cache_creation(cc: Mapping[str, Any]) -> CacheCreation:
    return CacheCreation(
        ephemeral_5m_input_tokens=cc["ephemeral_5m_input_tokens"],
        ephemeral_1h_input_tokens=cc["ephemeral_1h_input_tokens"],
    )


def parse_server_tool_use(stu: Mapping[str, Any]) -> ServerToolUse:
    return ServerToolUse(
        web_search_requests=stu["web_search_requests"],
        web_fetch_requests=stu["web_fetch_requests"],
    )


def parse_usage(usage: Mapping[str, Any]) -> Usage:
    return Usage(
        input_tokens=usage["input_tokens"],
        output_tokens=usage["output_tokens"],
        cache_read_input_tokens=usage["cache_read_input_tokens"],
        cache_creation_input_tokens=usage["cache_creation_input_tokens"],
        cache_creation=parse_cache_creation(cc) if (cc := usage.get("cache_creation")) else None,
        service_tier=usage.get("service_tier"),
        inference_geo=usage.get("inference_geo"),
        server_tool_use=parse_server_tool_use(stu) if (stu := usage.get("server_tool_use")) else None,
    )


def parse_event(data: Mapping[str, Any]) -> TranscriptEvent | None:
    match data["type"]:
        case "user":
            text, blocks = parse_user_blocks(
                data["message"]["content"],
                is_async=isinstance(tur := data.get("toolUseResult"), dict) and tur.get("isAsync") is True,
            )
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
                usage=parse_usage(usage) if (usage := data["message"].get("usage")) else None,
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


def parse_model_usage(data: Mapping[str, Any]) -> ModelUsage:
    return ModelUsage(
        input_tokens=data["inputTokens"],
        output_tokens=data["outputTokens"],
        cache_read_input_tokens=data["cacheReadInputTokens"],
        cache_creation_input_tokens=data["cacheCreationInputTokens"],
        web_search_requests=data["webSearchRequests"],
        cost_usd=data["costUSD"],
        context_window=data["contextWindow"],
        max_output_tokens=data["maxOutputTokens"],
    )


def parse_init(data: Mapping[str, Any]) -> InitInfo:
    return InitInfo(
        mcp_servers=tuple(McpServer(name=s["name"], status=s["status"]) for s in data["mcp_servers"]),
        plugins=tuple(Plugin(name=p["name"], path=p["path"], source=p["source"]) for p in data["plugins"]),
        tools=tuple(data["tools"]),
        skills=tuple(data["skills"]),
    )


def parse_print_message(data: Mapping[str, Any]) -> PrintMessage:
    message = data["message"]
    match data["type"]:
        case "assistant":
            text, blocks = parse_assistant_blocks(message["content"])
            model = message.get("model")
        case "user":
            text, blocks = parse_user_blocks(message["content"])
            model = None
        case _:
            raise ValueError(f"unexpected print message type: {data['type']!r}")
    return PrintMessage(
        role=data["type"],
        model=model,
        text=text,
        blocks=blocks,
        uuid=EventUuid(uuid) if (uuid := data.get("uuid")) else None,
        session_id=SessionId(data["session_id"]),
    )


def parse_print_result(raw: bytes) -> PrintResult:
    """Parse a 'claude -p --output-format json' payload into a :class:`~cc_transcript.models.PrintResult`.

    Args:
        raw: The raw bytes of the JSON array claude -p emits.

    Returns:
        The parsed -p (print mode) result: billing, usage, structured output, init
        snapshot, and the conversational messages.
    """
    elements = orjson.loads(raw)
    result = next(element for element in elements if element.get("type") == "result")
    init = next((e for e in elements if e.get("type") == "system" and e.get("subtype") == "init"), None)
    return PrintResult(
        total_cost_usd=result["total_cost_usd"],
        model_usage={model: parse_model_usage(usage) for model, usage in result["modelUsage"].items()},
        usage=parse_usage(result["usage"]),
        structured_output=result.get("structured_output"),
        num_turns=result["num_turns"],
        is_error=result["is_error"],
        result=result.get("result"),
        session_id=SessionId(result["session_id"]),
        fast_mode_state=result.get("fast_mode_state"),
        stop_reason=result.get("stop_reason"),
        permission_denials=tuple(result["permission_denials"]),
        init=parse_init(init) if init else None,
        messages=tuple(parse_print_message(e) for e in elements if e.get("type") in ("user", "assistant")),
    )


def decode_line(line: bytes) -> TranscriptEvent | None:
    try:
        data = orjson.loads(line)
    except orjson.JSONDecodeError:
        return None
    return parse_event(data) if isinstance(data, dict) else None


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
                except (OSError, ValueError, KeyError, TypeError):
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
