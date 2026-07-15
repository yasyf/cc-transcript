"""The single typed tool-call hierarchy shared by every consumer.

A hook author inspecting live stdin, a miner walking a parsed transcript, and
the review gate all see the same object for the same tool call. The classes are
native frozen views re-exported at the stable import path; each call retains
its raw input mapping (excluded from equality and repr), and the digest is
always derived from that raw substrate — a typed-vs-raw digest fork is
impossible by construction. Import-light by contract: standard library plus the
native extension only; :attr:`BashCall.command_line` reaches into
``cc_transcript.command`` lazily so the tree-sitter grammar never loads on the
hook hot path.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
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

TOOL_ALIASES_REVERSE: dict[str, str] = {v: k for k, v in TOOL_ALIASES.items()}

# cc-context routes real file edits through MCP tools whose bare names match no
# builtin edit gate; alias its write surface to the builtins those gates watch so
# an edit through the MCP can't slip past a Tool("Edit"/"Write"/"MultiEdit") guard.
MCP_TOOL_ALIASES: dict[str, str] = {
    "ccx_code_edit": "Edit",
    "ccx_code_replace": "Write",
}

READ_VERBS = frozenset({"get", "list", "search", "read", "view", "fetch", "query", "describe", "show", "find"})


def key_of(raw: Mapping[str, Any], *keys: str) -> Any | None:
    """The first non-null value among ``keys`` in ``raw``, else None."""
    return next((value for key in keys if (value := raw.get(key)) is not None), None)


def required_key(raw: Mapping[str, Any], *keys: str) -> Any:
    """The first non-null value among ``keys`` in ``raw``; raises ``KeyError`` if none is present."""
    if (value := key_of(raw, *keys)) is None:
        raise KeyError(keys[0])
    return value


def required_str(raw: Mapping[str, Any], *keys: str) -> str:
    """The first non-null value among ``keys`` in ``raw`` as a str; raises if absent or not a str."""
    if not isinstance(value := required_key(raw, *keys), str):
        raise TypeError(f"{keys[0]} must be a str, got {type(value).__name__}")
    return value


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

TOOL_TYPES: dict[str, type] = {
    "Bash": BashCall,
    "Edit": EditCall,
    "MultiEdit": MultiEditCall,
    "Write": WriteCall,
    "Read": ReadCall,
    "NotebookEdit": NotebookEditCall,
    "Grep": GrepCall,
    "Glob": GlobCall,
    "Agent": TaskCall,
    "Workflow": WorkflowCall,
    "Skill": SkillCall,
    "TaskCreate": TaskCreateCall,
    "TaskUpdate": TaskUpdateCall,
    "ExitPlanMode": ExitPlanModeCall,
}


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

TOOL_RESULT_TYPES: dict[str, type] = {
    "Bash": BashResult,
    "Edit": EditResult,
    "Write": WriteResult,
    "Read": ReadResult,
    "Agent": TaskResultBase,
    "Skill": SkillResult,
    "AskUserQuestion": AskUserQuestionResult,
}


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
    """

    name: str
    raw: Mapping[str, Any]

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
    """

    name: str
    raw: Mapping[str, Any] | str | None


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
    it degrades to an :class:`OtherCall` over an empty mapping, whose digest is
    the empty-input digest.

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
        return _native.toolcall_parse_view(name, "{}", "other")
    # Spans serialization and the native parse: json.dumps emits text (NaN, Infinity,
    # unpaired surrogates) the native JSON parser then rejects, so 'other' catches both.
    try:
        return _native.toolcall_parse_view(name, json.dumps(input), on_error)
    except (TypeError, ValueError):
        if on_error == "raise":
            raise
        return FallbackCall(name=name, raw=input)


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
    except (TypeError, ValueError):
        if on_error == "raise":
            raise
        return FallbackResult(name=name, raw=payload)
