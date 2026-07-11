"""The single typed tool-call hierarchy shared by every consumer.

A hook author inspecting live stdin, a miner walking a parsed transcript, and
the review gate all see the same object for the same tool call. Each call
retains its raw input mapping (excluded from equality and repr), and the
digest is always derived from that raw substrate — a typed-vs-raw digest fork
is impossible by construction. Import-light by contract: standard library
only; :attr:`BashCall.command_line` reaches into ``cc_transcript.command``
lazily so the tree-sitter grammar never loads on the hook hot path.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Literal

from cc_transcript.ids import ToolDigest, tool_digest

if TYPE_CHECKING:
    from collections.abc import Container
    from typing import Any, Self

    from cc_transcript.command import CommandLine
    from cc_transcript.models import Question

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
    return next((value for key in keys if (value := raw.get(key)) is not None), None)


def required_key(raw: Mapping[str, Any], *keys: str) -> Any:
    if (value := key_of(raw, *keys)) is None:
        raise KeyError(keys[0])
    return value


def required_str(raw: Mapping[str, Any], *keys: str) -> str:
    if not isinstance(value := required_key(raw, *keys), str):
        raise TypeError(f"{keys[0]} must be a str, got {type(value).__name__}")
    return value


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

    @classmethod
    def from_raw(cls, span: object) -> Self:
        match span:
            case {"old_string": str() as old, "new_string": str() as new}:
                return cls(old, new, span.get("replace_all", False))
            case _:
                raise TypeError(f"edit span missing or malformed: {span!r}")


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

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        """Extract this call type's fields from the tool ``name`` and raw input."""
        return cls(name=name, raw=raw)

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

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            command=required_str(raw, "command"),
            timeout=raw.get("timeout"),
            description=raw.get("description"),
            run_in_background=raw.get("run_in_background"),
        )

    @property
    def command_line(self) -> CommandLine:
        """The command parsed into a :class:`~cc_transcript.command.CommandLine`."""
        from cc_transcript.command import parse_command_line

        return parse_command_line(self.command)


@dataclass(frozen=True, slots=True, kw_only=True)
class EditCall(ToolCallBase):
    """An Edit replacement of ``old`` with ``new`` in one file."""

    file_path: str
    old: str
    new: str
    replace_all: bool = False

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            file_path=required_str(raw, "file_path"),
            old=required_str(raw, "old_string"),
            new=required_str(raw, "new_string"),
            replace_all=raw.get("replace_all", False),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class MultiEditCall(ToolCallBase):
    """A MultiEdit applying ``edits`` to one file, in order."""

    file_path: str
    edits: tuple[EditSpan, ...]

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            file_path=required_str(raw, "file_path"),
            edits=tuple(EditSpan.from_raw(span) for span in raw["edits"]),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class WriteCall(ToolCallBase):
    """A Write/Create of a whole file."""

    file_path: str
    content: str

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, file_path=required_str(raw, "file_path"), content=required_str(raw, "content"))


@dataclass(frozen=True, slots=True, kw_only=True)
class ReadCall(ToolCallBase):
    """A Read of a file, optionally windowed."""

    file_path: str
    offset: int | None = None
    limit: int | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            file_path=required_str(raw, "file_path"),
            offset=raw.get("offset"),
            limit=raw.get("limit"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class NotebookEditCall(ToolCallBase):
    """A NotebookEdit replacing a cell's source."""

    notebook_path: str
    new_source: str
    cell_id: str | None = None
    edit_mode: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            notebook_path=required_str(raw, "notebook_path"),
            new_source=required_str(raw, "new_source"),
            cell_id=raw.get("cell_id"),
            edit_mode=raw.get("edit_mode"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class GrepCall(ToolCallBase):
    """A Grep content search."""

    pattern: str
    path: str | None = None
    glob: str | None = None
    file_type: str | None = None
    output_mode: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            pattern=required_str(raw, "pattern"),
            path=raw.get("path"),
            glob=raw.get("glob"),
            file_type=raw.get("type"),
            output_mode=raw.get("output_mode"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class GlobCall(ToolCallBase):
    """A Glob file-pattern search."""

    pattern: str
    path: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, pattern=required_str(raw, "pattern"), path=raw.get("path"))


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskCall(ToolCallBase):
    """An Agent/Task subagent dispatch."""

    prompt: str
    agent_type: str | None = None
    model: str | None = None
    agent_name: str | None = None
    run_in_background: bool | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            prompt=required_str(raw, "prompt"),
            agent_type=key_of(raw, "subagent_type", "agent_type"),
            model=raw.get("model"),
            agent_name=raw.get("name"),
            run_in_background=raw.get("run_in_background"),
        )


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

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            script=raw.get("script"),
            script_path=key_of(raw, "scriptPath", "script_path"),
            workflow_name=raw.get("name"),
            args=raw.get("args"),
            resume_from_run_id=key_of(raw, "resumeFromRunId", "resume_from_run_id"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class SkillCall(ToolCallBase):
    """A Skill invocation."""

    skill: str
    args: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, skill=required_str(raw, "skill"), args=raw.get("args"))


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskCreateCall(ToolCallBase):
    """A TaskCreate tracker entry."""

    subject: str
    description: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, subject=required_str(raw, "subject"), description=raw.get("description"))


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskUpdateCall(ToolCallBase):
    """A TaskUpdate tracker change."""

    task_id: str
    status: str | None = None
    subject: str | None = None
    description: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            task_id=required_str(raw, "taskId", "task_id"),
            status=raw.get("status"),
            subject=raw.get("subject"),
            description=raw.get("description"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class ExitPlanModeCall(ToolCallBase):
    """An ExitPlanMode/ExitSpecMode plan submission."""

    plan: str

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, plan=required_str(raw, "plan"))


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

TOOL_TYPES: dict[str, type[ToolCallBase]] = {
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


def parse_tool_call(name: str, input: Mapping[str, Any], *, on_error: Literal["raise", "other"] = "raise") -> ToolCall:
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

    Example:
        >>> call = parse_tool_call("Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"})
        >>> call.new
        'y'
    """
    if not isinstance(input, dict):
        if on_error == "raise":
            raise ToolInputError(f"{name} input must be a mapping, got {type(input).__name__}")
        return OtherCall(name=name, raw={})
    if (cls := TOOL_TYPES.get(TOOL_ALIASES_REVERSE.get(name, name))) is None:
        return OtherCall(name=name, raw=input)
    try:
        return cls.from_raw(name, input)
    except (KeyError, TypeError) as error:
        if on_error == "raise":
            raise ToolInputError(f"{name} input missing or malformed: {error}") from error
        return OtherCall(name=name, raw=input)


class ToolResultError(ValueError):
    """A known tool's result payload did not match its expected shape."""


@dataclass(frozen=True, slots=True)
class QuestionAnnotation:
    """A reviewer's annotation on one answered AskUserQuestion round.

    Attributes:
        preview: The short preview the picker rendered under the answer, if any.
        notes: The free-text note the reviewer attached, if any.
    """

    preview: str | None = None
    notes: str | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class ToolResultBase:
    """Common shape of every typed tool result.

    Attributes:
        name: The tool name exactly as invoked (aliases are not normalized).
        raw: The verbatim ``toolUseResult`` payload — a mapping for structured
            results, a plain string for denials, or None when the record carried
            none. Excluded from equality and repr.
    """

    name: str
    raw: Mapping[str, Any] | str | None = field(repr=False, compare=False)

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        """Extract this result type's fields from the tool ``name`` and raw payload."""
        return cls(name=name, raw=raw)


@dataclass(frozen=True, slots=True, kw_only=True)
class BashResult(ToolResultBase):
    """A Bash/Execute execution result."""

    stdout: str | None = None
    stderr: str | None = None
    interrupted: bool = False
    is_image: bool = False
    no_output_expected: bool = False
    background_task_id: str | None = None
    return_code_interpretation: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            stdout=raw.get("stdout"),
            stderr=raw.get("stderr"),
            interrupted=raw.get("interrupted", False),
            is_image=raw.get("isImage", False),
            no_output_expected=raw.get("noOutputExpected", False),
            background_task_id=raw.get("backgroundTaskId"),
            return_code_interpretation=raw.get("returnCodeInterpretation"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class EditResult(ToolResultBase):
    """An Edit result: the applied replacement and its structured patch.

    ``structured_patch`` and ``original_file`` are kept verbatim — the patch as
    the raw hunk list Claude Code emits, the original file as its full text.
    """

    file_path: str | None = None
    old_string: str | None = None
    new_string: str | None = None
    replace_all: bool = False
    user_modified: bool = False
    stale_recovered: bool = False
    structured_patch: list[Any] | None = None
    original_file: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            file_path=raw.get("filePath"),
            old_string=raw.get("oldString"),
            new_string=raw.get("newString"),
            replace_all=raw.get("replaceAll", False),
            user_modified=raw.get("userModified", False),
            stale_recovered=raw.get("staleRecovered", False),
            structured_patch=raw.get("structuredPatch"),
            original_file=raw.get("originalFile"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class WriteResult(ToolResultBase):
    """A Write/Create result: the written content and its structured patch."""

    content: str | None = None
    file_path: str | None = None
    original_file: str | None = None
    structured_patch: list[Any] | None = None
    user_modified: bool = False

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            content=raw.get("content"),
            file_path=raw.get("filePath"),
            original_file=raw.get("originalFile"),
            structured_patch=raw.get("structuredPatch"),
            user_modified=raw.get("userModified", False),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class ReadResult(ToolResultBase):
    """A Read result: the file payload and its content type.

    ``file`` is kept verbatim — the raw mapping (``filePath``, ``content``,
    ``numLines``, …) Claude Code emits for the read window.
    """

    file: Mapping[str, Any] | None = None
    type: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(name=name, raw=raw, file=raw.get("file"), type=raw.get("type"))


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskResultBase(ToolResultBase):
    """The Agent/Task result family head; :meth:`from_raw` picks the variant.

    A terminal run (``totalDurationMs``/``usage`` present) becomes a
    :class:`TaskResult`; an in-flight launch (``outputFile`` present) becomes a
    :class:`TaskLaunchResult`; a payload matching neither shape degrades to
    :class:`OtherResult` rather than an all-None variant.
    """

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> TaskResult | TaskLaunchResult | OtherResult:
        if raw.get("totalDurationMs") is not None or raw.get("usage") is not None:
            return TaskResult.from_raw(name, raw)
        if raw.get("outputFile") is not None:
            return TaskLaunchResult.from_raw(name, raw)
        return OtherResult(name=name, raw=raw)


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskResult(TaskResultBase):
    """A completed Agent/Task subagent run.

    ``tool_stats``, ``usage``, and ``content`` are kept verbatim — the raw
    per-tool stats, usage mapping, and final content-block list the subagent
    returned.
    """

    agent_id: str | None = None
    agent_type: str | None = None
    status: str | None = None
    total_duration_ms: int | None = None
    total_tokens: int | None = None
    total_tool_use_count: int | None = None
    tool_stats: Mapping[str, Any] | None = None
    usage: Mapping[str, Any] | None = None
    content: list[Any] | None = None
    prompt: str | None = None
    resolved_model: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            agent_id=raw.get("agentId"),
            agent_type=raw.get("agentType"),
            status=raw.get("status"),
            total_duration_ms=raw.get("totalDurationMs"),
            total_tokens=raw.get("totalTokens"),
            total_tool_use_count=raw.get("totalToolUseCount"),
            tool_stats=raw.get("toolStats"),
            usage=raw.get("usage"),
            content=raw.get("content"),
            prompt=raw.get("prompt"),
            resolved_model=raw.get("resolvedModel"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class TaskLaunchResult(TaskResultBase):
    """An in-flight Agent/Task launch (async or backgrounded subagent)."""

    agent_id: str | None = None
    output_file: str | None = None
    is_async: bool = False
    can_read_output_file: bool = False
    description: str | None = None
    prompt: str | None = None
    status: str | None = None
    resolved_model: str | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            agent_id=raw.get("agentId"),
            output_file=raw.get("outputFile"),
            is_async=raw.get("isAsync", False),
            can_read_output_file=raw.get("canReadOutputFile", False),
            description=raw.get("description"),
            prompt=raw.get("prompt"),
            status=raw.get("status"),
            resolved_model=raw.get("resolvedModel"),
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class SkillResult(ToolResultBase):
    """A Skill invocation result."""

    command_name: str | None = None
    success: bool = False
    allowed_tools: tuple[str, ...] | None = None

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            command_name=raw.get("commandName"),
            success=raw.get("success", False),
            allowed_tools=tuple(tools) if isinstance(tools := raw.get("allowedTools"), list) else None,
        )


@dataclass(frozen=True, slots=True, kw_only=True)
class AskUserQuestionResult(ToolResultBase):
    """An AskUserQuestion result: the rounds, the answers, and any annotations.

    Attributes:
        questions: The rounds echoed in the payload, lifted via
            :func:`~cc_transcript.models.parse_questions`.
        answers: A mapping from each round's question text to the chosen answer.
        annotations: A mapping from question text to the reviewer's
            :class:`QuestionAnnotation`, present only for annotated rounds.
    """

    answers: Mapping[str, str] = field(default_factory=dict)
    annotations: Mapping[str, QuestionAnnotation] = field(default_factory=dict)

    @classmethod
    def from_raw(cls, name: str, raw: Mapping[str, Any]) -> Self:
        return cls(
            name=name,
            raw=raw,
            answers=answers if isinstance(answers := raw.get("answers"), Mapping) else {},
            annotations={
                question: QuestionAnnotation(preview=value.get("preview"), notes=value.get("notes"))
                for question, value in annotations.items()
                if isinstance(value, Mapping)
            }
            if isinstance(annotations := raw.get("annotations"), Mapping)
            else {},
        )

    @property
    def questions(self) -> tuple[Question, ...]:
        """The AskUserQuestion rounds echoed in the result payload."""
        from cc_transcript.models import parse_questions

        rounds = self.raw.get("questions") if isinstance(self.raw, Mapping) else None
        return parse_questions(rounds) or ()


@dataclass(frozen=True, slots=True, kw_only=True)
class TextResult(ToolResultBase):
    """A plain-string tool result — denials and other unstructured payloads."""

    text: str = ""


@dataclass(frozen=True, slots=True, kw_only=True)
class OtherResult(ToolResultBase):
    """A tool result the platform does not type: unknown tools, untyped tools
    (such as TodoWrite), and known tools whose payload shape did not match."""


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

TOOL_RESULT_TYPES: dict[str, type[ToolResultBase]] = {
    "Bash": BashResult,
    "Edit": EditResult,
    "Write": WriteResult,
    "Read": ReadResult,
    "Agent": TaskResultBase,
    "Skill": SkillResult,
    "AskUserQuestion": AskUserQuestionResult,
}


def parse_tool_result(
    name: str, payload: Mapping[str, Any] | str | None, *, on_error: Literal["raise", "other"] = "raise"
) -> ToolResult:
    """Parse a tool's name and record-level ``toolUseResult`` into the typed hierarchy.

    The payload is the verbatim ``toolUseResult`` — dict, string, or absent.
    A plain-string payload (a denial) is a :class:`TextResult`; a payload for a
    tool the platform does not type is an :class:`OtherResult`, as is an absent
    (None) payload. Strict by default: a known tool whose mapping payload is
    malformed raises :class:`ToolResultError`; the activity lift passes
    ``on_error='other'`` so a Claude Code shape change degrades to
    :class:`OtherResult` instead of crashing the session lift.

    Example:
        >>> parse_tool_result("Bash", {"stdout": "hi", "stderr": ""}).stdout
        'hi'
    """
    match payload:
        case str():
            return TextResult(name=name, raw=payload, text=payload)
        case Mapping():
            if (cls := TOOL_RESULT_TYPES.get(TOOL_ALIASES_REVERSE.get(name, name))) is None:
                return OtherResult(name=name, raw=payload)
            try:
                return cls.from_raw(name, payload)
            except (KeyError, TypeError) as error:
                if on_error == "raise":
                    raise ToolResultError(f"{name} result missing or malformed: {error}") from error
                return OtherResult(name=name, raw=payload)
        case _:
            return OtherResult(name=name, raw=payload)


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
    """Expand a pipe-separated tool spec to include alias and MCP bare spellings."""
    base = set(spec.split("|"))
    expanded = base | {alias for n in base for alias in (TOOL_ALIASES.get(n), TOOL_ALIASES_REVERSE.get(n)) if alias}
    return frozenset(expanded | {bare for bare, builtin in MCP_TOOL_ALIASES.items() if builtin in expanded})


def matches_names(actual: str, names: Container[str]) -> bool:
    """Whether ``actual`` is one of ``names``, exactly or as an MCP tool suffix.

    True when ``actual`` is in ``names``, or when it splits as
    ``mcp__<server>__<tool>`` on the first two ``__`` and ``<tool>`` — or its
    :data:`MCP_TOOL_ALIASES` builtin equivalent — is in ``names``. That alias
    closes the edit-gate bypass where cc-context's ``ccx_code_edit`` /
    ``ccx_code_replace`` write through the MCP under names no ``Edit``/``Write``
    gate would catch. Harness-rename aliases are not closed over — ``names`` is
    taken verbatim; pre-expand with :func:`expand_tool_names` for those.

    Example:
        >>> matches_names("mcp__github__Grep", {"Grep"})
        True
        >>> matches_names("mcp__cc-context__ccx_code_edit", {"Edit"})
        True
        >>> matches_names("Execute", {"Bash"})
        False
    """
    return actual in names or (
        (mp := mcp_parts(actual)) is not None and (mp[1] in names or MCP_TOOL_ALIASES.get(mp[1]) in names)
    )


def tool_name_matches(actual: str, spec: str) -> bool:
    """Whether ``actual`` matches a pipe spec, honoring aliases and MCP suffixes.

    Example:
        >>> tool_name_matches("Execute", "Bash|Grep")
        True
        >>> tool_name_matches("mcp__github__Grep", "Grep")
        True
    """
    return matches_names(actual, expand_tool_names(spec))


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
