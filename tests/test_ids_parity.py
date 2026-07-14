"""Rust↔Python parity for the ids digest contract.

The Python ``cc_transcript.ids`` reference and the Rust ``_native`` pyfunctions
must produce byte-identical canonical JSON and tool digests. ``digest_golden.json``
(recorded from the Python impl by ``scripts/gen_ids_golden.py``) freezes both, so
this asserts Rust == Python == golden over corpus shapes and tricky numbers, and
that both sides reject integers past IEEE-754 double precision.
"""

from __future__ import annotations

import json
import math
import random
import struct
from pathlib import Path
from typing import Any

import pytest

from cc_transcript import _native
from cc_transcript.ids import canonical_json, tool_digest
from tests.support import requires_rust

GOLDEN = json.loads((Path(__file__).resolve().parent / "testdata" / "digest_golden.json").read_text(encoding="utf-8"))
CORPUS = GOLDEN["corpus"]
TRICKY = GOLDEN["tricky"]
RAW = GOLDEN["raw"]
ERRORS = GOLDEN["errors"]


@requires_rust
@pytest.mark.parametrize("row", CORPUS, ids=[f"{i}-{row['tool']}" for i, row in enumerate(CORPUS)])
def test_corpus_canonical_and_digest_parity(row: dict[str, Any]) -> None:
    value_json = json.dumps(row["input"])
    assert canonical_json(row["input"]).decode() == row["canonical"]
    assert _native.ids_canonical_json(value_json) == row["canonical"]
    assert tool_digest(row["tool"], row["input"]) == row["digest"]
    assert _native.ids_tool_digest(row["tool"], value_json) == row["digest"]


@requires_rust
@pytest.mark.parametrize("row", TRICKY, ids=[str(i) for i in range(len(TRICKY))])
def test_tricky_canonical_parity(row: dict[str, Any]) -> None:
    assert canonical_json(row["value"]).decode() == row["canonical"]
    assert _native.ids_canonical_json(json.dumps(row["value"])) == row["canonical"]


@requires_rust
@pytest.mark.parametrize("row", RAW, ids=[row["json"] for row in RAW])
def test_raw_json_canonical_parity(row: dict[str, Any]) -> None:
    assert canonical_json(json.loads(row["json"])).decode() == row["canonical"]
    assert _native.ids_canonical_json(row["json"]) == row["canonical"]


@requires_rust
@pytest.mark.parametrize("row", ERRORS, ids=[row["json"] for row in ERRORS])
def test_out_of_range_integers_reject_on_both_sides(row: dict[str, Any]) -> None:
    with pytest.raises(ValueError):
        canonical_json(json.loads(row["json"]))
    with pytest.raises(ValueError):
        _native.ids_canonical_json(row["json"])


@requires_rust
def test_random_float_shortest_repr_fuzz() -> None:
    """10k seeded bit-random f64s must serialize identically in Python and Rust.

    Bit-pattern randomness reaches the shortest-repr ties (e.g. 698957826421429.2)
    that Rust std / plain ryu round the opposite way from Python / ECMAScript.
    """
    rng = random.Random(0xC0FFEE)
    values = [698957826421429.2]
    while len(values) < 10_000:
        candidate = struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0]
        if math.isfinite(candidate):
            values.append(candidate)
    for value in values:
        payload = json.dumps(value)
        assert _native.ids_canonical_json(payload) == canonical_json(value).decode(), payload
