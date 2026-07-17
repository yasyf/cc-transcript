from __future__ import annotations

import json
from pathlib import Path

from scripts.gen_corpus import REPO_ROOT

CORPUS_PREFIX = ".fixtures/corpus/"

CORPUS_MANIFEST: frozenset[str] = frozenset(
    record["file"].split(CORPUS_PREFIX, 1)[1]
    for record in json.loads((REPO_ROOT / "tests" / "testdata" / "views_golden.json").read_text())["files"]
    if CORPUS_PREFIX in record["file"]
)


def corpus_current(out: Path) -> bool:
    return out.exists() and frozenset(str(p.relative_to(out)) for p in out.rglob("*.jsonl")) == CORPUS_MANIFEST
