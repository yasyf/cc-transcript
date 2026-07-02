"""The single typed tool-call hierarchy shared by every consumer.

A hook author inspecting live stdin, a miner walking a parsed transcript, and
the review gate all see the same object for the same tool call. Each call
retains its raw input mapping (excluded from equality and repr), and the
digest is always derived from that raw substrate — a typed-vs-raw digest fork
is impossible by construction. Import-light by contract: standard library
only.
"""

from __future__ import annotations

import re
import shlex
from dataclasses import dataclass, field
from itertools import dropwhile
from typing import TYPE_CHECKING, Literal

from cc_transcript.ids import ToolDigest, tool_digest

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping
    from typing import Any

TOOL_ALIASES: dict[str, str] = {
    "Bash": "Execute",
    "Write": "Create",
    "Agent": "Task",
    "WebFetch": "FetchUrl",
    "ExitPlanMode": "ExitSpecMode",
}

TOOL_ALIASES_REVERSE: dict[str, str] = {v: k for k, v in TOOL_ALIASES.items()}

MULTI_LEVEL_TOOLS = frozenset(
    {
        "git",
        "gh",
        "uv",
        "uvx",
        "npx",
        "docker",
        "jj",
        "go",
        "cargo",
        "npm",
        "pnpm",
        "yarn",
        "kubectl",
        "pip",
        "brew",
        "aws",
        "gcloud",
        "terraform",
    }
)

WRAPPER_COMMANDS = frozenset({"sudo", "env", "time", "timeout", "nice", "nohup", "doas", "command", "exec", "xargs"})

READ_VERBS = frozenset({"get", "list", "search", "read", "view", "fetch", "query", "describe", "show", "find"})

BASH_OPERATORS = frozenset({"&&", "||", ";", "|", "&"})

SHELL_UNWRAP = frozenset({"do", "then", "else", "elif"})

SHELL_STOP = frozenset(
    {"for", "while", "until", "if", "case", "select", "in", "done", "fi", "esac", "{", "}", "(", ")"}
)

ASSIGNMENT_RE = re.compile(r"^\w+=")

OPERATOR_SPLIT_RE = re.compile(r"&&|\|\||[;|&]")


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
class WorkflowCall(ToolCallBase):
    """A Workflow dynamic-orchestration dispatch.

    Attributes:
        script: The inline workflow script, when passed directly.
        script_path: Path to a script file on disk, when passed instead of
            ``script``.
        workflow_name: A predefined workflow's name (``raw["name"]`` — distinct
            from :attr:`ToolCallBase.name`, the tool name).
        args: The value exposed to the script as its ``args`` global.
        resume_from_run_id: A prior run to resume from.
    """

    script: str | None = None
    script_path: str | None = None
    workflow_name: str | None = None
    args: Any = None
    resume_from_run_id: str | None = None


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


def parse_tool_call(name: str, input: Mapping[str, Any], *, on_error: Literal["raise", "other"] = "raise") -> ToolCall:
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
    return frozenset(
        (base := set(spec.split("|")))
        | {alias for n in base for alias in (TOOL_ALIASES.get(n), TOOL_ALIASES_REVERSE.get(n)) if alias}
    )


def tool_name_matches(actual: str, spec: str) -> bool:
    """Whether ``actual`` matches a pipe spec, honoring aliases and MCP suffixes.

    Example:
        >>> tool_name_matches("Execute", "Bash|Grep")
        True
        >>> tool_name_matches("mcp__github__Grep", "Grep")
        True
    """
    candidates = expand_tool_names(spec)
    return actual in candidates or ((mp := mcp_parts(actual)) is not None and mp[1] in candidates)


def mcp_parts(name: str) -> tuple[str, str] | None:
    """Split an ``mcp__server__tool`` name into ``(server, tool)``, else ``None``.

    Example:
        >>> mcp_parts("mcp__semble__search")
        ('semble', 'search')
        >>> mcp_parts("Bash") is None
        True
    """
    match name.split("__", 2):
        case ["mcp", server, tool]:
            return (server, tool)
        case _:
            return None


def mcp_access(tool: str) -> Literal["read", "write"]:
    """Classify an MCP tool segment as ``"read"`` or ``"write"`` by its verbs.

    Returns ``"read"`` when ``tool`` starts with, or has an underscore-delimited
    token equal to, a read verb (``get``, ``list``, ``search``, …); otherwise
    ``"write"``. The token check catches namespaced names like ``ccx_read``.

    Example:
        >>> mcp_access("search")
        'read'
        >>> mcp_access("ccx_read")
        'read'
        >>> mcp_access("deploy")
        'write'
    """
    lowered = tool.lower()
    return (
        "read" if lowered.startswith(tuple(READ_VERBS)) or any(t in READ_VERBS for t in lowered.split("_")) else "write"
    )


def segment_prefix(segment: list[str]) -> str | None:
    rest = list(
        dropwhile(
            lambda t: t in WRAPPER_COMMANDS or t in SHELL_UNWRAP,
            dropwhile(lambda t: ASSIGNMENT_RE.match(t) is not None, segment),
        )
    )
    if not rest or rest[0] in SHELL_STOP:
        return None
    argv0, *args = rest
    if argv0 not in MULTI_LEVEL_TOOLS:
        return argv0
    return f"{argv0} {sub}" if (sub := next((a for a in args if not a.startswith("-")), None)) else argv0


def split_on_operators(tokens: list[str]) -> Iterator[list[str]]:
    segment: list[str] = []
    for token in tokens:
        if token in BASH_OPERATORS:
            yield segment
            segment = []
        else:
            segment.append(token)
    yield segment


def bash_prefixes(command: str) -> tuple[str, ...]:
    """Command prefixes for each pipeline segment of a shell command.

    Splits ``command`` at shell operators (``&&``, ``||``, ``;``, ``|``, ``&``),
    then per segment drops leading ``VAR=val`` assignments and wrapper commands
    (``sudo``, ``env``, ``timeout``, …) to reach the real command. A multi-level
    tool (``git``, ``docker``, …) keeps its first non-flag argument as the
    subcommand, so ``git commit -m x`` yields ``"git commit"``. A malformed
    command (an unterminated quote) degrades to its first whitespace token.

    Example:
        >>> bash_prefixes("git add . && git commit -m 'x; y'")
        ('git add', 'git commit')
    """
    lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        tokens = list(lexer)
    except ValueError:
        return (argv0,) if (argv0 := next(iter(OPERATOR_SPLIT_RE.split(command)[0].split()), None)) else ()
    return tuple(prefix for segment in split_on_operators(tokens) if (prefix := segment_prefix(segment)) is not None)


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
            case "Workflow":
                return WorkflowCall(
                    name=name,
                    raw=raw,
                    script=raw.get("script"),
                    script_path=raw.get("scriptPath") or raw.get("script_path"),
                    workflow_name=raw.get("name"),
                    args=raw.get("args"),
                    resume_from_run_id=raw.get("resumeFromRunId") or raw.get("resume_from_run_id"),
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
