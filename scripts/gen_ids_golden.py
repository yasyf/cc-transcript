"""Record the ids parity golden under ``tests/testdata/digest_golden.json``.

Regenerates the deterministic corpus (``scripts/gen_corpus.py``), harvests every
distinct ``(tool_name, input)`` shape from its assistant ``tool_use`` blocks, and
appends a hand-built tricky-numbers section (float layout boundaries, integer
limits, unicode key sorting, string escaping, nesting, empties). Every row's
``canonical`` and ``digest`` come from the Python reference in ``cc_transcript.ids``,
so ``tests/test_ids_parity.py`` can assert the Rust port is byte-identical.

Run: ``uv run --no-sync python scripts/gen_ids_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import orjson

from cc_transcript.ids import canonical_json, tool_digest

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS_DIR = REPO_ROOT / ".fixtures" / "corpus"
GOLDEN = REPO_ROOT / "tests" / "testdata" / "digest_golden.json"

# Boundary values the random corpus never reaches: float layout arms, the
# ±(2^53-1) integer limits, UTF-16 key ordering, string escaping, nesting, empties.
TRICKY: tuple[object, ...] = (
    0.0,
    -0.0,
    1.0,
    -1.5,
    0.5,
    0.05,
    100.0,
    123.456,
    333333333.3333333,
    698957826421429.2,  # shortest-tie: Rust std / plain ryu pick .3, Python / ES pick .2
    0.1,
    0.2,
    0.3,
    3.141592653589793,
    1.2345678901234567,
    123456789.123456789,
    9007199254740992.0,
    1e-4,
    1e-5,
    0.000001,
    1e-7,
    1.5e-7,
    2.5e-8,
    1e15,
    1e16,
    1e17,
    1e20,
    1e21,
    1e22,
    9.999999999999997e22,
    5e-324,
    2.2250738585072014e-308,
    1.7976931348623157e308,
    -3.14,
    -1e-10,
    -2e30,
    0,
    1,
    -42,
    2**53 - 1,
    -(2**53 - 1),
    None,
    True,
    False,
    "a bare string with é and 漢字",
    {},
    [],
    {"z": 1, "a": 2, "m": 3},
    {"nested": {"b": [1, 2, {"c": None}], "a": "x"}},
    {"😀": "emoji", "€": "euro", "￿": "ffff"},
    {"controls": "\n\t\r\b\f\"\\"},
    {"mixed": [1, 1.5, "s", True, None, {}, []]},
    [0.1, 1e21, -0.0, 5e-324, 100.0],
    {"float_edge": 1e-6, "int_edge": 2**53 - 1},
)

# Raw JSON strings whose canonical form the Python object model can't express:
# duplicate keys, where json.loads keeps the last occurrence (escaped-equivalent too).
RAW: tuple[str, ...] = (
    '{"a":1,"a":2}',
    '{"a":1,"\\u0061":2}',
    '{"b":1,"a":2,"b":3}',
    '{"k":1,"\\u006b":2,"k":3}',
)

# Integers past IEEE-754 double precision — canonical_json rejects on both sides.
# NaN/inf can't round-trip strict JSON, so they stay in test_ids.py.
ERRORS: tuple[str, ...] = (
    "9007199254740993",
    "-9007199254740993",
    "100000000000000000000000000000",
)


def iter_tool_calls() -> Iterator[tuple[str, dict[str, Any]]]:
    for path in sorted(CORPUS_DIR.rglob("*.jsonl")):
        for line in path.read_bytes().splitlines():
            if not line.strip():
                continue
            message = orjson.loads(line).get("message")
            if not isinstance(message, dict) or not isinstance(content := message.get("content"), list):
                continue
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    yield block["name"], block["input"]


# A shape is (tool, key-set); the random corpus mints thousands of Read/Write rows
# that differ only by one scalar, so cap each shape to a deterministic sample.
CAP_PER_SHAPE = 12


def shape_key(row: dict[str, Any]) -> tuple[str, tuple[str, ...] | str]:
    tool_input = row["input"]
    keys = tuple(sorted(tool_input)) if isinstance(tool_input, dict) else type(tool_input).__name__
    return row["tool"], keys


def corpus_rows() -> list[dict[str, Any]]:
    seen: dict[tuple[str, str], dict[str, Any]] = {}
    for name, tool_input in iter_tool_calls():
        canonical = canonical_json(tool_input).decode()
        seen.setdefault(
            (name, canonical),
            {"tool": name, "input": tool_input, "canonical": canonical, "digest": tool_digest(name, tool_input)},
        )
    counts: dict[tuple[str, tuple[str, ...] | str], int] = {}
    rows: list[dict[str, Any]] = []
    for row in (seen[key] for key in sorted(seen)):
        shape = shape_key(row)
        counts[shape] = seen_count = counts.get(shape, 0) + 1
        if seen_count <= CAP_PER_SHAPE:
            rows.append(row)
    return rows


def main() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)
    corpus = corpus_rows()
    golden = {
        "corpus": corpus,
        "tricky": [{"value": value, "canonical": canonical_json(value).decode()} for value in TRICKY],
        "raw": [{"json": raw, "canonical": canonical_json(json.loads(raw)).decode()} for raw in RAW],
        "errors": [{"json": raw} for raw in ERRORS],
    }
    GOLDEN.write_bytes(orjson.dumps(golden, option=orjson.OPT_INDENT_2))
    print(
        f"wrote {len(corpus)} corpus + {len(TRICKY)} tricky + {len(RAW)} raw + {len(ERRORS)} errors "
        f"to {GOLDEN.relative_to(REPO_ROOT)}"
    )


if __name__ == "__main__":
    main()
