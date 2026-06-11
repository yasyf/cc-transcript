"""Headless structured completions via the ``claude`` CLI, behind the ``[llm]`` extra.

Argv construction and envelope parsing come from the shared ``spawnllm`` library;
the spawn stays local (``anyio.run_process``). It uses the user's existing Claude
Code auth (no API key). ``spawnllm`` and ``pydantic`` load lazily inside each
function, so importing the mining domain needs no extra installed.
"""

from __future__ import annotations

import os
import subprocess
from typing import TYPE_CHECKING

import anyio

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable

    from pydantic import BaseModel
    from spawnllm import TModel

CLAUDE_TIMEOUT = 180


def resolved_model(tier: TModel) -> str:
    """Returns the concrete Claude model name for an abstract tier.

    A verdict store's unique key includes the model string, so the resolution
    must stay byte-identical across releases for a judged corpus to stay valid.
    """
    from spawnllm import ClaudeCliBackend

    return ClaudeCliBackend.models[tier]


async def run_structured[M: BaseModel](
    prompt: str, *, response_model: type[M], tier: TModel, timeout: int = CLAUDE_TIMEOUT
) -> M:
    """Runs one headless ``claude`` turn and parses its structured output.

    The prompt is delivered over stdin and the response is forced into
    ``response_model``'s JSON schema via the CLI's ``--json-schema`` flag. The
    structured path runs with an empty system prompt, so all instructions must
    live in ``prompt``.

    Args:
        prompt: The full prompt, instructions included.
        response_model: The pydantic model the response must validate against.
        tier: The abstract model tier to run, resolved by the Claude backend.
        timeout: The per-call wall-clock budget in seconds.

    Returns:
        The validated ``response_model`` instance.

    Raises:
        subprocess.SubprocessError: If ``claude`` exits non-zero or times out.
        pydantic.ValidationError: If the response does not match the schema.
    """
    from spawnllm import ClaudeCliBackend, parse_structured_output, resolve_schema_path, schema_for

    backend = ClaudeCliBackend()
    argv = backend.build_command(
        backend.models[tier], resolve_schema_path(backend, schema_for(response_model)), agent=False
    )
    try:
        with anyio.fail_after(timeout):
            result = await anyio.run_process(argv, input=prompt.encode(), check=True, env=os.environ | backend.env())
    except TimeoutError as exc:
        raise subprocess.TimeoutExpired(argv, timeout) from exc
    return parse_structured_output(result.stdout.decode(), response_model)


def structured_judge[M: BaseModel](
    response_model: type[M], *, tier: TModel, timeout: int = CLAUDE_TIMEOUT
) -> Callable[[str], Awaitable[M]]:
    """Returns a prompt-to-verdict callable that plugs into :func:`run_verdicts`.

    Args:
        response_model: The pydantic model each response must validate against.
        tier: The abstract model tier to run.
        timeout: The per-call wall-clock budget in seconds.

    Returns:
        A callable awaiting one structured completion per prompt.

    Example:
        >>> judge = structured_judge(Verdict, tier="medium")
        >>> await run_verdicts(rows, prompt_for, judge, persist, concurrency=8)
    """
    return lambda prompt: run_structured(prompt, response_model=response_model, tier=tier, timeout=timeout)
