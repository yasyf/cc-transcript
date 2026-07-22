"""Typed superset event model — the native lazy views, re-exported at the stable import path.

Every class here is a frozen Rust-backed view over the shared parse output:
fields resolve on access instead of materializing at parse time. Import paths,
class names, field names, and semantics match the pre-inversion dataclasses;
``isinstance`` and keyword ``match`` patterns work unchanged.
"""

from __future__ import annotations

from typing import NewType

from cc_transcript._native import ApiError as ApiError
from cc_transcript._native import AssistantEvent as AssistantEvent
from cc_transcript._native import AsyncHookResponse as AsyncHookResponse
from cc_transcript._native import AttachmentEvent as AttachmentEvent
from cc_transcript._native import Attribution as Attribution
from cc_transcript._native import CacheCreation as CacheCreation
from cc_transcript._native import CompactBoundary as CompactBoundary
from cc_transcript._native import DeferredToolsDelta as DeferredToolsDelta
from cc_transcript._native import EntryMeta as EntryMeta
from cc_transcript._native import EventList as EventList
from cc_transcript._native import FallbackBlock as FallbackBlock
from cc_transcript._native import HookAdditionalContext as HookAdditionalContext
from cc_transcript._native import HookBlockingError as HookBlockingError
from cc_transcript._native import HookCancelled as HookCancelled
from cc_transcript._native import HookInfo as HookInfo
from cc_transcript._native import HookNonBlockingError as HookNonBlockingError
from cc_transcript._native import HookSuccess as HookSuccess
from cc_transcript._native import InitInfo as InitInfo
from cc_transcript._native import McpServer as McpServer
from cc_transcript._native import ModeEvent as ModeEvent
from cc_transcript._native import ModelRefusalFallback as ModelRefusalFallback
from cc_transcript._native import ModelUsage as ModelUsage
from cc_transcript._native import OtherAttachment as OtherAttachment
from cc_transcript._native import OtherBlock as OtherBlock
from cc_transcript._native import OtherEvent as OtherEvent
from cc_transcript._native import OtherSystemDetail as OtherSystemDetail
from cc_transcript._native import Plugin as Plugin
from cc_transcript._native import PreservedMessages as PreservedMessages
from cc_transcript._native import PreservedSegment as PreservedSegment
from cc_transcript._native import PrintMessage as PrintMessage
from cc_transcript._native import PrintResult as PrintResult
from cc_transcript._native import Question as Question
from cc_transcript._native import QueuedCommand as QueuedCommand
from cc_transcript._native import ServerToolUse as ServerToolUse
from cc_transcript._native import StopHookSummary as StopHookSummary
from cc_transcript._native import SystemEvent as SystemEvent
from cc_transcript._native import TextBlock as TextBlock
from cc_transcript._native import ThinkingBlock as ThinkingBlock
from cc_transcript._native import ToolResultBlock as ToolResultBlock
from cc_transcript._native import ToolUseBlock as ToolUseBlock
from cc_transcript._native import Transcript as Transcript
from cc_transcript._native import TurnDuration as TurnDuration
from cc_transcript._native import Usage as Usage
from cc_transcript._native import UserEvent as UserEvent
from cc_transcript.ids import EventUuid as EventUuid
from cc_transcript.ids import SessionId as SessionId
from cc_transcript.ids import ToolDigest as ToolDigest
from cc_transcript.ids import ToolUseId as ToolUseId
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

    def __setitem__(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def __delitem__(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def __ior__(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def clear(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def pop(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def popitem(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def setdefault(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")

    def update(self, *args: object, **kwargs: object) -> None:
        raise TypeError("ReadOnlyDict is read-only")


ContentBlock = TextBlock | ThinkingBlock | ToolUseBlock | ToolResultBlock | FallbackBlock | OtherBlock


SystemDetail = StopHookSummary | CompactBoundary | TurnDuration | ModelRefusalFallback | OtherSystemDetail


AttachmentDetail = (
    HookSuccess
    | HookBlockingError
    | HookNonBlockingError
    | HookCancelled
    | HookAdditionalContext
    | AsyncHookResponse
    | QueuedCommand
    | DeferredToolsDelta
    | OtherAttachment
)


TranscriptEvent = UserEvent | AssistantEvent | SystemEvent | ModeEvent | OtherEvent | AttachmentEvent
"""The union of every typed event a parsed transcript can yield."""


def tool_uses(event: UserEvent | AssistantEvent) -> tuple[ToolUseBlock, ...]:
    """The event's tool-use blocks, in content order."""
    return tuple(block for block in event.blocks if isinstance(block, ToolUseBlock))


def thinking_chars(event: UserEvent | AssistantEvent) -> int:
    """The total character count of the event's extended-thinking blocks."""
    return sum(len(block.thinking) for block in event.blocks if isinstance(block, ThinkingBlock))
