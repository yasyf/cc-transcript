"""Session-level queries over lifted activity.

The measured consumer surface of captain-hook's transcript queries, rebuilt
over :class:`~cc_transcript.activity.SessionActivity`. A :class:`Session` is
an immutable windowed view of a session's turns: every slice — :meth:`Session.after`,
:meth:`Session.before`, :meth:`Session.prior`, :meth:`Session.recent`,
:attr:`Session.current_turn` — returns another :class:`Session`, so hook
predicates compose over progressively narrower windows.
"""

from __future__ import annotations

import re
from collections.abc import Mapping
from dataclasses import dataclass
from fnmatch import fnmatch
from functools import cached_property
from pathlib import PurePath
from typing import TYPE_CHECKING, ClassVar

from cc_transcript.activity import SessionActivity, Turn, event_stamps, native_user_classifier
from cc_transcript.discovery import TranscriptExpiredError, resolve, subagent_paths, subagent_transcripts
from cc_transcript.filterspec import event_meta, session_id_of
from cc_transcript.ids import SessionId, ToolUseId
from cc_transcript.models import AssistantEvent, SystemEvent, ToolResultBlock, UserEvent
from cc_transcript.notifications import Notifications
from cc_transcript.parser import parse
from cc_transcript.tools import (
    BashCall,
    SkillCall,
    TaskCall,
    edits_of,
    expand_tool_names,
    file_paths_of,
    matches_names,
    tool_name_matches,
)

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator, Sequence
    from pathlib import Path

    from cc_transcript.activity import ToolUse, UserClassifier
    from cc_transcript.command import CommandLine
    from cc_transcript.ids import EventUuid
    from cc_transcript.models import TranscriptEvent


def is_failure(use: ToolUse) -> bool:
    return use.result is not None and use.result.is_error


def input_rule_matches(rule: str | re.Pattern[str] | Callable[[object], object] | object, value: object) -> bool:
    match rule:
        case re.Pattern() as pattern:
            return bool(pattern.search(str(value)))
        case rule if callable(rule):
            return bool(rule(value))
        case _:
            return rule == value


def carries_token(event: TranscriptEvent, token: str) -> bool:
    match event:
        case UserEvent(text=text, blocks=blocks):
            return token in text or any(
                token in block.content for block in blocks if isinstance(block, ToolResultBlock)
            )
        case AssistantEvent(text=text):
            return token in text
        case SystemEvent(content=content):
            return content is not None and token in content
        case _:
            return False


def event_positions(turns: Sequence[Turn]) -> dict[EventUuid, int]:
    return {
        meta.uuid: index
        for index, event in enumerate(event for turn in turns for event in turn.events)
        if (meta := event_meta(event)) is not None
    }


def trim_turn(turn: Turn, lo: int, hi: int) -> Turn:
    events = turn.events[lo:hi]
    positions = {meta.uuid: index for index, event in enumerate(turn.events) if (meta := event_meta(event)) is not None}
    started_at, ended_at = event_stamps(events)
    return Turn(
        index=turn.index,
        prompt=turn.prompt if lo == 0 else "",
        started_at=started_at,
        ended_at=ended_at,
        events=events,
        tool_uses=tuple(use for use in turn.tool_uses if lo <= positions[use.ref.event_uuid] < hi),
    )


@dataclass(frozen=True, slots=True)
class FileRef:
    """A file path carried by a tool call, with glob and prefix matching.

    Attributes:
        path: The path exactly as the tool call carried it.

    Example:
        >>> FileRef("/repo/tests/test_app.py").is_test
        True
    """

    path: str

    TEST_PATTERNS: ClassVar[tuple[str, ...]] = ("**/test_*.py", "**/conftest.py", "**/tests/**/*.py")

    def __str__(self) -> str:
        return self.path

    def __fspath__(self) -> str:
        return self.path

    @property
    def is_test(self) -> bool:
        """Whether the path names a Python test file."""
        return self.matches(*self.TEST_PATTERNS)

    @property
    def suffix(self) -> str:
        """The file extension including the leading dot (e.g. ``.py``), or ``""``."""
        return PurePath(self.path).suffix

    def matches(self, *globs: str) -> bool:
        """Whether the full path or the basename matches any glob."""
        name = PurePath(self.path).name
        return any(fnmatch(self.path, glob) or fnmatch(name, glob) for glob in globs)

    def under(self, *prefixes: str) -> bool:
        """Whether the path starts with, or contains a ``/``-anchored, prefix."""
        return any(self.path.startswith(prefix) or f"/{prefix}" in self.path for prefix in prefixes)


@dataclass(frozen=True, slots=True)
class ToolCallQuery:
    """A chainable filter over a window's tool calls.

    Calls whose result errored are hidden by default; :attr:`with_errors`
    widens the view and :meth:`failed` inverts it. Filters narrow, terminals
    extract.

    Example:
        >>> session.tool_calls.named("Edit|Write").files()
    """

    all_items: tuple[ToolUse, ...]
    include_errors: bool = False

    @property
    def items(self) -> tuple[ToolUse, ...]:
        """The effective view: every call, or only those that did not error."""
        if self.include_errors:
            return self.all_items
        return tuple(use for use in self.all_items if not is_failure(use))

    @property
    def with_errors(self) -> ToolCallQuery:
        """The same query with errored calls included."""
        return ToolCallQuery(self.all_items, include_errors=True)

    def named(self, spec: str) -> ToolCallQuery:
        """Calls whose tool name matches a pipe spec, honoring aliases and MCP suffixes."""
        return self.where(lambda use: tool_name_matches(use.call.name, spec))

    def touching(self, *globs: str) -> ToolCallQuery:
        """Calls targeting a file that matches any glob."""
        return self.where(lambda use: any(FileRef(path).matches(*globs) for path in file_paths_of(use.call)))

    def under(self, *prefixes: str) -> ToolCallQuery:
        """Calls targeting a file under any prefix."""
        return self.where(lambda use: any(FileRef(path).under(*prefixes) for path in file_paths_of(use.call)))

    def failed(self) -> ToolCallQuery:
        """Only the calls whose result errored."""
        return ToolCallQuery(tuple(use for use in self.all_items if is_failure(use)), include_errors=True)

    def in_turns(self, *indices: int) -> ToolCallQuery:
        """Calls fired in any of the given session turn indices."""
        return self.where(lambda use: use.turn_index in indices)

    def where(self, predicate: Callable[[ToolUse], bool]) -> ToolCallQuery:
        """Calls satisfying ``predicate``."""
        return ToolCallQuery(tuple(use for use in self.all_items if predicate(use)), self.include_errors)

    def where_input(self, **rules: object) -> ToolCallQuery:
        """Calls whose raw input carries every key, each matching its rule.

        A rule is a compiled regex (searched against ``str(value)``), a
        callable predicate, or a value compared for equality.
        """
        return self.where(
            lambda use: isinstance(use.call.raw, Mapping)
            and all(key in use.call.raw and input_rule_matches(rule, use.call.raw[key]) for key, rule in rules.items())
        )

    def count(self) -> int:
        """The number of matching calls."""
        return len(self.items)

    def any(self) -> bool:
        """Whether any call matches."""
        return bool(self.items)

    def first(self) -> ToolUse | None:
        """The earliest matching call, or None."""
        return items[0] if (items := self.items) else None

    def last(self) -> ToolUse | None:
        """The latest matching call, or None."""
        return items[-1] if (items := self.items) else None

    def files(self) -> tuple[FileRef, ...]:
        """The files the matching calls target, one entry per targeted file (every file
        of an apply_patch), in order."""
        return tuple(FileRef(path) for use in self.items for path in file_paths_of(use.call))

    def edited_files(self) -> tuple[FileRef, ...]:
        """The files edited by the matching calls, one entry per edited file (every file
        of an apply_patch), in order."""
        return tuple(FileRef(path) for use in self.items for path, _ in edits_of(use.call))

    def __iter__(self) -> Iterator[ToolUse]:
        return iter(self.items)

    def __len__(self) -> int:
        return len(self.items)

    def __bool__(self) -> bool:
        return bool(self.items)


@dataclass(frozen=True, slots=True)
class Session:
    """An immutable windowed view of a session's turns.

    Every slicing operation returns another :class:`Session`; turns at a
    window boundary are trimmed copies, so mid-turn slices stay event-precise.

    Attributes:
        turns: The turns in the window.
        path: The transcript file the session was loaded from, when known —
            required for sidechain (subagent) lookups.
        attachments: External transcript files (e.g. codex rollouts) registered
            with this session; :meth:`walk` and :attr:`deep` fold them in at
            depth 1. Empty for a session loaded straight from disk.

    Example:
        >>> session.prior().after(tool="Write", file=str(fp)).has_tool("ExitPlanMode")
    """

    turns: tuple[Turn, ...]
    path: Path | None = None
    attachments: tuple[Path, ...] = ()

    @classmethod
    def from_activity(
        cls, activity: SessionActivity, *, path: Path | None = None, attachments: tuple[Path, ...] = ()
    ) -> Session:
        """Views ``activity``'s full turn range as a session."""
        return cls(activity.turns, path, attachments)

    @classmethod
    def from_path(cls, path: Path, *, user_classifier: UserClassifier = native_user_classifier) -> Session:
        """Parses and lifts the transcript at ``path``."""
        events = parse(path).events
        session_id = session_id_of(events) or SessionId(path.stem)
        return cls.from_activity(
            SessionActivity.from_events(session_id, events, user_classifier=user_classifier), path=path
        )

    @classmethod
    def from_id(
        cls,
        session_id: SessionId,
        *,
        user_classifier: UserClassifier = native_user_classifier,
        root: Path | None = None,
    ) -> Session:
        """Discovers, parses, and lifts ``session_id``'s transcript from disk.

        Raises:
            TranscriptExpiredError: When no transcript for ``session_id``
                exists on disk.
        """
        if (path := resolve(session_id, root=root)) is None:
            raise TranscriptExpiredError(session_id)
        return cls.from_activity(
            SessionActivity.from_events(session_id, parse(path).events, user_classifier=user_classifier),
            path=path,
        )

    @property
    def events(self) -> tuple[TranscriptEvent, ...]:
        """Every event in the window, in order."""
        return tuple(event for turn in self.turns for event in turn.events)

    @property
    def tool_calls(self) -> ToolCallQuery:
        """The window's tool calls as a chainable query."""
        return ToolCallQuery(tuple(use for turn in self.turns for use in turn.tool_uses))

    @property
    def notifications(self) -> Notifications:
        """The harness notification-delivery queue replayed over the window's events."""
        return Notifications.from_events(self.events)

    @property
    def subagents(self) -> SubagentIndex:
        """The window's Task dispatches whose sidechain transcripts exist on disk."""
        if self.path is None:
            return SubagentIndex(())
        transcripts = subagent_transcripts(self.path)
        return SubagentIndex(
            tuple(
                SubagentSession(
                    id=tool_use_id,
                    type=call.agent_type,
                    session=Session.from_path(agent_path),
                    parent=use,
                )
                for use in self.tool_calls.with_errors
                if isinstance(call := use.call, TaskCall)
                and call.agent_type
                and (tool_use_id := use.ref.tool_use_id) is not None
                and (agent_path := transcripts.get(tool_use_id)) is not None
            )
        )

    def walk(self) -> Iterator[DeepSession]:
        """Every transcript reachable from this session, lazily and depth-first.

        Yields each descendant sidechain (subagent/teammate) transcript at every
        depth in DFS path order, then each registered attachment at depth 1 —
        never this session itself. A resolved-path seen-set (seeded with
        :attr:`path`) dedupes: the first occurrence of a path wins, so a
        tree-discovered sidechain outranks an equal attachment, and symlink
        cycles terminate. An unreadable transcript is skipped but its children
        are still walked; a structurally malformed line raises, as
        :attr:`subagents` does.
        """
        return deep_sessions(self)

    @property
    def deep(self) -> DeepView:
        """The recursive union view over this session and every transcript it reaches."""
        return DeepView(self)

    def after(self, *, tool: str, file: str | None = None) -> Session:
        """The window strictly after the last call matching ``tool``.

        ``file`` narrows the match to calls whose target path contains it as
        a substring. With no matching call the result is the empty window.
        """
        positions = event_positions(self.turns)
        matches = [
            positions[use.ref.event_uuid]
            for use in self.tool_calls.with_errors
            if tool_name_matches(use.call.name, tool)
            and (file is None or any(file in path for path in file_paths_of(use.call)))
        ]
        return windowed(self, max(matches) + 1, len(self)) if matches else windowed(self, 0, 0)

    def before(self, *, tool: str) -> Session:
        """The window strictly before the last call matching ``tool``.

        With no matching call the whole window is returned.
        """
        positions = event_positions(self.turns)
        matches = [
            positions[use.ref.event_uuid]
            for use in self.tool_calls.with_errors
            if tool_name_matches(use.call.name, tool)
        ]
        return windowed(self, 0, max(matches)) if matches else self

    def prior(self) -> Session:
        """The window without its last user or assistant event."""
        last = max(
            (index for index, event in enumerate(self.events) if isinstance(event, UserEvent | AssistantEvent)),
            default=None,
        )
        return windowed(self, 0, last) if last is not None else windowed(self, 0, 0)

    def recent(self, n: int) -> Session:
        """The window's last ``n`` events."""
        return windowed(self, max(len(self) - n, 0), len(self))

    @property
    def current_turn(self) -> Session:
        """The one-turn view of the window's last turn."""
        return Session(self.turns[-1:], self.path, self.attachments)

    @property
    def user_text(self) -> str:
        """The prompt that opened the window's last turn."""
        return self.turns[-1].prompt if self.turns else ""

    @property
    def first_prompt(self) -> str | None:
        """The first user prompt in the window, or None when there is none."""
        return next((turn.prompt for turn in self.turns if turn.prompt), None)

    @property
    def files_touched(self) -> tuple[FileRef, ...]:
        """The files targeted by any tool call in the window, one entry per call."""
        return self.tool_calls.files()

    @property
    def edited_files(self) -> tuple[FileRef, ...]:
        """The files modified by edit-shaped calls in the window, one entry per edited file."""
        return self.tool_calls.edited_files()

    def has_tool(self, name: str, *, subagents: bool = True) -> bool:
        """Whether any call in the window matches the pipe spec ``name``."""
        return self.tool_calls.named(name).any() or (
            subagents and any(deep.session.has_tool(name, subagents=False) for deep in self.walk())
        )

    def has_command(self, *argv: str, subagents: bool = True) -> bool:
        """Whether any Bash command in the window runs ``argv``.

        Matches when ``argv`` is a leading-token prefix of any parsed command's
        unwrapped argv, so ``has_command("git", "push")`` matches
        ``sudo git push -f`` and ``cd x && git push`` but not ``echo "git push"``.
        """
        return any(cmd.runs(*argv) for line in self.command_lines() for cmd in line) or (
            subagents and any(deep.session.has_command(*argv, subagents=False) for deep in self.walk())
        )

    def has_edit_to(self, *globs: str, subagents: bool = True) -> bool:
        """Whether any edit-shaped call in the window targets a file matching any glob."""
        return any(file.matches(*globs) for file in self.edited_files) or (
            subagents and any(deep.session.has_edit_to(*globs, subagents=False) for deep in self.walk())
        )

    def has_read(self, pattern: str, *, subagents: bool = True) -> bool:
        """Whether any Read in the window targets a path containing ``pattern``."""
        return any(pattern in str(file) for file in self.tool_calls.named("Read").files()) or (
            subagents and any(deep.session.has_read(pattern, subagents=False) for deep in self.walk())
        )

    def has_skill(self, *names: str, subagents: bool = True) -> bool:
        """Whether any Skill invocation in the window names one of ``names``."""
        return any(
            isinstance(call := use.call, SkillCall) and call.skill in names
            for use in self.tool_calls.named("Skill")
        ) or (subagents and any(deep.session.has_skill(*names, subagents=False) for deep in self.walk()))

    def has_override(self, token: str, *, invalidated_by: Sequence[str] = ("Edit", "Write")) -> bool:
        """Whether ``token`` appears in the window without a later invalidating call.

        The token counts wherever it last appears — user or assistant text,
        system content, or a tool result. Any call after that point matching
        ``invalidated_by`` (aliases honored, errored calls included) cancels
        the override.
        """
        last = max(
            (index for index, event in enumerate(self.events) if carries_token(event, token)),
            default=None,
        )
        if last is None:
            return False
        positions = event_positions(self.turns)
        expanded = expand_tool_names("|".join(invalidated_by))
        return not any(
            positions[use.ref.event_uuid] > last and matches_names(use.call.name, expanded)
            for use in self.tool_calls.with_errors
        )

    def count_failures(self) -> int:
        """The number of calls in the window whose result errored."""
        return self.tool_calls.failed().count()

    def assistant_text(self, n: int = 10, max_per_msg: int = 500) -> str:
        """The window's last ``n`` assistant texts, each capped at ``max_per_msg`` chars."""
        texts = [event.text.strip() for event in self.events if isinstance(event, AssistantEvent)]
        return "\n---\n".join(text[:max_per_msg] for text in texts[-n:] if text)

    def user_said(self, *keywords: str) -> bool:
        """Whether any prompt in the window contains any keyword, case-insensitively."""
        return any(keyword.lower() in turn.prompt.lower() for turn in self.turns for keyword in keywords)

    def commands(self) -> tuple[str, ...]:
        """The shell command strings of the window's Bash calls."""
        return tuple(call.command for use in self.tool_calls.named("Bash") if isinstance(call := use.call, BashCall))

    def command_lines(self) -> tuple[CommandLine, ...]:
        """The window's Bash commands parsed into :class:`~cc_transcript.command.CommandLine` objects."""
        from cc_transcript.command import parse_command_line

        return tuple(parse_command_line(command) for command in self.commands())

    def __len__(self) -> int:
        return sum(len(turn.events) for turn in self.turns)

    def __bool__(self) -> bool:
        return any(turn.events for turn in self.turns)


@dataclass(frozen=True, slots=True)
class SubagentSession:
    """One Task dispatch joined to its sidechain transcript.

    Attributes:
        id: The dispatching tool-use id.
        type: The subagent type the Task named.
        session: The sidechain transcript lifted into a :class:`Session`.
        parent: The dispatching tool use in the parent session.
    """

    id: ToolUseId
    type: str
    session: Session
    parent: ToolUse

    @property
    def tool_calls(self) -> ToolCallQuery:
        """The sidechain session's tool calls."""
        return self.session.tool_calls

    @property
    def failed(self) -> bool:
        """Whether the dispatch's result errored or any sidechain call failed."""
        return bool((result := self.parent.result) and result.is_error) or self.session.count_failures() > 0


@dataclass(frozen=True, slots=True)
class SubagentIndex:
    """The subagent dispatches of a session window.

    Example:
        >>> session.subagents.with_type("test-runner")
    """

    items: tuple[SubagentSession, ...]

    def with_type(self, pattern: str) -> tuple[SubagentSession, ...]:
        """The dispatches whose type is named in the pipe spec ``pattern``."""
        names = set(pattern.split("|"))
        return tuple(subagent for subagent in self.items if subagent.type in names)

    def __iter__(self) -> Iterator[SubagentSession]:
        return iter(self.items)

    def __len__(self) -> int:
        return len(self.items)

    def __bool__(self) -> bool:
        return bool(self.items)


@dataclass(frozen=True, slots=True)
class DeepSession:
    """One transcript reached by :meth:`Session.walk`.

    Attributes:
        session: The whole-session view of the reached transcript.
        path: The transcript file it was loaded from.
        provider: Its source provider, ``"claude"`` or ``"codex"``.
        depth: Distance from the root; ``1`` is a direct sidechain or attachment.
        spawned_by: The dispatching tool-use id parsed from an ``agent-<id>``
            sidechain stem, or None for an attachment.
    """

    session: Session
    path: Path
    provider: str
    depth: int
    spawned_by: ToolUseId | None


@dataclass(frozen=True)  # non-slots: the cached_property below needs __dict__
class DeepView:
    """The recursive union of a session and every transcript reachable from it.

    Sidechain (subagent/teammate) transcripts at every depth and registered
    attachments contribute their tool calls and events to one window-spanning
    view. The root axis respects the session's window; descendants and
    attachments are window-invariant, mirroring how ``has_tool`` already scans
    the whole sidechain tree.

    Example:
        >>> session.deep.tool_calls.named("Edit|Write").files()
    """

    root: Session

    @cached_property
    def sessions(self) -> tuple[DeepSession, ...]:
        """Every reached :class:`DeepSession`, materialized once: DFS, then attachments."""
        return tuple(self.root.walk())

    @property
    def tool_calls(self) -> ToolCallQuery:
        """The root window's calls, then every descendant's and attachment's calls.

        Positional, not chronological: root-window order, then DFS path order,
        then attachment registration order — so :meth:`ToolCallQuery.first` and
        :meth:`ToolCallQuery.last` read positionally.
        """
        return ToolCallQuery(
            self.root.tool_calls.all_items
            + tuple(use for deep in self.sessions for use in deep.session.tool_calls.all_items)
        )

    @property
    def events(self) -> tuple[TranscriptEvent, ...]:
        """Every event across the root window and every reached transcript, in walk order."""
        return self.root.events + tuple(event for deep in self.sessions for event in deep.session.events)

    def __iter__(self) -> Iterator[DeepSession]:
        return iter(self.sessions)


def windowed(session: Session, start: int, stop: int) -> Session:
    turns: list[Turn] = []
    base = 0
    for turn in session.turns:
        size = len(turn.events)
        lo, hi = max(start - base, 0), min(stop - base, size)
        if lo < hi:
            turns.append(turn if (lo, hi) == (0, size) else trim_turn(turn, lo, hi))
        base += size
    return Session(tuple(turns), session.path, session.attachments)


def sidechain_sessions(path: Path | None) -> tuple[Session, ...]:
    if path is None:
        return ()
    return tuple(Session.from_path(entry) for entry in subagent_paths(path))


def deep_sessions(root: Session) -> Iterator[DeepSession]:
    seen: set[Path] = {root.path.resolve()} if root.path is not None else set()
    if root.path is not None:
        yield from descend_sidechains(root.path, 1, seen)
    for attachment in root.attachments:
        yield from visit_transcript(attachment, 1, None, seen)


def descend_sidechains(parent: Path, depth: int, seen: set[Path]) -> Iterator[DeepSession]:
    for child in subagent_paths(parent):
        yield from visit_transcript(child, depth, ToolUseId(child.stem.removeprefix("agent-")), seen)


def visit_transcript(path: Path, depth: int, spawned_by: ToolUseId | None, seen: set[Path]) -> Iterator[DeepSession]:
    if (resolved := path.resolve()) in seen:
        return
    seen.add(resolved)
    if (deep := load_deep_session(path, depth, spawned_by)) is not None:
        yield deep
    yield from descend_sidechains(path, depth + 1, seen)


def load_deep_session(path: Path, depth: int, spawned_by: ToolUseId | None) -> DeepSession | None:
    try:
        transcript = parse(path)
    except OSError:
        return None
    session_id = session_id_of(transcript.events) or SessionId(path.stem)
    return DeepSession(
        session=Session.from_activity(SessionActivity.from_events(session_id, transcript.events), path=path),
        path=path,
        provider=transcript.provider,
        depth=depth,
        spawned_by=spawned_by,
    )
