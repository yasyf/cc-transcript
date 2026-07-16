"""The one bash-command parsing layer, re-exported from the native core.

Parses a raw shell string into typed commands — executable, arguments,
environment assignments, and redirects — split at pipeline and list operators.
The :class:`Command`, :class:`CommandLine`, :class:`CommandLineQuery`,
:class:`Occurrence`, and :class:`Redirect` views are native frozen objects
re-exported at this stable import path. :func:`parse_command_line` is the cached
entry point; :func:`command_prefixes` distills each command to its
permission-style prefix (``"git commit"``, ``"docker compose"``).
"""

from __future__ import annotations

from functools import lru_cache
from typing import TYPE_CHECKING

from cc_transcript._native import Command as Command
from cc_transcript._native import CommandLine as CommandLine
from cc_transcript._native import CommandLineQuery as CommandLineQuery
from cc_transcript._native import Occurrence as Occurrence
from cc_transcript._native import Redirect as Redirect

if TYPE_CHECKING:
    from collections.abc import Sequence

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
