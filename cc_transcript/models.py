"""Typed superset event model — the native lazy views, re-exported at the stable import path.

Every class here is a frozen Rust-backed view over the shared parse output:
fields resolve on access instead of materializing at parse time. Import paths,
class names, field names, and semantics match the pre-inversion dataclasses;
``isinstance`` and keyword ``match`` patterns work unchanged.
"""

from __future__ import annotations

from typing import NewType

from cc_transcript import _parser_rs
from cc_transcript.ids import EventUuid, SessionId, ToolDigest, ToolUseId
from cc_transcript.ids import tool_digest as tool_digest
from cc_transcript.tools import ToolCall as ToolCall
from cc_transcript.tools import parse_tool_call as parse_tool_call

CcVersion = NewType("CcVersion", str)


class ReadOnlyDict(dict):
    """A ``dict`` that rejects mutation — the v14 read-only view of a tool input.

    :attr:`ToolUseBlock.input` and :attr:`ToolResultBlock.tool_use_result` are
    views over immutable parse output. Wrapping the memoized mapping in this type
    keeps it a ``dict`` — so every dict-consuming path (``json``/``orjson``,
    digest canonicalization, ``isinstance``) keeps working and it reprs as
    ``{...}`` — while raising ``TypeError`` on any mutation, which also makes a
    reference cycle back through the untracked view unconstructible.
    """

    __slots__ = ()

    def read_only(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    __setitem__ = read_only
    __delitem__ = read_only
    clear = read_only
    pop = read_only
    popitem = read_only
    setdefault = read_only
    update = read_only
    __ior__ = read_only


TextBlock = _parser_rs.TextBlock
ThinkingBlock = _parser_rs.ThinkingBlock
Question = _parser_rs.Question
ToolUseBlock = _parser_rs.ToolUseBlock
ToolResultBlock = _parser_rs.ToolResultBlock
FallbackBlock = _parser_rs.FallbackBlock
OtherBlock = _parser_rs.OtherBlock

ContentBlock = TextBlock | ThinkingBlock | ToolUseBlock | ToolResultBlock | FallbackBlock | OtherBlock

EntryMeta = _parser_rs.EntryMeta
UserEvent = _parser_rs.UserEvent
Attribution = _parser_rs.Attribution
ApiError = _parser_rs.ApiError
AssistantEvent = _parser_rs.AssistantEvent

HookInfo = _parser_rs.HookInfo
StopHookSummary = _parser_rs.StopHookSummary
PreservedSegment = _parser_rs.PreservedSegment
PreservedMessages = _parser_rs.PreservedMessages
CompactBoundary = _parser_rs.CompactBoundary
TurnDuration = _parser_rs.TurnDuration
ModelRefusalFallback = _parser_rs.ModelRefusalFallback
OtherSystemDetail = _parser_rs.OtherSystemDetail

SystemDetail = StopHookSummary | CompactBoundary | TurnDuration | ModelRefusalFallback | OtherSystemDetail

SystemEvent = _parser_rs.SystemEvent
ModeEvent = _parser_rs.ModeEvent
OtherEvent = _parser_rs.OtherEvent

HookSuccess = _parser_rs.HookSuccess
HookBlockingError = _parser_rs.HookBlockingError
HookNonBlockingError = _parser_rs.HookNonBlockingError
HookCancelled = _parser_rs.HookCancelled
HookAdditionalContext = _parser_rs.HookAdditionalContext
AsyncHookResponse = _parser_rs.AsyncHookResponse
QueuedCommand = _parser_rs.QueuedCommand
OtherAttachment = _parser_rs.OtherAttachment

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

AttachmentEvent = _parser_rs.AttachmentEvent

CacheCreation = _parser_rs.CacheCreation
ServerToolUse = _parser_rs.ServerToolUse
Usage = _parser_rs.Usage
ModelUsage = _parser_rs.ModelUsage
McpServer = _parser_rs.McpServer
Plugin = _parser_rs.Plugin
InitInfo = _parser_rs.InitInfo
PrintMessage = _parser_rs.PrintMessage
PrintResult = _parser_rs.PrintResult

TranscriptEvent = UserEvent | AssistantEvent | SystemEvent | ModeEvent | OtherEvent | AttachmentEvent
"""The union of every typed event a parsed transcript can yield."""


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
            labels=[
                label
                for option in (q.get("options") if isinstance(q.get("options"), list) else ())
                if isinstance(option, dict) and isinstance(label := option.get("label"), str)
            ],
        )
        for q in rounds
        if isinstance(q, dict) and isinstance(text := q.get("question"), str)
    )


def tool_uses(event: UserEvent | AssistantEvent) -> tuple[ToolUseBlock, ...]:
    """The event's tool-use blocks, in content order."""
    return tuple(block for block in event.blocks if isinstance(block, ToolUseBlock))


def thinking_chars(event: UserEvent | AssistantEvent) -> int:
    """The total character count of the event's extended-thinking blocks."""
    return sum(len(block.thinking) for block in event.blocks if isinstance(block, ThinkingBlock))
