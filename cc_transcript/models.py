"""Typed superset event model — the native lazy views, re-exported at the stable import path.

Every class here is a frozen Rust-backed view over the shared parse output:
fields resolve on access instead of materializing at parse time. Import paths,
class names, field names, and semantics match the pre-inversion dataclasses;
``isinstance`` and keyword ``match`` patterns work unchanged.
"""

from __future__ import annotations

from typing import NewType

from cc_transcript import _native
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

    Only the top level is frozen, and only against ordinary mutation. Reaching
    past the instance (``dict.__setitem__(x, ...)``, ``dict.__init__``) or
    mutating a nested container is deliberate misuse and out of contract: it can
    resurrect the split-brain and the uncollectable cycle this type prevents.
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


TextBlock = _native.TextBlock
ThinkingBlock = _native.ThinkingBlock
Question = _native.Question
ToolUseBlock = _native.ToolUseBlock
ToolResultBlock = _native.ToolResultBlock
FallbackBlock = _native.FallbackBlock
OtherBlock = _native.OtherBlock

ContentBlock = TextBlock | ThinkingBlock | ToolUseBlock | ToolResultBlock | FallbackBlock | OtherBlock

EntryMeta = _native.EntryMeta
UserEvent = _native.UserEvent
Attribution = _native.Attribution
ApiError = _native.ApiError
AssistantEvent = _native.AssistantEvent

HookInfo = _native.HookInfo
StopHookSummary = _native.StopHookSummary
PreservedSegment = _native.PreservedSegment
PreservedMessages = _native.PreservedMessages
CompactBoundary = _native.CompactBoundary
TurnDuration = _native.TurnDuration
ModelRefusalFallback = _native.ModelRefusalFallback
OtherSystemDetail = _native.OtherSystemDetail

SystemDetail = StopHookSummary | CompactBoundary | TurnDuration | ModelRefusalFallback | OtherSystemDetail

SystemEvent = _native.SystemEvent
ModeEvent = _native.ModeEvent
OtherEvent = _native.OtherEvent

HookSuccess = _native.HookSuccess
HookBlockingError = _native.HookBlockingError
HookNonBlockingError = _native.HookNonBlockingError
HookCancelled = _native.HookCancelled
HookAdditionalContext = _native.HookAdditionalContext
AsyncHookResponse = _native.AsyncHookResponse
QueuedCommand = _native.QueuedCommand
OtherAttachment = _native.OtherAttachment

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

AttachmentEvent = _native.AttachmentEvent

CacheCreation = _native.CacheCreation
ServerToolUse = _native.ServerToolUse
Usage = _native.Usage
ModelUsage = _native.ModelUsage
McpServer = _native.McpServer
Plugin = _native.Plugin
InitInfo = _native.InitInfo
PrintMessage = _native.PrintMessage
PrintResult = _native.PrintResult

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
