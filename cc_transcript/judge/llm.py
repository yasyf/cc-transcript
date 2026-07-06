"""Headless structured completions via spawnllm's first ready CLI backend.

The structured call comes from the shared ``spawnllm`` library;
:func:`spawnllm.extract` resolves a backend, runs the spawn, and validates the
response into the model. The backend is whichever of spawnllm's CLIs is
installed and authenticated (:func:`spawnllm.select_backend`), using the
caller's existing CLI auth — no API key, no pinned provider. ``spawnllm`` and
``pydantic`` load lazily inside each function, so importing the judge package
needs no extra installed.
"""

from __future__ import annotations

import json
from functools import cache
from typing import TYPE_CHECKING

from cc_transcript.judge.verdicts import JudgeError

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


def structured_judge[M: BaseModel](
    response_model: type[M], *, tier: TModel, timeout: int = LLM_TIMEOUT
) -> Callable[[str], Awaitable[M]]:
    """Returns a prompt-to-verdict callable that plugs into :func:`run_verdicts`.

    Each call runs one structured completion on the cached default backend via
    :func:`spawnllm.extract`, which validates the response into ``response_model``.
    Provider and transport failures — a backend call error, no ready backend, a
    timeout, or a non-conforming response — surface from the returned callable as
    :class:`~cc_transcript.judge.verdicts.JudgeError`, the one exception
    :func:`run_verdicts` counts as failed and retries next pass.

    Example:
        >>> judge = structured_judge(Verdict, tier="medium")
        >>> await run_verdicts(rows, prompt_for, judge, persist, concurrency=8)
    """
    from pydantic import ValidationError
    from spawnllm import BackendCallError, BackendUnavailable, extract

    async def judge(prompt: str) -> M:
        try:
            return await extract(prompt, response_model, backend=default_backend(), model=tier, timeout=timeout)
        except (BackendCallError, BackendUnavailable, TimeoutError, ValidationError, json.JSONDecodeError) as error:
            raise JudgeError(str(error)) from error

    return judge
