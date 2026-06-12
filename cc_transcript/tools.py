"""The single typed tool-call hierarchy shared by every consumer.

A hook author inspecting live stdin, a miner walking a parsed transcript, and
the review gate all see the same object for the same tool call. Each call
retains its raw input mapping (excluded from equality and repr), and the
digest is always derived from that raw substrate — a typed-vs-raw digest fork
is impossible by construction. Import-light by contract: standard library
only.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Literal

from cc_transcript.ids import ToolDigest, tool_digest

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


class ToolInputError(ValueError):
    """A known tool's input did not match its expected shape."""


@dataclass(frozen=True, slots=True)
class Hunk:
    """A before/after content pair lowered from an edit-shaped tool call.

    Attributes:
        old: The content replaced; empty for pure additions such as Write.
        new: The content written.
    """

    old: str
    new: str


@dataclass(frozen=True, slots=True)
class EditSpan:
    """One replacement within a MultiEdit call, in application order."""

    old: str
    new: str
    replace_all: bool = False


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolCallBase:
    """Common shape of every typed tool call.

    Attributes:
        name: The tool name exactly as invoked (aliases are not normalized —
            the digest must match what the hook saw).
        raw: The verbatim input mapping; the only digest substrate.
    """

    name: str
    raw: Mapping[str, Any] = field(repr=False, compare=False)

    @property
    def digest(self) -> ToolDigest:
        """The cross-language content digest of this call."""
        return tool_digest(self.name, self.raw)


@dataclass(frozen=True, slots=True, kw_only=True)
class BashCall(ToolCallBase):
    """A Bash/Execute shell invocation."""

    command: str
    timeout: int | None = None
    description: str | None = None
    run_in_background: bool | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class EditCall(ToolCallBase):
    """An Edit replacement of ``old`` with ``new`` in one file."""

    file_path: str
    old: str
    new: str
    replace_all: bool = False


@dataclass(frozen=True, slots=True, kw_only=True)
class MultiEditCall(ToolCallBase):
    """A MultiEdit applying ``edits`` to one file, in order."""

    file_path: str
    edits: tuple[EditSpan, ...]


@dataclass(frozen=True, slots=True, kw_only=True)
class WriteCall(ToolCallBase):
    """A Write/Create of a whole file."""

    file_path: str
    content: str


@dataclass(frozen=True, slots=True, kw_only=True)
class ReadCall(ToolCallBase):
    """A Read of a file, optionally windowed."""

    file_path: str
    offset: int | None = None
    limit: int | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class NotebookEditCall(ToolCallBase):
    """A NotebookEdit replacing a cell's source."""

    notebook_path: str
    new_source: str
    cell_id: str | None = None
    edit_mode: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class GrepCall(ToolCallBase):
    """A Grep content search."""

    pattern: str
    path: str | None = None
    glob: str | None = None
    file_type: str | None = None
    output_mode: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class GlobCall(ToolCallBase):
    """A Glob file-pattern search."""

    pattern: str
    path: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskCall(ToolCallBase):
    """An Agent/Task subagent dispatch."""

    prompt: str
    agent_type: str | None = None
    model: str | None = None
    agent_name: str | None = None
    run_in_background: bool | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class SkillCall(ToolCallBase):
    """A Skill invocation."""

    skill: str
    args: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskCreateCall(ToolCallBase):
    """A TaskCreate tracker entry."""

    subject: str
    description: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskUpdateCall(ToolCallBase):
    """A TaskUpdate tracker change."""

    task_id: str
    status: str | None = None
    subject: str | None = None
    description: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class ExitPlanModeCall(ToolCallBase):
    """An ExitPlanMode/ExitSpecMode plan submission."""

    plan: str


@dataclass(frozen=True, slots=True, kw_only=True)
class OtherCall(ToolCallBase):
    """A tool the platform does not type: unknown names, MCP tools, and — under
    ``on_error='other'`` — known tools whose input failed to parse."""

    def get(self, key: str, default: Any = None) -> Any:
        return self.raw.get(key, default)


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
    | SkillCall
    | TaskCreateCall
    | TaskUpdateCall
    | ExitPlanModeCall
    | OtherCall
)


def parse_tool_call(
    name: str, input: Mapping[str, Any], *, on_error: Literal["raise", "other"] = "raise"
) -> ToolCall:
    """Parse a tool's name and raw input into the typed hierarchy.

    Strict by default: a known tool whose input is malformed raises
    :class:`ToolInputError` (leniency lives in tests). The wild-data boundaries
    — the hook runtime and the activity lift — pass ``on_error='other'`` so a
    Claude Code shape change or a model-emitted invalid call degrades to
    :class:`OtherCall` — with a still-correct digest, since the raw mapping is
    the substrate — instead of crashing every hook fire or session lift.

    Example:
        >>> call = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
        >>> call.new
        'y'
    """
    try:
        return typed_tool_call(name, input)
    except ToolInputError:
        if on_error == "raise":
            raise
        return OtherCall(name=name, raw=input)


def hunks_of(call: ToolCall) -> tuple[Hunk, ...]:
    """Lower an edit-shaped call to before/after hunks; ``()`` for the rest.

    MultiEdit yields one hunk per span in application order — never just the
    first. Write and NotebookEdit are pure additions with an empty old side.
    """
    match call:
        case EditCall(old=old, new=new):
            return (Hunk(old, new),)
        case MultiEditCall(edits=edits):
            return tuple(Hunk(span.old, span.new) for span in edits)
        case WriteCall(content=content):
            return (Hunk("", content),)
        case NotebookEditCall(new_source=new_source):
            return (Hunk("", new_source),)
        case _:
            return ()


def file_path_of(call: ToolCall) -> str | None:
    """The file a call targets, when it targets one."""
    match call:
        case EditCall() | MultiEditCall() | WriteCall() | ReadCall():
            return call.file_path
        case NotebookEditCall(notebook_path=notebook_path):
            return notebook_path
        case _:
            return None


def expand_tool_names(spec: str) -> frozenset[str]:
    """Expand a pipe-separated tool spec to include both alias spellings."""
    base = set(spec.split("|"))
    return frozenset(base) | {
        alias for n in base for alias in (TOOL_ALIASES.get(n), TOOL_ALIASES_REVERSE.get(n)) if alias
    }


def tool_name_matches(actual: str, spec: str) -> bool:
    """Whether ``actual`` matches a pipe spec, honoring aliases and MCP suffixes.

    Example:
        >>> tool_name_matches("Execute", "Bash|Grep")
        True
        >>> tool_name_matches("mcp__github__Grep", "Grep")
        True
    """
    candidates = expand_tool_names(spec)
    return actual in candidates or (
        actual.startswith("mcp__") and len(parts := actual.split("__", 2)) == 3 and parts[2] in candidates
    )


def typed_tool_call(name: str, raw: Mapping[str, Any]) -> ToolCall:
    if not isinstance(raw, dict):
        raise ToolInputError(f"{name} input must be a mapping, got {type(raw).__name__}")
    try:
        match TOOL_ALIASES_REVERSE.get(name, name):
            case "Bash":
                return BashCall(
                    name=name,
                    raw=raw,
                    command=raw["command"],
                    timeout=raw.get("timeout"),
                    description=raw.get("description"),
                    run_in_background=raw.get("run_in_background"),
                )
            case "Edit":
                return EditCall(
                    name=name,
                    raw=raw,
                    file_path=raw["file_path"],
                    old=raw["old_string"],
                    new=raw["new_string"],
                    replace_all=raw.get("replace_all", False),
                )
            case "MultiEdit":
                return MultiEditCall(
                    name=name,
                    raw=raw,
                    file_path=raw["file_path"],
                    edits=tuple(
                        EditSpan(span["old_string"], span["new_string"], span.get("replace_all", False))
                        for span in raw["edits"]
                    ),
                )
            case "Write":
                return WriteCall(name=name, raw=raw, file_path=raw["file_path"], content=raw["content"])
            case "Read":
                return ReadCall(
                    name=name, raw=raw, file_path=raw["file_path"], offset=raw.get("offset"), limit=raw.get("limit")
                )
            case "NotebookEdit":
                return NotebookEditCall(
                    name=name,
                    raw=raw,
                    notebook_path=raw["notebook_path"],
                    new_source=raw["new_source"],
                    cell_id=raw.get("cell_id"),
                    edit_mode=raw.get("edit_mode"),
                )
            case "Grep":
                return GrepCall(
                    name=name,
                    raw=raw,
                    pattern=raw["pattern"],
                    path=raw.get("path"),
                    glob=raw.get("glob"),
                    file_type=raw.get("type"),
                    output_mode=raw.get("output_mode"),
                )
            case "Glob":
                return GlobCall(name=name, raw=raw, pattern=raw["pattern"], path=raw.get("path"))
            case "Agent":
                return TaskCall(
                    name=name,
                    raw=raw,
                    prompt=raw["prompt"],
                    agent_type=raw.get("subagent_type") or raw.get("agent_type"),
                    model=raw.get("model"),
                    agent_name=raw.get("name"),
                    run_in_background=raw.get("run_in_background"),
                )
            case "Skill":
                return SkillCall(name=name, raw=raw, skill=raw["skill"], args=raw.get("args"))
            case "TaskCreate":
                return TaskCreateCall(name=name, raw=raw, subject=raw["subject"], description=raw.get("description"))
            case "TaskUpdate":
                return TaskUpdateCall(
                    name=name,
                    raw=raw,
                    task_id=raw.get("taskId") or raw["task_id"],
                    status=raw.get("status"),
                    subject=raw.get("subject"),
                    description=raw.get("description"),
                )
            case "ExitPlanMode":
                return ExitPlanModeCall(name=name, raw=raw, plan=raw["plan"])
            case _:
                return OtherCall(name=name, raw=raw)
    except (KeyError, TypeError) as error:
        raise ToolInputError(f"{name} input missing or malformed: {error}") from error
