from __future__ import annotations

import pytest

from scripts.gen_corpus import DEFAULT_OUT, DEFAULT_SEED, generate
from tests.corpus import corpus_current


@pytest.fixture(scope="session", autouse=True)
def bench_corpus() -> None:
    if not corpus_current(DEFAULT_OUT):
        generate(DEFAULT_OUT, DEFAULT_SEED)
