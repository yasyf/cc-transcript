"""Generate the tool-call parity golden from the Python reference over the corpus.

Walks every ``tool_use``/``tool_result`` block in ``.fixtures/corpus`` (build via
scripts/gen_corpus.py), dedupes by ``(tool_name, value-class shape)`` — the shape
keeps branch-classifying features (a bool's value, null vs int, list/dict structure)
so cases that take different parse branches survive — appends the hand-built edge
cases from ``tests/toolcall_cases.py``, and freezes each case's Python projection to
``tests/testdata/toolcall_golden.json``. The ``raise_calls`` block pins the strict
ToolInputError messages. ``tests/test_toolcall_parity.py`` replays it against both
backends.

Run: ``uv run --no-sync python scripts/gen_toolcall_golden.py``
"""

from __future__ import annotations

import json
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import orjson

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from cc_transcript.tools import ToolInputError, parse_tool_call, parse_tool_result  # noqa: E402
from tests.toolcall_cases import EDGE_CALLS, EDGE_RESULTS, STRICT_RAISERS, project  # noqa: E402

CORPUS = REPO_ROOT / ".fixtures" / "corpus"
GOLDEN = REPO_ROOT / "tests" / "testdata" / "toolcall_golden.json"

# Raw JSON with duplicate keys (a dict can't carry them); orjson keeps the last, pinning that
# the Rust path yields last-wins end-to-end through toolcall_parse / parse_tool_result.
DUP_KEY_CALLS: tuple[tuple[str, str], ...] = (
    (
        "Edit",
        '{"file_path":"/first.py","file_path":"/last.py","old_string":"a","old_string":"b",'
        '"new_string":"x","new_string":"y"}',
    ),
    ("Bash", '{"command":"ls","command":"pwd -P"}'),
    (
        "MultiEdit",
        '{"file_path":"/a.py","file_path":"/b.py","edits":'
        '[{"old_string":"p","new_string":"q","old_string":"pp","new_string":"qq"}]}',
    ),
)
DUP_KEY_RESULTS: tuple[tuple[str, str], ...] = (
    ("Bash", '{"stdout":"first","stdout":"second","stderr":"","interrupted":false}'),
    ("Edit", '{"filePath":"/a.py","filePath":"/b.py","oldString":"x","newString":"y"}'),
)


def classify(value: object) -> object:
    match value:
        case bool():
            return f"bool:{value}"
        case int():
            return "int"
        case float():
            return "float"
        case str():
            return "str"
        case None:
            return "null"
        case Mapping():
            return "dict", tuple(sorted((k, classify(v)) for k, v in value.items()))
        case list():
            return "list", tuple(classify(item) for item in value)
        case _:
            return type(value).__name__


def corpus_cases() -> tuple[list[tuple[str, dict[str, Any]]], list[tuple[str, object]]]:
    id_to_name: dict[str, str] = {}
    calls: list[tuple[str, dict[str, Any]]] = []
    results: list[tuple[str, object]] = []
    seen: set[object] = set()
    for path in sorted(CORPUS.rglob("*.jsonl")):
        for line in path.read_bytes().splitlines():
            if not line:
                continue
            event = orjson.loads(line)
            content = event.get("message", {}).get("content")
            if not isinstance(content, list):
                continue
            for block in content:
                match block.get("type"):
                    case "tool_use":
                        id_to_name[block["id"]] = block["name"]
                        if (key := ("call", block["name"], classify(block["input"]))) not in seen:
                            seen.add(key)
                            calls.append((block["name"], block["input"]))
                    case "tool_result":
                        if (name := id_to_name.get(block["tool_use_id"])) is None:
                            continue
                        payload = event.get("toolUseResult")
                        if (key := ("result", name, classify(payload))) not in seen:
                            seen.add(key)
                            results.append((name, payload))
    return calls, results


def raise_case(name: str, tool_input: object) -> dict[str, object]:
    try:
        parse_tool_call(name, tool_input, on_error="raise")
    except ToolInputError as error:
        return {"tool": name, "input": tool_input, "message": str(error)}
    raise AssertionError(f"expected {name} {tool_input!r} to raise ToolInputError")


def main() -> None:
    corpus_calls, corpus_results = corpus_cases()
    calls = [
        {"tool": name, "input": tool_input, "expected": project(parse_tool_call(name, tool_input, on_error="other"))}
        for name, tool_input in [*corpus_calls, *EDGE_CALLS]
    ]
    results = [
        {"tool": name, "payload": payload, "expected": project(parse_tool_result(name, payload, on_error="other"))}
        for name, payload in [*corpus_results, *EDGE_RESULTS]
    ]
    raise_calls = [raise_case(name, tool_input) for name, tool_input in STRICT_RAISERS]
    dup_key_calls = [
        {"tool": name, "input_json": raw, "expected": project(parse_tool_call(name, orjson.loads(raw), on_error="other"))}
        for name, raw in DUP_KEY_CALLS
    ]
    dup_key_results = [
        {"tool": name, "payload_json": raw, "expected": project(parse_tool_result(name, orjson.loads(raw), on_error="other"))}
        for name, raw in DUP_KEY_RESULTS
    ]
    # stdlib json, not orjson: the golden carries ints beyond orjson's 64-bit ceiling.
    GOLDEN.write_text(
        json.dumps(
            {
                "calls": calls,
                "results": results,
                "raise_calls": raise_calls,
                "dup_key_calls": dup_key_calls,
                "dup_key_results": dup_key_results,
            },
            indent=2,
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )
    print(
        f"wrote {len(calls)} calls + {len(results)} results + {len(raise_calls)} raisers "
        f"+ {len(dup_key_calls)} dup calls + {len(dup_key_results)} dup results to {GOLDEN.name}"
    )


if __name__ == "__main__":
    main()
