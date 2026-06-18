"""Headless structured completions via spawnllm's first ready CLI backend.

Argv construction and envelope parsing come from the shared ``spawnllm`` library;
the spawn stays local (``anyio.run_process``). The backend is whichever of
spawnllm's CLIs is installed and authenticated (:func:`spawnllm.select_backend`),
using the caller's existing CLI auth — no API key, no pinned provider.
``spawnllm`` and ``pydantic`` load lazily inside each function, so importing the
judge package needs no extra installed.
"""

from __future__ import annotations

import os
import subprocess
from functools import cache
from typing import TYPE_CHECKING, cast

import anyio

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable

    from pydantic import BaseModel
    from spawnllm import LlmBackend, TModel

LLM_TIMEOUT = 180


@cache
def default_backend() -> LlmBackend:
    """The first installed, authenticated spawnllm backend, cached per process.

    Raises:
        spawnllm.BackendUnavailable: When no backend is installed and authenticated.
    """
    from spawnllm import select_backend

    return select_backend()


def resolved_model(tier: TModel) -> str:
    """The concrete model name the active backend runs for an abstract tier.

    The verdict store keys on this string; it reflects whichever backend
    :func:`spawnllm.select_backend` resolves, so a judged corpus stays coherent
    within one backend environment.
    """
    return default_backend().models[tier]


async def run_structured_on[M: BaseModel](
    backend: LlmBackend, prompt: str, *, response_model: type[M], tier: TModel, timeout: int = LLM_TIMEOUT
) -> M:
    """Runs one structured completion on an explicit ``backend``.

    A caller that resolves a backend once per pass passes it here, so a large pass
    probes auth once; :func:`run_structured` is the select-and-run convenience.

    Raises:
        subprocess.SubprocessError: If the backend CLI exits non-zero or times out.
        pydantic.ValidationError: If the response does not match the schema.
    """
    from spawnllm import resolve_schema_path, schema_for

    argv, stdin = backend.invocation(
        prompt,
        model=backend.models[tier],
        schema_path=resolve_schema_path(backend, schema_for(response_model)),
        agent=False,
    )
    try:
        with anyio.fail_after(timeout):
            result = await anyio.run_process(
                argv, input=stdin.encode() if stdin is not None else None, check=True, env=os.environ | backend.env()
            )
    except TimeoutError as exc:
        raise subprocess.TimeoutExpired(argv, timeout) from exc
    return cast(response_model, backend.parse_response(result.stdout.decode(), response_model))


async def run_structured[M: BaseModel](
    prompt: str, *, response_model: type[M], tier: TModel, timeout: int = LLM_TIMEOUT
) -> M:
    """Runs one structured completion on the first ready backend.

    The prompt is delivered to the CLI and the response forced into
    ``response_model``'s JSON schema. Backend selection is cached per process.

    Raises:
        spawnllm.BackendUnavailable: If no backend is installed and authenticated.
        subprocess.SubprocessError: If the backend CLI exits non-zero or times out.
        pydantic.ValidationError: If the response does not match the schema.
    """
    return await run_structured_on(default_backend(), prompt, response_model=response_model, tier=tier, timeout=timeout)


def structured_judge[M: BaseModel](
    response_model: type[M], *, tier: TModel, timeout: int = LLM_TIMEOUT
) -> Callable[[str], Awaitable[M]]:
    """Returns a prompt-to-verdict callable that plugs into :func:`run_verdicts`.

    Example:
        >>> judge = structured_judge(Verdict, tier="medium")
        >>> await run_verdicts(rows, prompt_for, judge, persist, concurrency=8)
    """
    return lambda prompt: run_structured(prompt, response_model=response_model, tier=tier, timeout=timeout)
