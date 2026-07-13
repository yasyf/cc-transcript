from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal, NewType

from cc_transcript.ids import EventUuid, SessionId, ToolDigest, ToolUseId, tool_digest
from cc_transcript.tools import ToolCall, parse_tool_call

if TYPE_CHECKING:
    from datetime import datetime
    from typing import Any

CcVersion = NewType("CcVersion", str)


@dataclass(frozen=True, slots=True)
class TextBlock:
    """A text content block from a user or assistant message.

    Attributes:
        text: The block's literal text.
    """

    text: str


@dataclass(frozen=True, slots=True)
class ThinkingBlock:
    """An extended-thinking content block emitted by the assistant.

    Attributes:
        thinking: The model's thinking text.
    """

    thinking: str


@dataclass(frozen=True, slots=True)
class Question:
    """One AskUserQuestion round lifted from a tool-use input's ``questions`` array.

    Attributes:
        question: The prompt text shown to the user.
        header: The round's short header, or None when the input omits one.
        multi_select: Whether the round accepted more than one selection.
        labels: The option labels offered, in presentation order.
    """

    question: str
    header: str | None
    multi_select: bool
    labels: tuple[str, ...]


def parse_questions(rounds: object) -> tuple[Question, ...] | None:
    """Lift an AskUserQuestion ``questions`` array into typed rounds, or None.

    Mirrors the Rust parse-layer lift (``parse_questions`` in ``rust/crates/core/src/parse.rs``):
    a missing or non-list ``rounds`` reads as None; within the array each entry
    lacking a string ``question`` is dropped, ``header`` reads as None unless a
    string, ``multi_select`` is False unless ``multiSelect`` is a bool, and
    ``labels`` collects each option's string ``label``, skipping any without one.

    The single owner of the lift: :attr:`ToolUseBlock.questions` reads it from a
    tool-use input, and :attr:`~cc_transcript.tools.AskUserQuestionResult.questions`
    from the echoed result payload.
    """
    if not isinstance(rounds, list):
        return None
    return tuple(
        Question(
            question=text,
            header=h if isinstance(h := q.get("header"), str) else None,
            multi_select=isinstance(m := q.get("multiSelect"), bool) and m,
            labels=tuple(
                label
                for option in (q.get("options") if isinstance(q.get("options"), list) else ())
                if isinstance(option, dict) and isinstance(label := option.get("label"), str)
            ),
        )
        for q in rounds
        if isinstance(q, dict) and isinstance(text := q.get("question"), str)
    )


@dataclass(frozen=True, slots=True)
class ToolUseBlock:
    """An assistant request to invoke a tool.

    Attributes:
        id: The tool-use identifier referenced by the matching result.
        name: The tool's name.
        input: The tool's input arguments, preserved verbatim.
    """

    id: ToolUseId
    name: str
    input: Mapping[str, Any]

    @property
    def call(self) -> ToolCall:
        """The block parsed into the typed tool-call hierarchy.

        Strict: a known tool whose input is malformed raises
        :class:`~cc_transcript.tools.ToolInputError`.
        """
        return parse_tool_call(self.name, self.input)

    @property
    def digest(self) -> ToolDigest:
        """The cross-language content digest of this call."""
        return tool_digest(self.name, self.input)

    @property
    def file_path(self) -> str | None:
        """The raw ``file_path`` input argument when it is a string, else None.

        Mirrors the Rust parse-layer lift in ``rust/crates/core/src/parse.rs``: the value is
        read verbatim from the input for every tool, and a non-object input or a
        non-string value reads as None. Mining denial evidence consumes this uniform
        lift rather than the type-dispatched :func:`~cc_transcript.tools.file_path_of`.
        """
        return (
            p
            if isinstance(self.input, Mapping) and isinstance(p := self.input.get("file_path"), str)
            else None
        )

    @property
    def questions(self) -> tuple[Question, ...] | None:
        """The AskUserQuestion rounds lifted from the ``questions`` input array, or None.

        Delegates to :func:`parse_questions`, which mirrors the Rust parse layer; a
        non-object input reads as None.
        """
        return parse_questions(self.input.get("questions")) if isinstance(self.input, Mapping) else None


@dataclass(frozen=True, slots=True)
class ToolResultBlock:
    """The result of a tool invocation, delivered in a user turn.

    Attributes:
        tool_use_id: The id of the originating tool-use block.
        content: The result text, flattened from string or block content.
        is_error: Whether the tool reported a failure.
        is_async: Whether the originating tool ran asynchronously — computed at
            the parse layer from :attr:`tool_use_result`'s ``isAsync`` marker,
            then stored (like :attr:`UserEvent.interrupted`).
        tool_use_result: The record-level ``toolUseResult`` payload verbatim — a
            mapping for structured results, a plain string for denials, or None
            when the record carried none. Pass a tool name and this payload to
            :func:`~cc_transcript.tools.parse_tool_result` for the typed result.
        denial_kind: The tool-denial kind, computed at the parse layer — the
            record-level ``toolDenialKind`` (``user-rejected`` for a human
            rejection, ``permission-rule`` for a hook/guard block) when present,
            else ``user-rejected`` when this error block carries the legacy denial
            banner, else None.
    """

    tool_use_id: ToolUseId
    content: str
    is_error: bool
    is_async: bool = False
    tool_use_result: Mapping[str, Any] | str | None = None
    denial_kind: str | None = None


@dataclass(frozen=True, slots=True)
class FallbackBlock:
    """A marker that the assistant turn fell back from one model to another.

    Claude Code records this when a turn switches models mid-stream; it carries
    no message content, only the two model names.

    Attributes:
        from_model: The model the turn started on.
        to_model: The model the turn fell back to.
    """

    from_model: str
    to_model: str


@dataclass(frozen=True, slots=True)
class OtherBlock:
    """Any assistant content block whose ``type`` is not yet modeled.

    The escape hatch that keeps an unrecognized block from crashing the parser
    as Claude Code's transcript format evolves, mirroring :class:`OtherEvent`.

    Attributes:
        type: The block's ``type`` field.
        raw: The block's full decoded payload.
    """

    type: str
    raw: Mapping[str, Any]


ContentBlock = TextBlock | ThinkingBlock | ToolUseBlock | ToolResultBlock | FallbackBlock | OtherBlock


@dataclass(frozen=True, slots=True)
class EntryMeta:
    """Envelope metadata shared by the conversational transcript events.

    Attributes:
        uuid: The entry's unique identifier.
        parent_uuid: The parent entry's id, or None for roots.
        session_id: The session this entry belongs to.
        timestamp: The entry's timezone-aware timestamp.
        cwd: The working directory recorded for the entry.
        git_branch: The git branch recorded for the entry.
        cc_version: The Claude Code version that wrote the entry.
        is_sidechain: Whether the entry belongs to a subagent sidechain.
        is_meta: Whether the entry is a meta entry injected by the client.
        entrypoint: The entrypoint that produced the entry, e.g. ``cli``.
        is_compact_summary: Whether the entry is a compaction summary.
        is_visible_in_transcript_only: Whether the entry is transcript-only.
        user_type: The ``userType`` recorded for the entry, e.g. ``external``, or None when absent.
        slug: The session slug recorded for the entry, or None when absent.
    """

    uuid: EventUuid
    parent_uuid: EventUuid | None
    session_id: SessionId
    timestamp: datetime
    cwd: str | None
    git_branch: str | None
    cc_version: CcVersion | None
    is_sidechain: bool
    is_meta: bool
    entrypoint: str | None
    is_compact_summary: bool
    is_visible_in_transcript_only: bool
    user_type: str | None = None
    slug: str | None = None


@dataclass(frozen=True, slots=True)
class UserEvent:
    """A user turn.

    Attributes:
        meta: The entry envelope metadata.
        text: The joined text of the turn.
        blocks: The parsed content blocks, including tool results.
        interrupted: Whether the turn is a user interruption.
        is_agent_injected: Whether the turn is an agent-injected relay banner —
            a teammate-message digest, scheduled-task banner, or foreign-agent
            header — rather than an authored prompt.
        prompt_id: The client-assigned id of the prompt this turn belongs to, or None.
        prompt_source: How the prompt was submitted, e.g. ``typed``, ``queued``,
            ``system``, or ``sdk``, or None when absent.
        queue_priority: The queue priority recorded for a queued prompt, or None.
        image_paste_ids: The paste ids of images attached to the turn, or None when
            the turn carries no image-paste marker.
        source_tool_use_id: The id of the tool-use that produced this turn, when the
            turn originates from a tool result, else None.
        source_tool_assistant_uuid: The uuid of the assistant entry whose tool produced
            this turn, else None.
        mcp_meta: The verbatim ``mcpMeta`` payload attached to the turn, or None.
        permission_mode: The permission mode in effect for the turn, or None.
        interrupted_message_id: The API message id (``msg_...``) of the assistant
            turn this interruption cut short, or None. Set from the raw
            ``interruptedMessageId`` field; it names the interrupted assistant's
            API message, not a transcript event uuid.
    """

    meta: EntryMeta
    text: str
    blocks: tuple[ContentBlock, ...]
    interrupted: bool
    is_agent_injected: bool = False
    prompt_id: str | None = None
    prompt_source: str | None = None
    queue_priority: str | None = None
    image_paste_ids: tuple[int, ...] | None = None
    source_tool_use_id: ToolUseId | None = None
    source_tool_assistant_uuid: EventUuid | None = None
    mcp_meta: Mapping[str, Any] | None = None
    permission_mode: str | None = None
    interrupted_message_id: str | None = None


@dataclass(frozen=True, slots=True)
class Attribution:
    """The plugin, skill, or MCP tool an assistant turn is attributed to.

    Present on an :class:`AssistantEvent` only when the entry carries at least one
    of the four attribution fields; each component is independently optional.

    Attributes:
        plugin: The plugin the turn is attributed to, or None.
        skill: The skill the turn is attributed to, or None.
        mcp_server: The MCP server the turn is attributed to, or None.
        mcp_tool: The MCP tool the turn is attributed to, or None.
    """

    plugin: str | None
    skill: str | None
    mcp_server: str | None
    mcp_tool: str | None


@dataclass(frozen=True, slots=True)
class ApiError:
    """The upstream API error an assistant turn failed with.

    Present on an :class:`AssistantEvent` only when the entry's
    ``isApiErrorMessage`` flag is set; each component is independently optional.

    Attributes:
        error: The error kind the API reported, e.g. ``rate_limit``, or None.
        status: The HTTP status of the failed request, e.g. ``429``, or None.
        details: The free-text error detail, or None.
    """

    error: str | None
    status: int | None
    details: str | None


@dataclass(frozen=True, slots=True)
class AssistantEvent:
    """An assistant turn.

    Attributes:
        meta: The entry envelope metadata.
        model: The model that produced the turn, e.g. ``<synthetic>``.
        text: The joined text of the turn.
        blocks: The parsed content blocks, including thinking and tool uses.
        stop_reason: The model's stop reason, when present.
        usage: Token usage for the turn, or None when the entry carries no usage
            (older transcripts, API-error messages).
        request_id: The API request id that produced the turn, or None.
        forked_from: The id of the message this turn was forked from, or None.
        attribution: The plugin/skill/MCP attribution for the turn, or None when the
            entry carries no attribution field.
        api_error: The upstream API error the turn failed with, or None when the
            entry is not an API-error message.
    """

    meta: EntryMeta
    model: str
    text: str
    blocks: tuple[ContentBlock, ...]
    stop_reason: str | None
    usage: Usage | None
    request_id: str | None = None
    forked_from: str | None = None
    attribution: Attribution | None = None
    api_error: ApiError | None = None


@dataclass(frozen=True, slots=True)
class HookInfo:
    """One hook invocation recorded in a stop-hook summary.

    Attributes:
        command: The hook command that ran.
        duration_ms: The hook's wall-clock duration in milliseconds, when recorded.
    """

    command: str
    duration_ms: int | None = None


@dataclass(frozen=True, slots=True)
class StopHookSummary:
    """The typed detail of a ``stop_hook_summary`` system entry.

    Attributes:
        hook_count: The number of hooks that ran.
        hook_infos: The per-hook command and duration records.
        hook_errors: The error strings raised by hooks.
        hook_additional_context: The extra context strings hooks contributed.
        prevented_continuation: Whether a hook blocked the turn from continuing.
        stop_reason: The reason the turn stopped, when recorded.
        has_output: Whether the hooks produced output.
        tool_use_id: The tool-use id the summary is tied to, when present.
    """

    hook_count: int | None = None
    hook_infos: tuple[HookInfo, ...] = ()
    hook_errors: tuple[str, ...] = ()
    hook_additional_context: tuple[str, ...] = ()
    prevented_continuation: bool = False
    stop_reason: str | None = None
    has_output: bool = False
    tool_use_id: ToolUseId | None = None


@dataclass(frozen=True, slots=True)
class PreservedSegment:
    """The head/anchor/tail uuids of the segment preserved across a compaction.

    Attributes:
        head_uuid: The first preserved event's uuid.
        anchor_uuid: The anchor event's uuid.
        tail_uuid: The last preserved event's uuid.
    """

    head_uuid: EventUuid | None = None
    anchor_uuid: EventUuid | None = None
    tail_uuid: EventUuid | None = None


@dataclass(frozen=True, slots=True)
class PreservedMessages:
    """The message uuids preserved across a compaction.

    Attributes:
        anchor_uuid: The anchor event's uuid.
        uuids: The uuids of the preserved messages, in order.
        all_uuids: The uuids of every message considered for preservation,
            in order.
    """

    anchor_uuid: EventUuid | None = None
    uuids: tuple[EventUuid, ...] = ()
    all_uuids: tuple[EventUuid, ...] = ()


@dataclass(frozen=True, slots=True)
class CompactBoundary:
    """The typed detail of a ``compact_boundary`` system entry.

    Attributes:
        trigger: What triggered the compaction, such as ``manual`` or ``auto``.
        pre_tokens: The context token count before compaction.
        post_tokens: The context token count after compaction.
        duration_ms: The compaction's wall-clock duration in milliseconds.
        cumulative_dropped_tokens: The session's running total of tokens dropped
            by compactions.
        pre_compact_discovered_tools: The tool names discovered before compaction.
        preserved_segment: The head/anchor/tail segment preserved, when recorded.
        preserved_messages: The preserved message uuids, when recorded.
        logical_parent_uuid: The uuid the post-compaction thread logically
            continues from.
        precomputed: Whether the compaction was precomputed, when recorded.
    """

    trigger: str | None = None
    pre_tokens: int | None = None
    post_tokens: int | None = None
    duration_ms: int | None = None
    cumulative_dropped_tokens: int | None = None
    pre_compact_discovered_tools: tuple[str, ...] = ()
    preserved_segment: PreservedSegment | None = None
    preserved_messages: PreservedMessages | None = None
    logical_parent_uuid: EventUuid | None = None
    precomputed: bool | None = None


@dataclass(frozen=True, slots=True)
class TurnDuration:
    """The typed detail of a ``turn_duration`` system entry.

    Attributes:
        duration_ms: The turn's wall-clock duration in milliseconds.
        message_count: The number of messages in the turn.
        pending_workflow_count: The workflows still pending, when recorded.
        pending_background_agent_count: The background agents still pending, when
            recorded.
    """

    duration_ms: int | None = None
    message_count: int | None = None
    pending_workflow_count: int | None = None
    pending_background_agent_count: int | None = None


@dataclass(frozen=True, slots=True)
class ModelRefusalFallback:
    """The typed detail of a ``model_refusal_fallback`` system entry.

    Attributes:
        api_refusal_category: The refusal category the API reported, when present.
        api_refusal_explanation: The refusal explanation the API reported, when present.
        trigger: What triggered the fallback.
        direction: The fallback direction.
        original_model: The model that refused.
        fallback_model: The model fallen back to.
        retracted_message_uuids: The uuids of messages retracted by the fallback.
        refused_user_message_uuid: The user message uuid that drew the refusal,
            when present.
    """

    api_refusal_category: str | None = None
    api_refusal_explanation: str | None = None
    trigger: str | None = None
    direction: str | None = None
    original_model: str | None = None
    fallback_model: str | None = None
    retracted_message_uuids: tuple[EventUuid, ...] = ()
    refused_user_message_uuid: EventUuid | None = None


@dataclass(frozen=True, slots=True)
class OtherSystemDetail:
    """The catch-all detail for a system entry without a typed subtype.

    Carries the entry's full decoded payload verbatim, so no system entry is
    lossy regardless of subtype.

    Attributes:
        raw: The entry's full decoded payload.
    """

    raw: Mapping[str, Any]


SystemDetail = StopHookSummary | CompactBoundary | TurnDuration | ModelRefusalFallback | OtherSystemDetail


@dataclass(frozen=True, slots=True)
class SystemEvent:
    """A system entry, such as a hook summary or notice.

    Attributes:
        meta: The entry envelope metadata.
        subtype: The system entry's subtype.
        content: The entry's text content, when present.
        level: The entry's severity level, when present.
        detail: The typed detail for the subtype — a :class:`StopHookSummary`,
            :class:`CompactBoundary`, :class:`TurnDuration`, or
            :class:`ModelRefusalFallback` when recognized, else an
            :class:`OtherSystemDetail` carrying the full payload. Always set.
    """

    meta: EntryMeta
    subtype: str
    content: str | None
    level: str | None
    detail: SystemDetail


@dataclass(frozen=True, slots=True)
class ModeEvent:
    """A mode or permission-mode change marker.

    These entries carry only a session id on disk — no uuid, timestamp, or
    other envelope fields — so they hold a :attr:`session_id` directly rather
    than an :class:`EntryMeta`.

    Attributes:
        session_id: The session whose mode changed.
        channel: Which mode channel changed.
        value: The new mode value.
    """

    session_id: SessionId
    channel: Literal["mode", "permission-mode"]
    value: str


@dataclass(frozen=True, slots=True)
class OtherEvent:
    """Any recognized entry without a guaranteed conversational envelope.

    Covers ai-title, last-prompt, summary, queue-operation,
    file-history-snapshot, and similar entry types whose shape carries no
    :class:`EntryMeta`. Attachments are typed separately as
    :class:`AttachmentEvent`.

    Attributes:
        type: The entry's ``type`` field.
        raw: The entry's full decoded payload.
    """

    type: str
    raw: Mapping[str, Any]


@dataclass(frozen=True, slots=True)
class HookSuccess:
    """A hook that fired and exited cleanly, attached to the turn it ran on.

    Attributes:
        hook_name: The hook matcher that fired, e.g. ``PostToolUse:Bash``, or None.
        hook_event: The lifecycle event that triggered it, e.g. ``PostToolUse``, or None.
        tool_use_id: The tool-use the hook ran against, or None for lifecycle hooks.
        command: The hook command line, or None.
        content: The additional context the hook injected, or None.
        stdout: The hook's captured stdout, or None.
        stderr: The hook's captured stderr, or None.
        exit_code: The hook's exit status, or None.
        duration_ms: The hook's wall-clock duration in milliseconds, or None.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    tool_use_id: ToolUseId | None = None
    command: str | None = None
    content: str | None = None
    stdout: str | None = None
    stderr: str | None = None
    exit_code: int | None = None
    duration_ms: int | None = None


@dataclass(frozen=True, slots=True)
class HookBlockingError:
    """A hook that blocked the turn, carrying the structured blocking payload.

    Attributes:
        hook_name: The hook matcher that fired, or None.
        hook_event: The lifecycle event that triggered it, or None.
        tool_use_id: The tool-use the hook ran against, or None.
        blocking_error: The verbatim ``blockingError`` payload, or None.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    tool_use_id: ToolUseId | None = None
    blocking_error: Mapping[str, Any] | None = None


@dataclass(frozen=True, slots=True)
class HookNonBlockingError:
    """A hook that failed without blocking the turn.

    Attributes:
        hook_name: The hook matcher that fired, or None.
        hook_event: The lifecycle event that triggered it, or None.
        tool_use_id: The tool-use the hook ran against, or None.
        command: The hook command line, or None.
        stdout: The hook's captured stdout, or None.
        stderr: The hook's captured stderr, or None.
        exit_code: The hook's exit status, or None.
        duration_ms: The hook's wall-clock duration in milliseconds, or None.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    tool_use_id: ToolUseId | None = None
    command: str | None = None
    stdout: str | None = None
    stderr: str | None = None
    exit_code: int | None = None
    duration_ms: int | None = None


@dataclass(frozen=True, slots=True)
class HookCancelled:
    """A hook the harness cancelled, typically on timeout.

    Attributes:
        hook_name: The hook matcher that fired, or None.
        hook_event: The lifecycle event that triggered it, or None.
        tool_use_id: The tool-use the hook ran against, or None.
        command: The hook command line, or None.
        duration_ms: How long it ran before cancellation, or None.
        timed_out: Whether the cancellation was a timeout, or None.
        timeout_ms: The configured timeout in milliseconds, or None.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    tool_use_id: ToolUseId | None = None
    command: str | None = None
    duration_ms: int | None = None
    timed_out: bool | None = None
    timeout_ms: int | None = None


@dataclass(frozen=True, slots=True)
class HookAdditionalContext:
    """Context a hook injected into the turn without blocking it.

    Attributes:
        hook_name: The hook matcher that fired, or None.
        hook_event: The lifecycle event that triggered it, or None.
        tool_use_id: The tool-use the hook ran against, or None.
        content: The injected context lines, in order.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    tool_use_id: ToolUseId | None = None
    content: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class AsyncHookResponse:
    """The result of an asynchronously-executed hook, matched back by process id.

    Attributes:
        hook_name: The hook matcher that fired, or None.
        hook_event: The lifecycle event that triggered it, or None.
        process_id: The async execution's process id, or None.
        stdout: The hook's captured stdout, or None.
        stderr: The hook's captured stderr, or None.
        exit_code: The hook's exit status, or None.
        response: The verbatim ``response`` payload, or None.
    """

    hook_name: str | None = None
    hook_event: str | None = None
    process_id: str | None = None
    stdout: str | None = None
    stderr: str | None = None
    exit_code: int | None = None
    response: Mapping[str, Any] | None = None


@dataclass(frozen=True, slots=True)
class QueuedCommand:
    """A user command queued for delivery to the agent, replayed as an attachment.

    Attributes:
        prompt: The queued command's prompt text, or None when it carries no
            plain-string prompt (e.g. an image-paste payload).
        command_mode: How the command was queued, e.g. ``prompt`` or
            ``task-notification``, or None.
    """

    prompt: str | None = None
    command_mode: str | None = None


@dataclass(frozen=True, slots=True)
class OtherAttachment:
    """Any attachment whose type has no typed detail, carried verbatim.

    Covers the many informational attachment types (skill/tool/agent listings,
    reminders, plan-mode markers, file references, and anything future) whose
    shape is not further decomposed, mirroring :class:`OtherSystemDetail`.

    Attributes:
        raw: The attachment entry's full decoded payload.
    """

    raw: Mapping[str, Any]


AttachmentDetail = (
    HookSuccess
    | HookBlockingError
    | HookNonBlockingError
    | HookCancelled
    | HookAdditionalContext
    | AsyncHookResponse
    | QueuedCommand
    | OtherAttachment
)


@dataclass(frozen=True, slots=True)
class AttachmentEvent:
    """A harness attachment record — a hook firing, a queued command, or an
    informational injection — carrying the full envelope it was written with.

    Recognized attachment types carry their typed :class:`AttachmentDetail`;
    every other type carries the full record verbatim under
    :class:`OtherAttachment`, so no attachment is lossy.

    Attributes:
        meta: The entry envelope metadata.
        attachment_type: The raw ``attachment.type`` string, e.g. ``hook_success``.
        detail: The typed attachment payload.
    """

    meta: EntryMeta
    attachment_type: str
    detail: AttachmentDetail


@dataclass(frozen=True, slots=True)
class CacheCreation:
    """The split of cache-creation input tokens by TTL bucket.

    Attributes:
        ephemeral_5m_input_tokens: Cache-creation tokens written to the 5-minute TTL bucket.
        ephemeral_1h_input_tokens: Cache-creation tokens written to the 1-hour TTL bucket.
    """

    ephemeral_5m_input_tokens: int
    ephemeral_1h_input_tokens: int


@dataclass(frozen=True, slots=True)
class ServerToolUse:
    """Server-side tool invocation counts billed within a turn.

    Attributes:
        web_search_requests: The number of server-side web-search requests.
        web_fetch_requests: The number of server-side web-fetch requests.
    """

    web_search_requests: int
    web_fetch_requests: int


@dataclass(frozen=True, slots=True)
class Usage:
    """Token usage and cache accounting for a single assistant turn or a -p (print mode) result.

    Exposes both the flat cache_creation_input_tokens and the per-TTL cache_creation
    split, faithfully and without opinion.

    Attributes:
        input_tokens: The number of input tokens consumed by the turn.
        output_tokens: The number of output tokens produced by the turn.
        cache_read_input_tokens: The number of input tokens served from the cache.
        cache_creation_input_tokens: The flat total of input tokens written to the cache.
        cache_creation: The per-TTL split of cache-creation tokens, when present.
        service_tier: The service tier that billed the turn, when present.
        inference_geo: The inference geography that served the turn, when present.
        server_tool_use: Server-side tool invocation counts, when present.
    """

    input_tokens: int
    output_tokens: int
    cache_read_input_tokens: int
    cache_creation_input_tokens: int
    cache_creation: CacheCreation | None
    service_tier: str | None
    inference_geo: str | None
    server_tool_use: ServerToolUse | None


@dataclass(frozen=True, slots=True)
class ModelUsage:
    """Per-model token usage and cost from a -p (print mode) result's modelUsage map.

    Attributes:
        input_tokens: The number of input tokens consumed by the model.
        output_tokens: The number of output tokens produced by the model.
        cache_read_input_tokens: The number of input tokens served from the cache.
        cache_creation_input_tokens: The flat total of input tokens written to the cache.
        web_search_requests: The number of server-side web-search requests billed to the model.
        cost_usd: The cost in USD attributed to the model.
        context_window: The model's context window size in tokens.
        max_output_tokens: The model's maximum output token budget.
    """

    input_tokens: int
    output_tokens: int
    cache_read_input_tokens: int
    cache_creation_input_tokens: int
    web_search_requests: int
    cost_usd: float
    context_window: int
    max_output_tokens: int


@dataclass(frozen=True, slots=True)
class McpServer:
    """An MCP server entry from the -p init element.

    Attributes:
        name: The configured name of the MCP server.
        status: The connection status reported for the server.
    """

    name: str
    status: str


@dataclass(frozen=True, slots=True)
class Plugin:
    """A plugin entry from the -p init element.

    Attributes:
        name: The plugin's name.
        path: The filesystem path the plugin was loaded from.
        source: The source the plugin was installed from.
    """

    name: str
    path: str
    source: str


@dataclass(frozen=True, slots=True)
class InitInfo:
    """The session init snapshot from a -p system/init element.

    Attributes:
        mcp_servers: The MCP servers configured for the session.
        plugins: The plugins loaded for the session.
        tools: The tool names available to the session.
        skills: The skill names available to the session.
    """

    mcp_servers: tuple[McpServer, ...]
    plugins: tuple[Plugin, ...]
    tools: tuple[str, ...]
    skills: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class PrintMessage:
    """A conversational message lifted from a -p (print mode) result.

    Unlike on-disk events it carries no EntryMeta — the -p element shape lacks
    timestamp/parentUuid — so it holds only role, model, text, blocks, and the ids
    that are present.

    Attributes:
        role: The author of the message, either "user" or "assistant".
        model: The model that produced the message, when present.
        text: The flattened text of the message.
        blocks: The structured content blocks of the message.
        uuid: The message's event uuid, when present.
        session_id: The session the message belongs to.
    """

    role: Literal["user", "assistant"]
    model: str | None
    text: str
    blocks: tuple[ContentBlock, ...]
    uuid: EventUuid | None
    session_id: SessionId


@dataclass(frozen=True, slots=True)
class PrintResult:
    """A parsed 'claude -p --output-format json' result.

    Holds the billing/usage/structured-output payload, the init snapshot, and the
    conversational messages. Reuses the shared Usage model; not a TranscriptEvent.

    Attributes:
        total_cost_usd: The total cost in USD for the run.
        model_usage: Per-model usage and cost, keyed by model name.
        usage: The aggregate token usage for the run.
        structured_output: The structured output payload, when present.
        num_turns: The number of turns in the run.
        is_error: Whether the run ended in an error.
        result: The final result text, when present.
        session_id: The session the run belongs to.
        fast_mode_state: The fast-mode state reported for the run, when present.
        stop_reason: The reason the run stopped, when present.
        permission_denials: The permission denials recorded during the run.
        init: The session init snapshot, when present.
        messages: The conversational messages of the run.
    """

    total_cost_usd: float
    model_usage: Mapping[str, ModelUsage]
    usage: Usage
    structured_output: Mapping[str, Any] | None
    num_turns: int
    is_error: bool
    result: str | None
    session_id: SessionId
    fast_mode_state: str | None
    stop_reason: str | None
    permission_denials: tuple[Mapping[str, Any], ...]
    init: InitInfo | None
    messages: tuple[PrintMessage, ...]


TranscriptEvent = UserEvent | AssistantEvent | SystemEvent | ModeEvent | OtherEvent | AttachmentEvent
"""The union of every typed event a parsed transcript can yield."""


def tool_uses(event: UserEvent | AssistantEvent) -> tuple[ToolUseBlock, ...]:
    """The event's tool-use blocks, in content order."""
    return tuple(block for block in event.blocks if isinstance(block, ToolUseBlock))


def thinking_chars(event: UserEvent | AssistantEvent) -> int:
    """The total character count of the event's extended-thinking blocks."""
    return sum(len(block.thinking) for block in event.blocks if isinstance(block, ThinkingBlock))
