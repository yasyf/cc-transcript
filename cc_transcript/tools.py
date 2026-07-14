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

from cc_transcript import _parser_rs
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


Hunk = _parser_rs.Hunk
EditSpan = _parser_rs.EditSpan
ToolCallBase = _parser_rs.ToolCallBase
BashCall = _parser_rs.BashCall
EditCall = _parser_rs.EditCall
MultiEditCall = _parser_rs.MultiEditCall
WriteCall = _parser_rs.WriteCall
ReadCall = _parser_rs.ReadCall
NotebookEditCall = _parser_rs.NotebookEditCall
GrepCall = _parser_rs.GrepCall
GlobCall = _parser_rs.GlobCall
TaskCall = _parser_rs.TaskCall
WorkflowCall = _parser_rs.WorkflowCall
SkillCall = _parser_rs.SkillCall
TaskCreateCall = _parser_rs.TaskCreateCall
TaskUpdateCall = _parser_rs.TaskUpdateCall
ExitPlanModeCall = _parser_rs.ExitPlanModeCall
OtherCall = _parser_rs.OtherCall

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

QuestionAnnotation = _parser_rs.QuestionAnnotation
ToolResultBase = _parser_rs.ToolResultBase
BashResult = _parser_rs.BashResult
EditResult = _parser_rs.EditResult
WriteResult = _parser_rs.WriteResult
ReadResult = _parser_rs.ReadResult
TaskResultBase = _parser_rs.TaskResultBase
TaskResult = _parser_rs.TaskResult
TaskLaunchResult = _parser_rs.TaskLaunchResult
SkillResult = _parser_rs.SkillResult
AskUserQuestionResult = _parser_rs.AskUserQuestionResult
TextResult = _parser_rs.TextResult
OtherResult = _parser_rs.OtherResult

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

hunks_of = _parser_rs.hunks_of
file_path_of = _parser_rs.file_path_of
expand_tool_names = _parser_rs.expand_tool_names
matches_names = _parser_rs.matches_names
tool_name_matches = _parser_rs.tool_name_matches
mcp_parts = _parser_rs.mcp_parts
mcp_access = _parser_rs.mcp_access


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
        return _parser_rs.toolcall_parse_view(name, "{}", "other")
    try:
        serialized = json.dumps(input)
    except (TypeError, ValueError):
        if on_error == "raise":
            raise
        return FallbackCall(name=name, raw=input)
    return _parser_rs.toolcall_parse_view(name, serialized, on_error)


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
        serialized = json.dumps(payload)
    except (TypeError, ValueError):
        if on_error == "raise":
            raise
        return FallbackResult(name=name, raw=payload)
    return _parser_rs.toolresult_parse_view(name, serialized)
