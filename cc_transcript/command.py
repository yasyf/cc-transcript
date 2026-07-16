"""The one bash-command parsing layer, backed by tree-sitter-bash.

Parses a raw shell string into typed commands — executable, arguments,
environment assignments, and redirects — split at pipeline and list
operators. :func:`parse_command_line` is the cached entry point;
:func:`command_prefixes` distills each command to its permission-style
prefix (``"git commit"``, ``"docker compose"``).
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from functools import cached_property, lru_cache
from itertools import dropwhile
from typing import TYPE_CHECKING

import tree_sitter_bash as tsbash
from tree_sitter import Language, Node, Parser

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator, Mapping, Sequence

BASH_PARSER = Parser(Language(tsbash.language()))

COMPOUND_OPS = frozenset({"&&", "||", ";", "|", "&"})

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

ASSIGNMENT_RE = re.compile(r"^\w+=")

PIPE_GAP_RE = re.compile(r"\s*\|&?\s*")

REDIRECT_OP_TYPES = frozenset({"file_descriptor", ">", ">>", "<", "<<", ">&", "<&", ">|"})


@dataclass(frozen=True)
class Redirect:
    """A shell redirect parsed from a bash command (e.g. ``> file.txt``, ``2>&1``)."""

    op: str
    target: str
    fd: int | None = None


@dataclass(frozen=True)
class Command:
    """A single parsed shell command with executable, arguments, env vars, and redirects.

    Use ``Command.parse(raw)`` to parse a command string, or access via ``CommandLine``.
    """

    raw: str
    executable: str
    args: tuple[str, ...]
    env: tuple[tuple[str, str], ...] = ()
    redirects: tuple[Redirect, ...] = ()
    span: tuple[int, int] | None = field(default=None, compare=False, repr=False)

    @classmethod
    def parse(cls, raw: str) -> Command | None:
        """Parse ``raw`` and return its final command, or ``None`` when nothing parses."""
        return CommandLine.parse(raw).primary

    @cached_property
    def argv(self) -> tuple[str, ...]:
        """The full argument vector — ``(executable, *args)``, or ``()`` when nothing parsed."""
        return (self.executable, *self.args) if self.executable else ()

    @cached_property
    def program(self) -> str:
        """The program this command ultimately runs, resolving launcher invocations.

        ``uv run pytest …`` resolves to ``pytest`` and ``python -m module …``
        (or ``python3 -m``) to ``module``; any other command resolves to its own
        executable. Unlike ``unwrapped`` — which strips privilege/environment
        wrappers (``sudo``, ``env``, ``timeout``, …) and returns a full
        ``Command`` — this resolves only those two launcher shapes and returns
        just the program name.
        """
        if self.executable == "uv" and len(self.args) >= 2 and self.args[0] == "run":
            return self.args[1]
        if re.match(r"python3?$", self.executable) and len(self.args) >= 2 and self.args[0] == "-m":
            return self.args[1]
        return self.executable

    @cached_property
    def env_dict(self) -> dict[str, str]:
        """The leading ``VAR=val`` assignments as a dict, keyed by variable name."""
        return dict(self.env)

    @cached_property
    def unwrapped(self) -> Command:
        """This command with leading wrappers (``sudo``, ``env``, ``timeout``, …) stripped.

        Shifts past each wrapper plus its flag arguments, ``VAR=val`` words, and
        bare ASCII-integer arguments (covering ``env -i``, ``timeout 30``,
        ``nice -n 10``); returns ``self`` when no unwrapping applies.
        """
        argv = self.argv
        while argv and argv[0] in WRAPPER_COMMANDS:
            argv = tuple(
                dropwhile(
                    lambda a: a.startswith("-") or (a.isascii() and a.isdigit()) or ASSIGNMENT_RE.match(a) is not None,
                    argv[1:],
                )
            )
        if argv == self.argv:
            return self
        return Command(
            raw=self.raw,
            executable=argv[0] if argv else "",
            args=argv[1:],
            env=self.env,
            redirects=self.redirects,
            span=self.span,
        )

    @property
    def prefix(self) -> str | None:
        """The permission-style prefix of the unwrapped command, or ``None`` when empty.

        Multi-level tools (``git``, ``docker``, …) keep their first non-flag argument
        as the subcommand, so ``git commit -m x`` yields ``"git commit"``.
        """
        match self.unwrapped:
            case Command(executable=""):
                return None
            case Command(executable=exe, args=args) if exe in MULTI_LEVEL_TOOLS:
                return f"{exe} {sub}" if (sub := next((a for a in args if not a.startswith("-")), None)) else exe
            case cmd:
                return cmd.executable

    def runs(self, *argv: str) -> bool:
        """Return whether the unwrapped command's argv starts with ``argv``.

        Args:
            *argv: Leading argv tokens to match, e.g. ``("git", "push")``.

        Returns:
            ``True`` if ``argv`` is non-empty and is a prefix of the unwrapped
            command's ``argv``.
        """
        return bool(argv) and self.unwrapped.argv[: len(argv)] == argv

    def matches(self, pattern: str) -> bool:
        """Return whether ``pattern`` matches the rendered command.

        Args:
            pattern: Regular expression searched against ``str(self)`` (the
                joined argv, or the raw text when nothing parsed).

        Returns:
            ``True`` if the pattern matches anywhere in the rendered command.
        """
        return bool(re.search(pattern, str(self)))

    def has_arg(self, *patterns: str) -> bool:
        """Return whether any argument matches any of ``patterns``.

        Args:
            *patterns: Regular expressions searched against each argument
                (the executable is not considered).

        Returns:
            ``True`` if at least one pattern matches at least one argument.
        """
        return any(re.search(p, a) for p in patterns for a in self.args)

    def __str__(self) -> str:
        return " ".join(self.argv) if self.argv else self.raw

    def __contains__(self, item: str) -> bool:
        return item in str(self)

    def __bool__(self) -> bool:
        return bool(self.executable)


@dataclass(frozen=True)
class CommandLine:
    """A full parsed bash command line, potentially containing multiple commands joined by operators.

    Use ``CommandLine.parse(raw)`` (or the cached ``parse_command_line``) to parse.
    Access individual commands via ``.commands`` or the final command via ``.primary``.
    """

    raw: str
    parts: tuple[tuple[Command, str | None], ...]

    @classmethod
    def parse(cls, raw: str) -> CommandLine:
        """Parse ``raw`` with tree-sitter-bash into a ``CommandLine``.

        Blank or comment-only input parses to empty ``parts``, so ``.primary``
        and ``.head`` are ``None`` and the line is falsy.
        """
        return cls(raw=raw, parts=tuple(cls.walk_node(BASH_PARSER.parse(raw.encode()).root_node)))

    @cached_property
    def commands(self) -> tuple[Command, ...]:
        return tuple(cmd for cmd, _ in self.parts)

    @cached_property
    def primary(self) -> Command | None:
        """The final command of the line, or ``None`` when nothing parsed."""
        return self.parts[-1][0] if self.parts else None

    @cached_property
    def head(self) -> Command | None:
        """The first command of the line, or ``None`` when nothing parsed."""
        return self.parts[0][0] if self.parts else None

    @cached_property
    def prefixes(self) -> tuple[str, ...]:
        """The permission-style prefix of each command, absent prefixes dropped."""
        return tuple(prefix for cmd in self.commands if (prefix := cmd.prefix) is not None)

    def __iter__(self) -> Iterator[Command]:
        return iter(self.commands)

    def __len__(self) -> int:
        return len(self.parts)

    def __str__(self) -> str:
        return self.raw

    def __contains__(self, item: str) -> bool:
        return item in self.raw

    def __bool__(self) -> bool:
        return bool(self.parts)

    @cached_property
    def q(self) -> CommandLineQuery:
        return CommandLineQuery(self)

    @cached_property
    def occurrences(self) -> tuple[Occurrence, ...]:
        """One ``Occurrence`` per part, in line order."""
        return tuple(Occurrence(self, index) for index in range(len(self.parts)))

    def splice(self, replacements: Mapping[int, str]) -> str:
        """Swap each indexed command's byte span for its replacement, preserving every other byte.

        Args:
            replacements: Command index → replacement text. Operates on
                ``raw.encode()``; untouched bytes (operators, redirects,
                heredoc bodies, comments) pass through verbatim.

        Returns:
            The rewritten line, decoded once at the end.

        Raises:
            ValueError: If an indexed command has no ``span``, or the spans are
                out of order or overlap.
        """
        source = self.raw.encode()
        out = bytearray()
        cursor = 0
        for index in sorted(replacements):
            span = self.parts[index][0].span
            if span is None:
                raise ValueError(f"command at index {index} has no span")
            start, end = span
            if start < cursor:
                raise ValueError(f"span {span} at index {index} overlaps or precedes cursor {cursor}")
            out += source[cursor:start]
            out += replacements[index].encode()
            cursor = end
        out += source[cursor:]
        return out.decode()

    def rewrite_occurrences(self, to: Callable[[Occurrence], str | None]) -> str | None:
        """Map ``to`` over each occurrence and splice in its non-``None`` results.

        Args:
            to: Maps an ``Occurrence`` to replacement text, or ``None`` to leave
                that command untouched.

        Returns:
            The spliced line, or ``None`` when ``to`` returned ``None`` for every
            occurrence.
        """
        replacements = {occ.index: text for occ in self.occurrences if (text := to(occ)) is not None}
        return self.splice(replacements) if replacements else None

    @staticmethod
    def node_text(node: Node) -> str:
        return node.text.decode() if node.text else ""

    @staticmethod
    def dequote(text: str) -> str:
        return text[1:-1] if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"" else text

    @staticmethod
    def word_text(node: Node) -> str:
        return (
            CommandLine.dequote(CommandLine.node_text(node))
            if node.type in ("string", "raw_string")
            else CommandLine.node_text(node)
        )

    @staticmethod
    def extract_redirect(node: Node) -> Redirect:
        op = ""
        target = ""
        fd: int | None = None

        for child in node.children:
            match child.type:
                case "file_descriptor":
                    fd = int(CommandLine.node_text(child)) if CommandLine.node_text(child).isdigit() else None
                case t if t in (">", ">>", "<", "<<", ">&", "<&", ">|"):
                    op = t
                case _:
                    text = CommandLine.node_text(child)
                    if not op and text in (">", ">>", "<", "<<", ">&", "<&", ">|"):
                        op = text
                    elif op:
                        target = text
                    else:
                        target = text

        return Redirect(op=op, target=target, fd=fd)

    @staticmethod
    def extract_command(node: Node) -> Command:
        executable = ""
        args: list[str] = []
        env: list[tuple[str, str]] = []
        redirects: list[Redirect] = []

        for child in node.children:
            match child.type:
                case "command_name":
                    executable = CommandLine.word_text(child)
                case "variable_assignment":
                    name = next((c for c in child.children if c.type == "variable_name"), None)
                    val = child.children[-1] if len(child.children) >= 3 else None
                    if name:
                        env.append(
                            (
                                CommandLine.node_text(name),
                                CommandLine.word_text(val) if val and val.type != "=" else "",
                            )
                        )
                case "file_redirect":
                    redirects.append(CommandLine.extract_redirect(child))
                case _ if child.type in (
                    "word",
                    "string",
                    "raw_string",
                    "number",
                    "concatenation",
                    "simple_expansion",
                    "expansion",
                ):
                    if executable:
                        args.append(CommandLine.word_text(child))
                    else:
                        executable = CommandLine.word_text(child)
                case _:
                    pass

        content = [c for c in node.children if c.type != "file_redirect"]
        span = (content[0].start_byte, content[-1].end_byte) if content else (node.start_byte, node.end_byte)

        return Command(
            raw=CommandLine.node_text(node),
            executable=executable,
            args=tuple(args),
            env=tuple(env),
            redirects=tuple(redirects),
            span=span,
        )

    @staticmethod
    def collect_parts(children: list[Node], ops: frozenset[str]) -> list[tuple[Command, str | None]]:
        parts: list[tuple[Command, str | None]] = []
        for child in children:
            text = CommandLine.node_text(child)
            if child.type in ops or text in ops:
                if parts:
                    cmd, _ = parts[-1]
                    parts[-1] = (cmd, text)
                continue
            if sub := CommandLine.walk_node(child):
                parts.extend(sub)
        return parts

    @staticmethod
    def redirect_absorbed_word(node: Node) -> bool:
        """Whether a ``file_redirect`` swallowed a command word past its target.

        Tree-sitter-bash folds a word that trails a redirect (the ``b`` in
        ``echo a >out b``) into the ``file_redirect`` node beside the target, so
        a redirect carrying more than one target word split the command's
        arguments — leaving it no contiguous byte span to splice.
        """
        return sum(c.type not in REDIRECT_OP_TYPES for c in node.children) > 1

    @staticmethod
    def walk_redirected(node: Node) -> list[tuple[Command, str | None]]:
        redirects: list[Redirect] = []
        inner_parts: list[tuple[Command, str | None]] = []
        broken = False
        for child in node.children:
            if child.type == "file_redirect":
                redirects.append(CommandLine.extract_redirect(child))
                broken = broken or CommandLine.redirect_absorbed_word(child)
            else:
                inner_parts.extend(CommandLine.walk_node(child))
        if redirects and inner_parts:
            inner_parts = [
                (
                    Command(
                        raw=cmd.raw,
                        executable=cmd.executable,
                        args=cmd.args,
                        env=cmd.env,
                        redirects=(*cmd.redirects, *redirects),
                        span=None if broken else cmd.span,
                    ),
                    op,
                )
                for cmd, op in inner_parts
            ]
        return inner_parts or [
            (
                Command(
                    raw=CommandLine.node_text(node),
                    executable="",
                    args=(),
                    redirects=tuple(redirects),
                    span=(node.start_byte, node.end_byte),
                ),
                None,
            )
        ]

    @staticmethod
    def walk_node(node: Node) -> list[tuple[Command, str | None]]:
        match node.type:
            case "program":
                return CommandLine.collect_parts(node.children, frozenset({";"}))
            case "list":
                return CommandLine.collect_parts(node.children, COMPOUND_OPS)
            case "pipeline":
                return CommandLine.collect_parts(node.children, frozenset({"|"}))
            case "command":
                return [(CommandLine.extract_command(node), None)]
            case "redirected_statement":
                return CommandLine.walk_redirected(node)
            case _:
                parts: list[tuple[Command, str | None]] = []
                for child in node.children:
                    parts.extend(CommandLine.walk_node(child))
                return parts


@dataclass(frozen=True)
class Occurrence:
    """One command of a ``CommandLine`` with its position and joining context.

    Pairs the ``line`` with this command's ``index`` into ``line.parts``,
    exposing the command, the operators on either side, and whether it sits in a
    pipeline. Hook packs map over ``CommandLine.occurrences`` and return
    replacement text per occurrence to rewrite individual commands in place.
    """

    line: CommandLine
    index: int

    @property
    def command(self) -> Command:
        """The command at this occurrence's index."""
        return self.line.parts[self.index][0]

    @property
    def prev_op(self) -> str | None:
        """The operator joining the previous command to this one; ``None`` at index 0."""
        return self.line.parts[self.index - 1][1] if self.index > 0 else None

    @property
    def next_op(self) -> str | None:
        """The operator joining this command to the next; ``None`` for the final command."""
        return self.line.parts[self.index][1]

    @cached_property
    def piped(self) -> bool:
        """Whether this command sits on either side of a pipe.

        ``True`` when a neighboring operator is ``|``. Tree-sitter records no
        operator token for ``|&``, and newline-separated statements also carry a
        ``None`` operator, so a ``None`` neighbor falls back to inspecting the
        decoded source gap toward that neighbor: it counts as piped only when the
        gap is exactly one pipe operator token (``|`` or ``|&``) surrounded by
        whitespace. Anything else in the gap — a comment, a redirect target, a
        heredoc body, ``||``, an intervening statement — is not a pipe.
        """
        if self.prev_op == "|" or self.next_op == "|":
            return True
        span = self.command.span
        if span is None:
            return False
        source = self.line.raw.encode()
        if self.next_op is None and self.index + 1 < len(self.line.parts):
            nxt = self.line.parts[self.index + 1][0].span
            if nxt is not None and PIPE_GAP_RE.fullmatch(source[span[1] : nxt[0]].decode()):
                return True
        if self.prev_op is None and self.index > 0:
            prev = self.line.parts[self.index - 1][0].span
            if prev is not None and PIPE_GAP_RE.fullmatch(source[prev[1] : span[0]].decode()):
                return True
        return False


@dataclass(frozen=True)
class CommandLineQuery:
    """Predicate helpers for inspecting a parsed ``CommandLine``.

    Wraps a ``CommandLine`` to answer common yes/no questions a caller
    needs — which executable runs, whether a subcommand or token appears, or
    whether the line redirects/pipes. Obtain one via ``CommandLine.q``.
    """

    line: CommandLine

    def runs(self, *argv: str) -> bool:
        """Return whether the primary command's unwrapped argv starts with ``argv``.

        Args:
            *argv: Leading argv tokens to match, e.g. ``("git", "push")``.

        Returns:
            ``True`` if ``argv`` is non-empty and is a prefix of the primary
            command's unwrapped ``argv``; ``False`` when the line parsed to
            no commands.
        """
        return (primary := self.line.primary) is not None and primary.runs(*argv)

    def has_subcommand(self, name: str) -> bool:
        """Return whether any command in the line carries ``name`` as an argument.

        Args:
            name: The subcommand/argument token to look for (e.g. ``"push"``).

        Returns:
            ``True`` if ``name`` appears in the arguments of any parsed command.
        """
        return any(name in cmd.args for cmd in self.line.commands)

    def any_command(self, pred: Callable[[Command], bool]) -> bool:
        """Return whether any command in the line satisfies ``pred``.

        Args:
            pred: Predicate applied to each parsed ``Command``.

        Returns:
            ``True`` if ``pred`` returns truthy for at least one command.
        """
        return any(pred(cmd) for cmd in self.line.commands)

    def uses_redirect(self) -> bool:
        """Return whether the line redirects output or pipes between commands.

        Returns:
            ``True`` if any command has a file redirect or the parts are joined
            by a pipe (``|``) operator.
        """
        return any(cmd.redirects for cmd in self.line.commands) or any(op == "|" for _, op in self.line.parts if op)

    def contains_token(self, token: str) -> bool:
        """Return whether ``token`` appears as a whole argv element in any command.

        Unlike ``has_subcommand`` this matches the executable as well as the
        arguments and requires an exact element match, not a substring.

        Args:
            token: The exact argv token to look for.

        Returns:
            ``True`` if ``token`` equals an argv element of any parsed command.
        """
        return any(token == a for cmd in self.line.commands for a in cmd.argv)


parse_command_line = lru_cache(maxsize=4096)(CommandLine.parse)


def command_prefixes(command: str) -> tuple[str, ...]:
    """Permission-style prefixes for each command of a shell command line.

    Splits ``command`` at shell operators via the bash grammar, then per command
    unwraps leading wrappers (``sudo``, ``env``, ``timeout``, …) to reach the real
    executable. A multi-level tool (``git``, ``docker``, …) keeps its first
    non-flag argument as the subcommand.

    Example:
        >>> command_prefixes("sudo git push -f && echo hi")
        ('git push', 'echo')
    """
    return parse_command_line(command).prefixes


def bulk_command_prefixes(commands: Sequence[str]) -> list[tuple[str, ...]]:
    """``command_prefixes`` over many commands at once, on the Rust fast path."""
    from cc_transcript import _native

    return [tuple(prefixes) for prefixes in _native.command_prefixes(list(commands))]


# P6 flip: native views supersede the (now-dead) Python bodies above; sub-lane 2 deletes them.
from cc_transcript._native import (  # noqa: E402
    Command as Command,  # pyright: ignore[reportAssignmentType]
    CommandLine as CommandLine,  # pyright: ignore[reportAssignmentType]
    CommandLineQuery as CommandLineQuery,  # pyright: ignore[reportAssignmentType]
    Occurrence as Occurrence,  # pyright: ignore[reportAssignmentType]
    Redirect as Redirect,  # pyright: ignore[reportAssignmentType]
)

parse_command_line = lru_cache(maxsize=4096)(CommandLine.parse)
