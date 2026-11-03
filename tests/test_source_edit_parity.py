"""Parity: the owner's ``has_source_edit`` applies the rules Python's :class:`FileRef` defines.

``testdata/source_edit_cases.json`` is shared with a Rust test in ``snapshot_prepared.rs``, so
each side is pinned to the same answers: captain-hook's ``EditedSource`` rules here, the owner's
prepared predicate there.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from cc_transcript.query import FileRef

TABLE = json.loads((Path(__file__).resolve().parent / "testdata" / "source_edit_cases.json").read_text())


def in_project(path: str, root: str | None) -> bool:
    return not Path(path).is_absolute() or root is None or Path(path).resolve().is_relative_to(Path(root).resolve())


@pytest.mark.parametrize("case", TABLE["cases"], ids=lambda case: case["path"])
def test_python_rules_give_the_shared_answer(case: dict[str, object]) -> None:
    file = FileRef(str(case["path"]))
    assert (
        not file.is_test
        and file.suffix not in TABLE["exclude_suffixes"]
        and not file.under(*TABLE["exclude_prefixes"])
        and in_project(file.path, case["root"])
    ) is case["expected"]
