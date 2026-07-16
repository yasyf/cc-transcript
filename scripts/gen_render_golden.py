"""Generate the renderer parity golden.

Freezes the renderer outputs so ``tests/test_render_parity.py`` can replay them. The
raw-path renderers live only in the native core, so their sections come from
``_native``; ``render_tool_call`` stays a Python object renderer, so its section is the
frozen Python value the parity test still drift-guards against ``_native``. Three
sections:

* ``tool_calls`` — ``render_tool_call`` over every corpus tool-use input (deduped by
  ``(name, value-shape)`` like the toolcall golden), each of the 15 typed classes, and
  hand-built edge inputs (unicode clipped by code point, huge payloads straddling the
  clip boundary, multi-line hunks, and non-string ``primary_arg`` values), at several
  budgets.
* ``transcripts`` — ``render_compact_lines`` (several width/thinking/uuids combos),
  ``render_haystacks`` (several ``where`` sets), and per-transcript ``render_stats`` over
  the shared parser fixture (``tests.support.fixture_bytes`` — every event/attachment
  kind) plus the smallest corpus files. Raw JSONL is embedded base64, so the test never
  needs the gitignored corpus. Cap: the ``CORPUS_SAMPLE`` smallest files (the full corpus
  is the ``render`` bench's job, not the parity golden's).
* ``stats_all`` — ``render_stats`` over every sampled transcript at once.

Run: ``uv run --no-sync python scripts/gen_render_golden.py``
"""

from __future__ import annotations

import base64
import json
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import orjson

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from cc_transcript import _native  # noqa: E402
from cc_transcript.render import Budget, render_tool_call  # noqa: E402
from cc_transcript.tools import parse_tool_call  # noqa: E402
from tests.support import fixture_bytes  # noqa: E402

CORPUS = REPO_ROOT / ".fixtures" / "corpus"
GOLDEN = REPO_ROOT / "tests" / "testdata" / "render_golden.json"
CORPUS_SAMPLE = 3

BUDGETS: tuple[tuple[int, int], ...] = ((700, 1500), (100, 100), (24, 24), (5, 5))
COMPACT_COMBOS: tuple[tuple[int, bool, bool], ...] = ((100, True, True), (0, False, False), (200, True, False))
HAYSTACK_COMBOS: tuple[tuple[str, ...], ...] = (("text", "thinking", "tools"), ("text",), ("tools",), ("thinking",))

BIG = "x" * 2000
MULTILINE_NEW = "line1\nline2\n" + "y" * 2000
UNICODE_CMD = "echo 'héllo 🤖 漢字 café́ end'"

EDGE_CALLS: tuple[tuple[str, dict[str, Any]], ...] = (
    ("Bash", {"command": UNICODE_CMD}),
    ("Bash", {"command": BIG}),
    ("Edit", {"file_path": "/big.py", "old_string": BIG, "new_string": MULTILINE_NEW}),
    ("Edit", {"file_path": "/u.py", "old_string": "café́", "new_string": "漢字\n漢字漢字"}),
    (
        "MultiEdit",
        {
            "file_path": "/m.py",
            "edits": [
                {"old_string": "a\nb", "new_string": "c\nd"},
                {"old_string": BIG, "new_string": "z"},
            ],
        },
    ),
    ("Write", {"file_path": "/w.py", "content": "# generated 🤖\n" + BIG}),
    ("Read", {"file_path": "/r.py", "limit": 40}),
    ("NotebookEdit", {"notebook_path": "/nb.ipynb", "new_source": "print(1)\nprint(2)"}),
    ("Grep", {"pattern": "parse_bytes", "output_mode": "content"}),
    ("Glob", {"pattern": "**/*.py"}),
    ("Agent", {"description": "explore", "prompt": "map the parser", "subagent_type": "Explore"}),
    ("Workflow", {"script": "print(1)", "name": "w"}),
    ("Skill", {"skill": "codex", "args": "review the diff"}),
    ("TaskCreate", {"subject": "port render", "description": "the one renderer"}),
    ("TaskUpdate", {"taskId": "t1", "status": "completed"}),
    ("ExitPlanMode", {"plan": "step one\nstep two"}),
    # primary_arg over non-string values: str() of int/bool/null and repr() inside containers.
    ("TodoWrite", {"todos": [{"content": "a", "status": "pending"}]}),
    ("Mystery", {"count": 5}),
    ("Mystery", {"flag": True, "ratio": 1.5}),
    ("Mystery", {"x": None}),
    ("Read", {"file_path": None, "extra": "y"}),
    ("Mystery", {"items": ["it's", 'say "hi"', "both ' and \""]}),
    # Non-printable code points inside a container: py_string_repr must \x/\u-escape like repr().
    ("Mystery", {"chars": ["z\x01w", "a\x85b", "x y", "\xa0", "é 漢 😀"]}),
    ("Mystery", {}),
)

# Raw JSON with DUPLICATE keys (a dict literal cannot express these); json.loads and the
# Rust dedup both resolve last-wins keeping first position.
DUP_KEY_CALLS: tuple[tuple[str, str], ...] = (
    ("Read", '{"file_path": "/first.py", "file_path": "/last.py", "limit": 1, "limit": 2}'),
    ("Grep", '{"pattern": "first", "pattern": "last", "output_mode": "content"}'),
    ("Mystery", '{"z": 1, "y": 2, "z": 3}'),
    ("TodoWrite", '{"todos": [1], "todos": [2, 3]}'),
)

# Raw JSON with non-canonical NUMBER lexemes, passed to Rust verbatim so json.dumps
# cannot launder them into Python's layout first (the parity-test masking codex flagged).
RAW_JSON_CALLS: tuple[tuple[str, str], ...] = (
    ("Mystery", '{"ratio": 1e-7}'),
    ("Mystery", '{"ratio": -0}'),
    ("Mystery", '{"ratio": 1e16}'),
    ("Mystery", '{"ratio": 1e15}'),
    ("Mystery", '{"ratio": 0.1}'),
    ("Mystery", '{"ratio": -0.5}'),
    ("Mystery", '{"tie": 698957826421429.2}'),
    ("Mystery", '{"count": 99999999999999999999}'),
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


def corpus_calls() -> list[tuple[str, dict[str, Any]]]:
    calls: list[tuple[str, dict[str, Any]]] = []
    seen: set[object] = set()
    for path in sorted(CORPUS.rglob("*.jsonl")):
        for line in path.read_bytes().splitlines():
            if not line:
                continue
            content = orjson.loads(line).get("message", {}).get("content")
            if not isinstance(content, list):
                continue
            for block in content:
                if block.get("type") != "tool_use":
                    continue
                if (key := (block["name"], classify(block["input"]))) not in seen:
                    seen.add(key)
                    calls.append((block["name"], block["input"]))
    return calls


def tool_call_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for name, tool_input in [*corpus_calls(), *EDGE_CALLS]:
        call = parse_tool_call(name, tool_input, on_error="other")
        input_json = json.dumps(tool_input)
        for turn_chars, tool_chars in BUDGETS:
            cases.append(
                {
                    "name": name,
                    "input_json": input_json,
                    "budget": [turn_chars, tool_chars],
                    "expected": render_tool_call(call, budget=Budget(turn_chars=turn_chars, tool_chars=tool_chars)),
                }
            )
    return cases


def dup_key_call_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for name, input_json in DUP_KEY_CALLS:
        call = parse_tool_call(name, json.loads(input_json), on_error="other")
        for turn_chars, tool_chars in BUDGETS[:2]:
            cases.append(
                {
                    "name": name,
                    "input_json": input_json,
                    "budget": [turn_chars, tool_chars],
                    "expected": render_tool_call(call, budget=Budget(turn_chars=turn_chars, tool_chars=tool_chars)),
                }
            )
    return cases


def dup_key_transcript() -> bytes:
    lines = [
        '{"type":"user","uuid":"du1","sessionId":"dupsess","timestamp":"2026-01-02T03:04:05.000Z",'
        '"message":{"role":"user","content":"dup key session"}}',
        '{"type":"assistant","uuid":"da1","parentUuid":"du1","sessionId":"dupsess",'
        '"timestamp":"2026-01-02T03:04:06.000Z","message":{"role":"assistant","model":"claude-opus-4-8",'
        '"stop_reason":"tool_use","content":[{"type":"text","text":"editing"},'
        '{"type":"tool_use","id":"dtool","name":"Read",'
        '"input":{"file_path":"/first.py","file_path":"/last.py","limit":1,"limit":2}}]}}',
        '{"type":"user","uuid":"du2","parentUuid":"da1","sessionId":"dupsess",'
        '"timestamp":"2026-01-02T03:04:07.000Z","message":{"role":"user",'
        '"content":[{"type":"tool_result","tool_use_id":"dtool","content":"ok","is_error":false}]}}',
    ]
    return "\n".join(lines).encode("utf-8")


def raw_json_call_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for name, input_json in RAW_JSON_CALLS:
        call = parse_tool_call(name, json.loads(input_json), on_error="other")
        for turn_chars, tool_chars in BUDGETS[:2]:
            cases.append(
                {
                    "name": name,
                    "input_json": input_json,
                    "budget": [turn_chars, tool_chars],
                    "expected": render_tool_call(call, budget=Budget(turn_chars=turn_chars, tool_chars=tool_chars)),
                }
            )
    return cases


def numeric_transcript() -> bytes:
    lines = [
        '{"type":"user","uuid":"nu1","sessionId":"numsess","timestamp":"2026-01-02T03:04:05.000Z",'
        '"message":{"role":"user","content":"numeric session"}}',
        '{"type":"assistant","uuid":"na1","parentUuid":"nu1","sessionId":"numsess",'
        '"timestamp":"2026-01-02T03:04:06.000Z","message":{"role":"assistant","model":"claude-opus-4-8",'
        '"stop_reason":"tool_use","content":[{"type":"text","text":"reading"},'
        '{"type":"tool_use","id":"ntool","name":"Read",'
        '"input":{"file_path":"/n.py","ratio":1e-7,"big":1e16,"neg":-0,"tie":698957826421429.2}}]}}',
        '{"type":"user","uuid":"nu2","parentUuid":"na1","sessionId":"numsess",'
        '"timestamp":"2026-01-02T03:04:07.000Z","message":{"role":"user",'
        '"content":[{"type":"tool_result","tool_use_id":"ntool","content":"ok","is_error":false}]}}',
    ]
    return "\n".join(lines).encode("utf-8")


def transcript_case(id_: str, raw: bytes) -> dict[str, Any]:
    return {
        "id": id_,
        "jsonl_b64": base64.b64encode(raw).decode("ascii"),
        "compact": [
            {
                "width": width,
                "thinking": thinking,
                "uuids": uuids,
                "lines": _native.render_compact_lines(raw, width, thinking, uuids),
            }
            for width, thinking, uuids in COMPACT_COMBOS
        ],
        "haystack": [
            {
                "where": list(wheres),
                "lines": _native.render_haystacks(raw, list(wheres)),
            }
            for wheres in HAYSTACK_COMBOS
        ],
        "stats": _native.render_stats([raw]),
    }


def sample_transcripts() -> list[tuple[str, bytes]]:
    smallest = sorted(CORPUS.rglob("*.jsonl"), key=lambda p: (p.stat().st_size, str(p)))[:CORPUS_SAMPLE]
    return [
        ("fixture", fixture_bytes()),
        ("dup-key", dup_key_transcript()),
        ("numeric", numeric_transcript()),
        *((str(p.relative_to(REPO_ROOT)), p.read_bytes()) for p in smallest),
    ]


def main() -> None:
    samples = sample_transcripts()
    transcripts = [transcript_case(id_, raw) for id_, raw in samples]
    golden = {
        "tool_calls": tool_call_cases(),
        "dup_key_calls": dup_key_call_cases(),
        "raw_json_calls": raw_json_call_cases(),
        "transcripts": transcripts,
        "stats_all": _native.render_stats([raw for _, raw in samples]),
    }
    GOLDEN.write_text(json.dumps(golden, indent=2, ensure_ascii=False), encoding="utf-8")
    print(
        f"wrote {len(golden['tool_calls'])} tool-call + {len(golden['dup_key_calls'])} dup-key + "
        f"{len(golden['raw_json_calls'])} raw-json cases + {len(transcripts)} transcripts to {GOLDEN.name}"
    )


if __name__ == "__main__":
    main()
