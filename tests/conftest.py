from __future__ import annotations

import pytest

from scripts.gen_corpus import DEFAULT_OUT, DEFAULT_SEED, FILE_PLAN, generate


@pytest.fixture(scope="session", autouse=True)
def bench_corpus() -> None:
    if not DEFAULT_OUT.exists() or sum(1 for _ in DEFAULT_OUT.rglob("*.jsonl")) != len(FILE_PLAN):
        generate(DEFAULT_OUT, DEFAULT_SEED)
