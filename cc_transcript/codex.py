"""The codex session surface: discovery, resolution, and per-rollout session info.

A sync facade over the native codex functions, mirroring
:mod:`cc_transcript.discovery` for the ``~/.codex/sessions`` tree. The sessions-root
default is a facade concern; the native side scans exactly the root it is handed.

There is no parse function here: :func:`cc_transcript.parse` is already
provider-transparent — it sniffs a codex rollout, lowers it into the native event
model, and yields the same :class:`~cc_transcript.models.Transcript` view a Claude
transcript produces, with ``provider == "codex"``. This module adds only the codex
identity, lifecycle, and token-usage surface that the lowered event stream drops.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Literal

if TYPE_CHECKING:
    from cc_transcript.ids import SessionId

SESSIONS_ROOT = Path.home() / ".codex" / "sessions"

Lifecycle = Literal["uninitialized", "no_instrumentation", "open", "completed", "aborted"]
"""A codex rollout's turn-lifecycle state.

``uninitialized`` — no content at all; ``no_instrumentation`` — content but no
``task_started``/``task_complete`` brackets; ``open`` — the latest turn is still
running; ``completed`` — the latest turn finished; ``aborted`` — the latest turn was
interrupted.
"""


@dataclass(frozen=True, slots=True)
class CodexRollout:
    """A rollout found in the codex sessions tree.

    Attributes:
        path: The rollout's path.
        session_id: The session id encoded in the rollout filename.
        compressed: Whether the rollout is a ``.jsonl.zst`` file.
    """

    path: Path
    session_id: str
    compressed: bool


@dataclass(frozen=True, slots=True)
class CodexUsage:
    """Session-level token totals lowered from a codex rollout.

    The totals are the last observed cumulative ``total_token_usage`` fields — codex
    ``token_count`` events carry running totals, not per-turn deltas — so this is the
    session's final accounting, not a sum.

    Attributes:
        input_tokens: Total input tokens, or None when never reported.
        cached_input_tokens: Cached input tokens, or None when never reported.
        output_tokens: Output tokens, or None when never reported.
        reasoning_output_tokens: Reasoning output tokens, or None when never reported.
        total_tokens: Total tokens, or None when never reported.
        model_context_window: The model's context window, or None when never reported.
        token_count_events: The number of ``token_count`` events observed.
    """

    input_tokens: int | None
    cached_input_tokens: int | None
    output_tokens: int | None
    reasoning_output_tokens: int | None
    total_tokens: int | None
    model_context_window: int | None
    token_count_events: int


@dataclass(frozen=True, slots=True)
class CodexPendingItem:
    """A dangling tool call in the rollout's open turn.

    Attributes:
        tool_use_id: The call's ``call_id``.
        name: The tool's name, such as ``exec``.
        kind: How the call contributes to the verdict; always ``mid_tool`` for codex.
    """

    tool_use_id: str | None
    name: str
    kind: str


@dataclass(frozen=True, slots=True)
class CodexSessionInfo:
    """The identity, lifecycle, and usage of one codex rollout.

    Attributes:
        rollout_thread_id: This rollout's own thread id (the session-meta ``id``).
        session_id: The logical session id; a forked rollout inherits its origin's.
        parent_thread_id: The spawning thread's id for a subagent rollout, else None.
        forked_from_id: The origin thread's id when this rollout is a fork, else None.
        cwd: The working directory recorded in the session meta.
        originator: What launched the session, such as ``codex_exec``.
        cli_version: The codex CLI version that wrote the rollout.
        model_provider: The model provider, such as ``openai``.
        lifecycle: The latest turn's lifecycle state.
        turn_id: The latest turn's id, or None before any turn brackets.
        pending: The dangling tool calls in an open turn, in document order.
        last_event_epoch: The latest envelope timestamp as epoch seconds, or None.
        usage: The session token totals, or None when no ``token_count`` event occurred.
    """

    rollout_thread_id: str | None
    session_id: str | None
    parent_thread_id: str | None
    forked_from_id: str | None
    cwd: str | None
    originator: str | None
    cli_version: str | None
    model_provider: str | None
    lifecycle: Lifecycle
    turn_id: str | None
    pending: tuple[CodexPendingItem, ...]
    last_event_epoch: int | None
    usage: CodexUsage | None


def sessions_root(root: Path | None = None) -> Path:
    """The codex sessions root, defaulting to ``~/.codex/sessions``.

    Args:
        root: An explicit root; when None, :data:`SESSIONS_ROOT`.

    Returns:
        The root that :func:`discover` and :func:`find_transcript` scan.
    """
    return root or SESSIONS_ROOT


def discover(root: Path | None = None) -> tuple[CodexRollout, ...]:
    """Every codex rollout under ``root``, newest first.

    Rollouts live as ``rollout-<timestamp>-<session_id>.jsonl`` (or ``.jsonl.zst``)
    files under the date-sharded sessions tree. Each result preserves its path,
    session id, and compression state.

    Args:
        root: The sessions root; when None, :func:`sessions_root`.

    Returns:
        The rollouts, newest first; ``()`` when the root does not exist.

    Example:
        >>> for rollout in discover():
        ...     print(rollout.session_id, rollout.path)
    """
    from cc_transcript import _native

    return tuple(CodexRollout(*rollout) for rollout in _native.codex_discover(sessions_root(root)))


def find_transcript(session_id: SessionId, root: Path | None = None) -> Path | None:
    """Locates ``session_id``'s rollout under the codex sessions tree.

    Only uncompressed ``.jsonl`` rollouts resolve. Compressed rollouts appear in
    ``discover()`` but do not resolve until zstd support lands. The newest wins among
    same-id uncompressed rollouts.

    Args:
        session_id: The codex session id, as it appears in the rollout filename.
        root: The sessions root; when None, :func:`sessions_root`.

    Returns:
        The rollout path, or None when no rollout carries that session id.
    """
    from cc_transcript import _native

    return _native.codex_resolve(session_id, sessions_root(root))


def session_info(path: Path) -> CodexSessionInfo:
    """The identity, lifecycle, and token usage of the rollout at ``path``.

    The native extension reads and folds the rollout in one pass, returning the
    session identity, the turn lifecycle, any dangling calls, and the token totals.

    Args:
        path: The ``.jsonl`` rollout to inspect.

    Returns:
        The rollout's session info.
    """
    from cc_transcript import _native

    payload = _native.codex_session_info(str(path))
    identity = payload["identity"]
    lifecycle = payload["lifecycle"]
    usage = payload["usage"]
    return CodexSessionInfo(
        rollout_thread_id=identity["rollout_thread_id"],
        session_id=identity["session_id"],
        parent_thread_id=identity["parent_thread_id"],
        forked_from_id=identity["forked_from_id"],
        cwd=identity["cwd"],
        originator=identity["originator"],
        cli_version=identity["cli_version"],
        model_provider=identity["model_provider"],
        lifecycle=lifecycle["state"],
        turn_id=lifecycle["turn_id"],
        pending=tuple(CodexPendingItem(**item) for item in payload["pending"]),
        last_event_epoch=payload["last_event_epoch"],
        usage=CodexUsage(**usage) if usage is not None else None,
    )
