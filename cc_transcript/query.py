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
import threading
from collections import OrderedDict
from collections.abc import Mapping
from dataclasses import dataclass, field, replace
from fnmatch import fnmatch
from functools import cached_property
from pathlib import PurePath
from typing import TYPE_CHECKING, ClassVar

from cc_transcript.activity import ActivityLift, SessionActivity, Turn, event_stamps, native_user_classifier
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
    import os
    from collections.abc import Callable, Iterator, Sequence
    from pathlib import Path

    from cc_transcript.activity import ToolUse, UserClassifier
    from cc_transcript.command import CommandLine
    from cc_transcript.ids import EventUuid
    from cc_transcript.models import Transcript, TranscriptEvent


DEEP_LIFT_BUDGET = 1024 * 1024 * 1024
DEEP_LIFT_FENCE = 64
IDLE_WALKS_BEFORE_RELEASE = 256

type LiftStamp = tuple[int, int, int, int]
type LiftKey = tuple[Path, int, ToolUseId | None]

RESOLVED_PATHS: OrderedDict[Path, Path] = OrderedDict()
"""The real path of each root and attachment a deep call has seeded or folded in, resolved once.

A deep call dedupes by real path, so it seeds its seen-set with the root and resolves every
attachment; a resident process re-answering per event over a hundred attachments paid a
resolve for each on every call. A path's resolution is held for the process's lifetime,
least-recently-used past :data:`SIDECHAIN_INPUTS_LIMIT` paths, so an attachment retargeted
through a symlink keeps the real path it first resolved to.
"""

RESOLVED_PATHS_GUARD = threading.Lock()


def stamp_of(stat: os.stat_result) -> LiftStamp:
    return (stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns, stat.st_ino)


@dataclass(slots=True)
class DeepLift:
    stamp: LiftStamp
    deep: DeepSession
    lift: ActivityLift
    consumed: int
    fence: bytes
    found: SessionId | None
    idle: int = 0


@dataclass(slots=True)
class LiftCache:
    held: OrderedDict[LiftKey, DeepLift] = field(default_factory=OrderedDict)
    size: int = 0
    guard: threading.Lock = field(default_factory=threading.Lock)

    def get(self, key: LiftKey, stamp: LiftStamp) -> DeepLift | None:
        with self.guard:
            match self.held.get(key):
                case DeepLift(stamp=held_stamp) as lift if held_stamp == stamp:
                    self.held.move_to_end(key)
                    return lift
                case _:
                    return None

    def idle_release(self, key: LiftKey, stamp: LiftStamp) -> None:
        """Counts one unchanged walk over ``key``'s cursor, releasing it once it goes idle.

        A held cursor amortizes the next growth of a live sidechain; a finished one earns
        nothing, so a stamp-unchanged walk over one that :data:`SIDECHAIN_INPUTS` already
        answers bumps its idle count, and past :data:`IDLE_WALKS_BEFORE_RELEASE` the entry
        is dropped and its cursor with it. Any growth replaces the entry with ``idle`` 0.
        """
        with self.guard:
            match self.held.get(key):
                case DeepLift(stamp=held_stamp) as lift if held_stamp == stamp:
                    lift.idle += 1
                    if lift.idle >= IDLE_WALKS_BEFORE_RELEASE:
                        self.drop(key)

    def take(self, key: LiftKey) -> DeepLift | None:
        with self.guard:
            return self.drop(key)

    def put(self, key: LiftKey, lift: DeepLift) -> DeepLift:
        with self.guard:
            self.drop(key)
            self.held[key] = lift
            self.size += lift.stamp[0]
            while self.size > DEEP_LIFT_BUDGET:
                self.drop(next(iter(self.held)))
        return lift

    def drop(self, key: LiftKey) -> DeepLift | None:
        if (lift := self.held.pop(key, None)) is not None:
            self.size -= lift.stamp[0]
        return lift

    def clear(self) -> None:
        with self.guard:
            self.held.clear()
            self.size = 0

    def __len__(self) -> int:
        return len(self.held)


DEEP_LIFTS = LiftCache()
"""Lifted sidechain transcripts, keyed by tree position and stamped ``(size, mtime_ns, ctime_ns, inode)``.

A least-recently-used hold bounded at :data:`DEEP_LIFT_BUDGET` source bytes. One resident
process serves many lead sessions, so the tree walked most recently stays held while an
idle lead's sidechains age out. A lifted session costs about 2.3 times its source bytes in
resident memory, so the default holds roughly 2.3 GiB. A single tree larger than the budget
reparses the part that no longer fits on every walk.

Each entry keeps the :class:`~cc_transcript.activity.ActivityLift` cursor behind its lift and
the byte offset it has consumed, which always ends on a newline. A transcript that has only
grown — same inode, larger, its last held bytes unchanged — is extended by its appended lines
alone: the tail past the last newline is parsed for the session but never fed to the cursor.
A shrink, a replaced or rewritten file, a short read, or a session id that surfaces only in
the appended lines lifts the file afresh. The cursor is mutable, so a stamp miss takes the
entry out of the hold before extending it and puts the grown entry back: a concurrent reader
never sees a cursor mid-extension, and two threads that miss together each produce a correct
entry, the later put replacing the earlier. A dropped entry releases its cursor with it.

:meth:`Session.walk` holds every transcript it reaches; the ``has_*`` predicates hold only a
transcript they have had to lift twice, since a sidechain that changed once is a running
subagent whose next growth would otherwise cost a whole relift, while a finished one is
lifted once and its :class:`PredicateInputs` kept instead. A held cursor is released once its
file survives :data:`IDLE_WALKS_BEFORE_RELEASE` deep walks unchanged, so resident cursors
track the set of sidechains still being written rather than every one that ever grew; a
released file that grows again pays one relift and is re-admitted. :data:`DEEP_LIFT_BUDGET`
stays the upper bound over that live set.
"""


SIDECHAIN_INPUTS_LIMIT = 16384


@dataclass(slots=True)
class PredicateInputsCache:
    held: OrderedDict[Path, tuple[LiftStamp, PredicateInputs]] = field(default_factory=OrderedDict)
    guard: threading.Lock = field(default_factory=threading.Lock)

    def get(self, path: Path, stamp: LiftStamp) -> PredicateInputs | None:
        with self.guard:
            match self.held.get(path):
                case (held_stamp, inputs) if held_stamp == stamp:
                    self.held.move_to_end(path)
                    return inputs
                case _:
                    return None

    def put(self, path: Path, stamp: LiftStamp, inputs: PredicateInputs) -> PredicateInputs:
        with self.guard:
            self.held[path] = (stamp, inputs)
            self.held.move_to_end(path)
            while len(self.held) > SIDECHAIN_INPUTS_LIMIT:
                self.held.popitem(last=False)
        return inputs

    def holds(self, path: Path) -> bool:
        with self.guard:
            return path in self.held

    def clear(self) -> None:
        with self.guard:
            self.held.clear()

    def __len__(self) -> int:
        return len(self.held)


@dataclass(slots=True)
class StampSet:
    held: OrderedDict[Path, LiftStamp] = field(default_factory=OrderedDict)
    guard: threading.Lock = field(default_factory=threading.Lock)

    def has(self, path: Path, stamp: LiftStamp) -> bool:
        with self.guard:
            if self.held.get(path) != stamp:
                return False
            self.held.move_to_end(path)
            return True

    def put(self, path: Path, stamp: LiftStamp) -> None:
        with self.guard:
            self.held[path] = stamp
            self.held.move_to_end(path)
            while len(self.held) > SIDECHAIN_INPUTS_LIMIT:
                self.held.popitem(last=False)

    def clear(self) -> None:
        with self.guard:
            self.held.clear()

    def __len__(self) -> int:
        return len(self.held)


SIDECHAIN_INPUTS = PredicateInputsCache()
"""Each sidechain's :class:`PredicateInputs`, keyed by path and stamped like :data:`DEEP_LIFTS`.

A lifted session costs about 2.3 times its source bytes, so a tree of a thousand sidechains
cannot stay lifted; its :class:`PredicateInputs` are a few kilobytes each. A predicate lifts a
sidechain only when its stamp moved, keeps those inputs, and lets the lift go, so a resident
process re-answering the same predicates per event neither reparses nor holds the tree.
"""


UNREADABLE = StampSet()
"""Transcripts whose typed parse failed, keyed by path and stamped like :data:`DEEP_LIFTS`.

A sidechain carrying one line the typed parser rejects — schema drift Claude Code has not
caught up to — cannot be lifted, so a walk skips it. Without this a resident process re-read
and re-parsed the whole file on every walk forever. Only a deterministic parse failure
(:class:`UnparseableTranscript`) is held, never a transient filesystem error, which must
retry. The failure is held by full stamp: an unchanged bad file is skipped at stamp-check
cost, and any change — a growth that completes the line, a rewrite — clears the miss and
retries.
"""


SIDECHAIN_LISTINGS: OrderedDict[Path, tuple[tuple[int, int], tuple[tuple[Path, bool], ...]]] = OrderedDict()
"""Each non-empty ``subagents`` listing and whether each file is a symlink, stamped ``(inode, mtime_ns)``.

Adding, removing, renaming, or replacing a sidechain moves the directory's mtime, so an unchanged
stamp proves the listing current. A walk then skips the directory read and resolves the directory
once rather than every file; only a symlinked sidechain is resolved on every walk. An empty listing
is never held, since the native lister reports an unreadable directory as empty.
"""

SIDECHAIN_LISTINGS_GUARD = threading.Lock()


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
            lambda use: (
                isinstance(use.call.raw, Mapping)
                and all(
                    key in use.call.raw and input_rule_matches(rule, use.call.raw[key]) for key, rule in rules.items()
                )
            )
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


@dataclass(frozen=True)
class Session:
    """An immutable windowed view of a session's turns.

    Every slicing operation returns another :class:`Session`; turns at a
    window boundary are trimmed copies, so mid-turn slices stay event-precise.

    Unslotted, unlike its neighbours: the window is immutable, so every
    derivation over it — :attr:`tool_calls`, :meth:`commands`,
    :meth:`command_lines` — memoizes per instance, and a predicate that
    queries one window repeatedly pays each derivation once.

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

    @cached_property
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
        cycles terminate. An unreadable or unparseable transcript is skipped
        but its children are still walked.

        Each reached transcript is parsed and lifted once per ``(size, mtime,
        ctime, inode)`` stamp and memoized across walks, and one that has only
        grown is extended by its appended lines, so a predicate that walks
        repeatedly — or a resident process that re-walks per event — parses
        only what was appended.
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
        return any_inputs(self, lambda inputs: inputs.has_tool(name), subagents=subagents)

    def has_command(self, *argv: str, subagents: bool = True) -> bool:
        """Whether any Bash command in the window runs ``argv``.

        Matches when ``argv`` is a leading-token prefix of any parsed command's
        unwrapped argv, so ``has_command("git", "push")`` matches
        ``sudo git push -f`` and ``cd x && git push`` but not ``echo "git push"``.
        """
        return any_inputs(self, lambda inputs: inputs.has_command(argv), subagents=subagents)

    def has_edit_to(self, *globs: str, subagents: bool = True) -> bool:
        """Whether any edit-shaped call in the window targets a file matching any glob."""
        return any_inputs(self, lambda inputs: inputs.has_edit_to(globs), subagents=subagents)

    def has_read(self, pattern: str, *, subagents: bool = True) -> bool:
        """Whether any Read in the window targets a path containing ``pattern``."""
        return any_inputs(self, lambda inputs: inputs.has_read(pattern), subagents=subagents)

    def has_skill(self, *names: str, subagents: bool = True) -> bool:
        """Whether any Skill invocation in the window names one of ``names``."""
        return any_inputs(self, lambda inputs: inputs.has_skill(names), subagents=subagents)

    @cached_property
    def predicate_inputs(self) -> PredicateInputs:
        """What the ``has_*`` predicates read from this window."""
        return PredicateInputs.of(self)

    def deep_inputs(self) -> Iterator[PredicateInputs]:
        """This window's :class:`PredicateInputs`, then those of every transcript :meth:`walk` reaches.

        The walk order and dedupe match :meth:`walk`, but a reached transcript stays lifted only
        once it has changed: each one's inputs are held per ``(size, mtime, ctime, inode)`` stamp,
        a transcript is lifted again only once its stamp moves, and from then on one that has only
        grown is extended by its appended lines. Prefer it to :meth:`walk` for any predicate these
        inputs can answer.
        """
        yield self.predicate_inputs
        for path, depth, spawned_by in reachable_transcripts(self):
            if (inputs := load_predicate_inputs(path, depth, spawned_by)) is not None:
                yield inputs

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
        return self._commands

    def command_lines(self) -> tuple[CommandLine, ...]:
        """The window's Bash commands parsed into :class:`~cc_transcript.command.CommandLine` objects."""
        return self._command_lines

    @cached_property
    def _commands(self) -> tuple[str, ...]:
        return tuple(call.command for use in self.tool_calls.named("Bash") if isinstance(call := use.call, BashCall))

    @cached_property
    def _command_lines(self) -> tuple[CommandLine, ...]:
        return self.predicate_inputs.command_lines

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
class PredicateInputs:
    """The slice of one transcript window that the ``has_*`` predicates read.

    Small enough to hold for every transcript in a tree of thousands, where a lifted
    :class:`Session` is not. Calls whose result errored are left out, as
    :class:`ToolCallQuery` hides them by default. Answers that do not depend on the MCP
    tool registry are memoized, up to :attr:`MAX_ANSWERS` per instance.

    Attributes:
        calls: Each call's tool name and the file paths it targets, in order.
        commands: The command string of every Bash call.
        edited_files: The files modified by edit-shaped calls, one entry per edited file.
        skills: The skill named by every Skill call.
    """

    calls: tuple[tuple[str, tuple[str, ...]], ...]
    commands: tuple[str, ...]
    edited_files: tuple[FileRef, ...]
    skills: tuple[str, ...]
    answers: dict[tuple[str, object], bool] = field(default_factory=dict, compare=False, repr=False)

    MAX_ANSWERS: ClassVar[int] = 256

    @classmethod
    def of(cls, session: Session) -> PredicateInputs:
        """Extracts the predicate inputs from ``session``'s window."""
        calls = session.tool_calls
        return cls(
            calls=tuple((use.call.name, tuple(file_paths_of(use.call))) for use in calls.items),
            commands=session.commands(),
            edited_files=session.edited_files,
            skills=tuple(call.skill for use in calls.named("Skill") if isinstance(call := use.call, SkillCall)),
        )

    @cached_property
    def command_lines(self) -> tuple[CommandLine, ...]:
        """:attr:`commands` parsed into :class:`~cc_transcript.command.CommandLine` objects."""
        from cc_transcript.command import parse_command_line

        return tuple(parse_command_line(command) for command in self.commands)

    @cached_property
    def tool_names(self) -> frozenset[str]:
        """The distinct names in :attr:`calls`."""
        return frozenset(name for name, _ in self.calls)

    def files(self, spec: str) -> tuple[FileRef, ...]:
        """The files targeted by calls matching the pipe spec ``spec``, one entry per path."""
        matching = {name for name in self.tool_names if tool_name_matches(name, spec)}
        return tuple(FileRef(path) for name, paths in self.calls if name in matching for path in paths)

    def answer(self, query: tuple[str, object], compute: Callable[[], bool]) -> bool:
        if (known := self.answers.get(query)) is None:
            if len(self.answers) >= self.MAX_ANSWERS:
                self.answers.clear()
            known = self.answers[query] = compute()
        return known

    def has_tool(self, name: str) -> bool:
        return any(tool_name_matches(tool, name) for tool in self.tool_names)

    def has_command(self, argv: tuple[str, ...]) -> bool:
        return self.answer(
            ("command", argv), lambda: any(cmd.runs(*argv) for line in self.command_lines for cmd in line)
        )

    def has_edit_to(self, globs: tuple[str, ...]) -> bool:
        return self.answer(("edit", globs), lambda: any(file.matches(*globs) for file in self.edited_files))

    def has_read(self, pattern: str) -> bool:
        return any(pattern in str(file) for file in self.files("Read"))

    def has_skill(self, names: tuple[str, ...]) -> bool:
        return self.answer(("skill", names), lambda: any(skill in names for skill in self.skills))


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


def any_inputs(session: Session, predicate: Callable[[PredicateInputs], bool], *, subagents: bool) -> bool:
    return any(map(predicate, session.deep_inputs() if subagents else (session.predicate_inputs,)))


def reachable_transcripts(root: Session) -> Iterator[tuple[Path, int, ToolUseId | None]]:
    seen: set[Path] = {resolved_path(root.path)} if root.path is not None else set()
    if root.path is not None:
        yield from reachable_sidechains(root.path, 1, seen)
    for attachment in root.attachments:
        yield from reachable_from(attachment, resolved_path(attachment), 1, None, seen)


def resolved_path(path: Path) -> Path:
    with RESOLVED_PATHS_GUARD:
        if (held := RESOLVED_PATHS.get(path)) is not None:
            RESOLVED_PATHS.move_to_end(path)
            return held
    resolved = path.resolve()
    with RESOLVED_PATHS_GUARD:
        RESOLVED_PATHS[path] = resolved
        RESOLVED_PATHS.move_to_end(path)
        while len(RESOLVED_PATHS) > SIDECHAIN_INPUTS_LIMIT:
            RESOLVED_PATHS.popitem(last=False)
    return resolved


def reachable_sidechains(parent: Path, depth: int, seen: set[Path]) -> Iterator[tuple[Path, int, ToolUseId | None]]:
    for child, resolved in sidechain_listing(parent):
        yield from reachable_from(child, resolved, depth, ToolUseId(child.stem.removeprefix("agent-")), seen)


def reachable_from(
    path: Path, resolved: Path, depth: int, spawned_by: ToolUseId | None, seen: set[Path]
) -> Iterator[tuple[Path, int, ToolUseId | None]]:
    if resolved in seen:
        return
    seen.add(resolved)
    yield path, depth, spawned_by
    yield from reachable_sidechains(path, depth + 1, seen)


def sidechain_listing(parent: Path) -> tuple[tuple[Path, Path], ...]:
    directory = parent.parent / parent.stem / "subagents"
    try:
        stat = directory.stat()
    except OSError:
        return ()
    stamp = (stat.st_ino, stat.st_mtime_ns)
    with SIDECHAIN_LISTINGS_GUARD:
        held = SIDECHAIN_LISTINGS.get(directory)
    if held is not None and held[0] == stamp:
        children = held[1]
    else:
        children = tuple((child, child.is_symlink()) for child in subagent_paths(parent))
        if children:
            with SIDECHAIN_LISTINGS_GUARD:
                SIDECHAIN_LISTINGS[directory] = (stamp, children)
                SIDECHAIN_LISTINGS.move_to_end(directory)
                while len(SIDECHAIN_LISTINGS) > SIDECHAIN_INPUTS_LIMIT:
                    SIDECHAIN_LISTINGS.popitem(last=False)
    real = directory.resolve()
    return tuple((child, child.resolve() if link else real / child.name) for child, link in children)


def load_predicate_inputs(path: Path, depth: int, spawned_by: ToolUseId | None) -> PredicateInputs | None:
    try:
        stamp = stamp_of(path.stat())
    except OSError:
        return None
    if (held := SIDECHAIN_INPUTS.get(path, stamp)) is not None:
        DEEP_LIFTS.idle_release((path, depth, spawned_by), stamp)
        return held
    try:
        deep = deep_session_at(path, depth, spawned_by, stamp, hold=SIDECHAIN_INPUTS.holds(path))
    except OSError:
        return None
    return SIDECHAIN_INPUTS.put(path, stamp, PredicateInputs.of(deep.session))


def deep_sessions(root: Session) -> Iterator[DeepSession]:
    for path, depth, spawned_by in reachable_transcripts(root):
        if (deep := load_deep_session(path, depth, spawned_by)) is not None:
            yield deep


def load_deep_session(path: Path, depth: int, spawned_by: ToolUseId | None) -> DeepSession | None:
    try:
        return deep_session_at(path, depth, spawned_by, stamp_of(path.stat()), hold=True)
    except OSError:
        return None


def deep_session_at(
    path: Path, depth: int, spawned_by: ToolUseId | None, stamp: LiftStamp, *, hold: bool
) -> DeepSession:
    key = (path, depth, spawned_by)
    if (held := DEEP_LIFTS.get(key, stamp)) is not None:
        return held.deep
    if UNREADABLE.has(path, stamp):
        raise UnparseableTranscript(f"unreadable transcript: {path}")
    stale = DEEP_LIFTS.take(key)
    try:
        lifted = (None if stale is None else grown_lift(stale, path, stamp)) or lift_deep_session(
            path, depth, spawned_by, stamp
        )
    except UnparseableTranscript:
        UNREADABLE.put(path, stamp)
        raise
    return (DEEP_LIFTS.put(key, lifted) if hold or stale is not None else lifted).deep


def lift_deep_session(path: Path, depth: int, spawned_by: ToolUseId | None, stamp: LiftStamp) -> DeepLift:
    raw = path.read_bytes()
    events = list((transcript := parsed(path, raw)).events)
    session_id = (found := session_id_of(events)) or SessionId(path.stem)
    cut = raw.rfind(b"\n") + 1
    committed = len(events) if cut == len(raw) else len(events) - len(parsed(path, raw[cut:]).events)
    activity = (lift := ActivityLift(session_id)).extend(events[:committed])
    return DeepLift(
        stamp=stamp,
        deep=DeepSession(
            session=Session.from_activity(
                activity if committed == len(events) else SessionActivity.from_events(session_id, events), path=path
            ),
            path=path,
            provider=transcript.provider,
            depth=depth,
            spawned_by=spawned_by,
        ),
        lift=lift,
        consumed=cut,
        fence=raw[max(cut - DEEP_LIFT_FENCE, 0) : cut],
        found=found,
    )


def grown_lift(held: DeepLift, path: Path, stamp: LiftStamp) -> DeepLift | None:
    if stamp[3] != held.stamp[3] or stamp[0] <= held.stamp[0] or stamp[0] < held.consumed:
        return None
    if (read := appended_bytes(path, held.consumed - len(held.fence), stamp[0])) is None or not read.startswith(
        held.fence
    ):
        return None
    appended = read[len(held.fence) :]
    cut = appended.rfind(b"\n") + 1
    committed = list(parsed(path, appended[:cut]).events)
    tail = [] if cut == len(appended) else list(parsed(path, appended[cut:]).events)
    found = held.found or session_id_of([*committed, *tail])
    if (found or SessionId(path.stem)) != held.lift.session_id:
        return None
    activity = held.lift.extend(committed)
    session = Session.from_activity(
        activity
        if not tail
        else SessionActivity.from_events(
            held.lift.session_id, [*(event for turn in activity.turns for event in turn.events), *tail]
        ),
        path=path,
    )
    return DeepLift(
        stamp=stamp,
        deep=replace(held.deep, session=session),
        lift=held.lift,
        consumed=held.consumed + cut,
        fence=(held.fence + appended[:cut])[-DEEP_LIFT_FENCE:],
        found=found,
    )


def appended_bytes(path: Path, start: int, stop: int) -> bytes | None:
    with path.open("rb") as handle:
        handle.seek(start)
        read = handle.read(stop - start)
    return read if len(read) == stop - start else None


class UnparseableTranscript(OSError):
    """A transcript a typed parse rejected deterministically, not a filesystem failure.

    Raised only for a line the parser cannot type — schema drift — so a walk can hold the
    failure by stamp, where a transient ``OSError`` (EACCES, EIO, an ENOENT race) must
    retry. An ``OSError`` subclass so both still skip the transcript rather than raise.
    """


def parsed(path: Path, raw: bytes) -> Transcript:
    try:
        return parse(raw)
    except (KeyError, ValueError):
        raise UnparseableTranscript(f"unreadable transcript: {path}") from None
