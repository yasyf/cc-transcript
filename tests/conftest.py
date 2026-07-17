from __future__ import annotations

import pytest

from scripts.gen_corpus import DEFAULT_OUT, DEFAULT_SEED, generate


@pytest.fixture(scope="session", autouse=True)
def bench_corpus() -> None:
    if not DEFAULT_OUT.exists():
        generate(DEFAULT_OUT, DEFAULT_SEED)
