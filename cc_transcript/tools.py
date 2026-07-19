"""The single typed tool-call hierarchy shared by every consumer.

A hook author inspecting live stdin, a miner walking a parsed transcript, and
the review gate all see the same object for the same tool call. The classes are
native frozen views re-exported at the stable import path; each call retains
its raw input mapping (excluded from equality and repr), and the digest is
always derived from that raw substrate — a typed-vs-raw digest fork is
impossible by construction. Import-light by contract: standard library plus the
native extension only; :attr:`BashCall.command_line` is a native property, so
parsing a command pulls in no Python module or bash grammar on the hook hot path.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Literal

from cc_transcript import _native
from cc_transcript._native import AskUserQuestionResult as AskUserQuestionResult
from cc_transcript._native import BashCall as BashCall
from cc_transcript._native import BashResult as BashResult
from cc_transcript._native import EditCall as EditCall
from cc_transcript._native import EditResult as EditResult
from cc_transcript._native import EditSpan as EditSpan
from cc_transcript._native import ExitPlanModeCall as ExitPlanModeCall
from cc_transcript._native import GlobCall as GlobCall
from cc_transcript._native import GrepCall as GrepCall
from cc_transcript._native import Hunk as Hunk
from cc_transcript._native import MultiEditCall as MultiEditCall
from cc_transcript._native import NotebookEditCall as NotebookEditCall
from cc_transcript._native import OtherCall as OtherCall
from cc_transcript._native import OtherResult as OtherResult
from cc_transcript._native import QuestionAnnotation as QuestionAnnotation
from cc_transcript._native import ReadCall as ReadCall
from cc_transcript._native import ReadResult as ReadResult
from cc_transcript._native import SkillCall as SkillCall
from cc_transcript._native import SkillResult as SkillResult
from cc_transcript._native import TaskCall as TaskCall
from cc_transcript._native import TaskCreateCall as TaskCreateCall
from cc_transcript._native import TaskLaunchResult as TaskLaunchResult
from cc_transcript._native import TaskResult as TaskResult
from cc_transcript._native import TaskResultBase as TaskResultBase
from cc_transcript._native import TaskUpdateCall as TaskUpdateCall
from cc_transcript._native import TextResult as TextResult
from cc_transcript._native import ToolCallBase as ToolCallBase
from cc_transcript._native import ToolResultBase as ToolResultBase
from cc_transcript._native import WorkflowCall as WorkflowCall
from cc_transcript._native import WriteCall as WriteCall
from cc_transcript._native import WriteResult as WriteResult
from cc_transcript._native import expand_tool_names as expand_tool_names
from cc_transcript._native import file_path_of as file_path_of
from cc_transcript._native import hunks_of as hunks_of
from cc_transcript._native import matches_names as matches_names
from cc_transcript._native import mcp_access as mcp_access
from cc_transcript._native import mcp_parts as mcp_parts
from cc_transcript._native import tool_name_matches as tool_name_matches
from cc_transcript.ids import ToolDigest as ToolDigest
from cc_transcript.ids import tool_digest as tool_digest

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import Any

TOOL_ALIASES: dict[str, str] = {
    "Bash": "Execute",
    "Write": "Create",
    "Agent": "Task",
    "WebFetch": "FetchUrl",
    "ExitPlanMode": "ExitSpecMode",
}


class ToolInputError(ValueError):
    """A known tool's input did not match its expected shape."""


class ToolResultError(ValueError):
    """A known tool's result payload did not match its expected shape."""


ToolCall = (
    BashCall
    | EditCall
    | MultiEditCall
    | WriteCall
    | ReadCall
    | NotebookEditCall
    | GrepCall
    | GlobCall
    | TaskCall
    | WorkflowCall
    | SkillCall
    | TaskCreateCall
    | TaskUpdateCall
    | ExitPlanModeCall
    | OtherCall
)


ToolResult = (
    BashResult
    | EditResult
    | WriteResult
    | ReadResult
    | TaskResult
    | TaskLaunchResult
    | SkillResult
    | AskUserQuestionResult
    | TextResult
    | OtherResult
)


@dataclass(frozen=True, slots=True)
class FallbackCall:
    """The ``on_error='other'`` fallback for tool input outside the JSON contract.

    v14 routes input through the native parser as a JSON document, so a mapping
    carrying values JSON cannot express — a ``datetime``, ``bytes``, a reference
    cycle — has no native view. This Python-side stand-in mirrors the
    :class:`OtherCall` surface and holds the original mapping verbatim, so a
    wild-data boundary degrades instead of crashing. :attr:`digest` still derives
    from that mapping, and so raises only when the mapping is itself undigestable.

    Attributes:
        name: The tool name exactly as invoked.
        raw: The original input mapping, verbatim.
        error: The JSON-serialization failure this call degraded from. A
            :class:`FallbackCall` the parse functions build always carries a
            non-None error; excluded from equality and repr.
    """

    name: str
    raw: Mapping[str, Any]
    error: str | None = field(default=None, compare=False, repr=False)

    @property
    def digest(self) -> ToolDigest:
        """The cross-language content digest of this call's raw mapping."""
        return tool_digest(self.name, self.raw)


@dataclass(frozen=True, slots=True)
class FallbackResult:
    """The ``on_error='other'`` fallback for a tool result payload outside the JSON contract.

    Mirrors :class:`OtherResult` and holds the original ``toolUseResult`` payload
    verbatim when it carries values JSON cannot express, so the result lift stays
    total at a wild-data boundary.

    Attributes:
        name: The tool name exactly as invoked.
        raw: The original ``toolUseResult`` payload, verbatim.
        error: The JSON-serialization failure this result degraded from. A
            :class:`FallbackResult` the parse functions build always carries a
            non-None error; excluded from equality and repr.
    """

    name: str
    raw: Mapping[str, Any] | str | None
    error: str | None = field(default=None, compare=False, repr=False)


def parse_tool_call(
    name: str, input: Mapping[str, Any], *, on_error: Literal["raise", "other"] = "raise"
) -> ToolCall | FallbackCall:
    """Parse a tool's name and raw input into the typed hierarchy.

    Strict by default: a known tool whose input is malformed raises
    :class:`ToolInputError` (leniency lives in tests). Malformed covers a
    required field that is missing, explicitly null, or of the wrong runtime
    type — validation happens here at the boundary, never downstream of a
    typed field. The wild-data boundaries
    — the hook runtime and the activity lift — pass ``on_error='other'`` so a
    Claude Code shape change or a model-emitted invalid call degrades to
    :class:`OtherCall` — with a still-correct digest, since the raw mapping is
    the substrate — instead of crashing every hook fire or session lift. A
    non-mapping ``input`` raises under strict mode; under ``on_error='other'``
    it degrades to an :class:`OtherCall` over an empty mapping, carrying the
    non-mapping strict-failure message, whose digest is the empty-input digest.
    On a degraded :class:`OtherCall`, ``error`` is None if and only if the input
    parsed as a mapping but no typed model exists for the tool; any malformed
    payload — a missing, null, or wrong-typed field, or a non-mapping input —
    carries the strict failure message instead.

    v14 contract: ``input`` is decoded-JSON values. It is serialized to a JSON
    document for the native parser, so tuples normalize to lists and non-string
    keys to strings (JSON semantics). Values JSON cannot express — a
    ``datetime``, ``bytes``, a reference cycle — are out of contract: strict mode
    raises, and ``on_error='other'`` degrades to a :class:`FallbackCall` holding
    the original mapping verbatim rather than crashing.

    Example:
        >>> call = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
        >>> call.new
        'y'
    """
    if not isinstance(input, dict):
        if on_error == "raise":
            raise ToolInputError(f"{name} input must be a mapping, got {type(input).__name__}")
        try:
            return _native.toolcall_parse_view(name, json.dumps(input), "other")
        except (TypeError, ValueError):
            return _native.toolcall_parse_view(name, "null", "other")
    # Spans serialization and the native parse: json.dumps emits text (NaN, Infinity,
    # unpaired surrogates) the native JSON parser then rejects, so 'other' catches both.
    try:
        return _native.toolcall_parse_view(name, json.dumps(input), on_error)
    except (TypeError, ValueError) as exc:
        if on_error == "raise":
            raise
        return FallbackCall(name=name, raw=input, error=str(exc))


def parse_tool_result(
    name: str, payload: Mapping[str, Any] | str | None, *, on_error: Literal["raise", "other"] = "raise"
) -> ToolResult | FallbackResult:
    """Parse a tool's name and record-level ``toolUseResult`` into the typed hierarchy.

    The payload is the verbatim ``toolUseResult`` — dict, string, or absent.
    A plain-string payload (a denial) is a :class:`TextResult`; a payload for a
    tool the platform does not type is an :class:`OtherResult`, as is an absent
    (None) payload. The native result lift is total, so ``on_error`` fires only
    when the payload carries values JSON cannot express: strict mode raises,
    ``on_error='other'`` degrades to a :class:`FallbackResult` holding the
    original payload verbatim.

    Example:
        >>> parse_tool_result("Bash", {"stdout": "hi", "stderr": ""}).stdout
        'hi'
    """
    try:
        return _native.toolresult_parse_view(name, json.dumps(payload))
    except (TypeError, ValueError) as exc:
        if on_error == "raise":
            raise
        return FallbackResult(name=name, raw=payload, error=str(exc))
