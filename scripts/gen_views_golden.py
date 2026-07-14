"""Materialize the views-equivalence golden from the eager dataclass path.

Pins every public field (recursively, JSON-projected via ``tests/viewgolden``)
of every object the current pre-inversion parse path materializes — over
committed edge fixtures written by this script and a deterministic sample of
the ``.fixtures`` bench corpus — into ``tests/testdata/views_golden.json``.
Edge fixtures record full projections plus reprs; corpus files record a capped
deterministic sample (``viewgolden.sample_indices``, ``SAMPLE_CAP`` events per
file; strings beyond ``STR_CAP`` chars pin as sha256+length). The P3 lazy views
are proved field-for-field against this golden by ``tests/test_views_parity.py``.

Regenerate only while the eager path still exists:

Run: ``uv run --no-sync python scripts/gen_views_golden.py``
(regenerate the corpus first: ``uv run --no-sync python scripts/gen_corpus.py``)
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path
from typing import Any

import orjson

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT))

from cc_transcript import _parser_rs
from cc_transcript.parser import TranscriptParser
from tests import viewgolden
from tests.support import fixture_bytes, raw_envelope

EDGE_DIR = REPO_ROOT / "tests" / "testdata" / "views_edge"
GOLDEN_PATH = REPO_ROOT / "tests" / "testdata" / "views_golden.json"
CORPUS_DIR = REPO_ROOT / ".fixtures" / "corpus"

INTERRUPT_MARKER = "[Request interrupted by user]"

DUP_KEY_LINE = (
    b'{"type":"assistant","uuid":"trap-dup1","parentUuid":null,"sessionId":"sess-1",'
    b'"timestamp":"2026-01-03T06:05:04.000Z","cwd":"/repo","gitBranch":"main","version":"1.2.3",'
    b'"isSidechain":false,"entrypoint":"cli","message":{"role":"assistant","model":"claude-opus-4-8",'
    b'"stop_reason":null,"content":[{"type":"tool_use","id":"tc_dup1","name":"Edit",'
    b'"input":{"file_path":"/first.py","old_string":"o","new_string":"n","file_path":"/last.py"}}]}}'
)

HUGE_INT_LINE = (
    b'{"type":"assistant","uuid":"trap-huge1","parentUuid":null,"sessionId":"sess-1",'
    b'"timestamp":"2026-01-03T06:05:06.000Z","cwd":"/repo","gitBranch":"main","version":"1.2.3",'
    b'"isSidechain":false,"entrypoint":"cli","message":{"role":"assistant","model":"claude-opus-4-8",'
    b'"stop_reason":null,"content":[{"type":"tool_use","id":"tc_huge1","name":"NumTool",'
    b'"input":{"huge":12345678901234567890123456789,"negzero_raw":-0}}]}}'
)


def tool_use(id_: str, name: str, input_: Any) -> dict[str, Any]:
    return {"type": "tool_use", "id": id_, "name": name, "input": input_}


def edge_assistant(uuid: str, secs: int, blocks: list[dict[str, Any]]) -> dict[str, Any]:
    return raw_envelope(
        uuid=uuid,
        timestamp=f"2026-01-03T04:05:{secs:02d}.000Z",
        type="assistant",
        message={"role": "assistant", "model": "claude-opus-4-8", "stop_reason": "tool_use", "content": blocks},
    )


def edge_result(uuid: str, secs: int, tool_use_id: str, payload: Any, *, content: Any = "ok") -> dict[str, Any]:
    return raw_envelope(
        uuid=uuid,
        timestamp=f"2026-01-03T05:05:{secs:02d}.000Z",
        type="user",
        toolUseResult=payload,
        message={
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": False}],
        },
    )


def edge_tools_entries() -> list[dict[str, Any]]:
    calls = [
        edge_assistant(
            "edge-a1",
            1,
            [
                tool_use(
                    "tc_bash1",
                    "Bash",
                    {"command": "echo hi && ls | wc -l", "timeout": 5000, "description": "count", "run_in_background": False},
                ),
                tool_use("tc_bash2", "Execute", {"command": "true"}),
                tool_use("tc_bash3", "Bash", {"command": "x", "timeout": "9"}),
                tool_use("tc_bash4", "Bash", {}),
                tool_use("tc_bash5", "Bash", {"command": 42}),
            ],
        ),
        edge_assistant(
            "edge-a2",
            2,
            [
                tool_use(
                    "tc_edit1",
                    "Edit",
                    {"file_path": "/a.py", "old_string": "x = 1", "new_string": "x = 2", "replace_all": True},
                ),
                tool_use("tc_edit2", "Edit", {"file_path": "/b.py", "old_string": "o", "new_string": "n"}),
                tool_use("tc_edit3", "Edit", {"file_path": "/c.py", "old_string": "o", "new_string": "n", "replace_all": "yes"}),
                tool_use("tc_edit4", "Edit", {"file_path": "/d.py", "old_string": "only-old"}),
                tool_use(
                    "tc_medit1",
                    "MultiEdit",
                    {
                        "file_path": "/m.py",
                        "edits": [
                            {"old_string": "a", "new_string": "b"},
                            {"old_string": "c", "new_string": "d", "replace_all": True},
                        ],
                    },
                ),
                tool_use("tc_medit2", "MultiEdit", {"file_path": "/m2.py", "edits": [{"old_string": "a"}]}),
                tool_use("tc_medit3", "MultiEdit", {"file_path": "/m3.py", "edits": 5}),
                tool_use("tc_write1", "Write", {"file_path": "/w.py", "content": "print('hi')\n"}),
                tool_use("tc_write2", "Create", {"file_path": "/w2.py", "content": ""}),
                tool_use("tc_write3", "Write", {"file_path": "/w3.py"}),
            ],
        ),
        edge_assistant(
            "edge-a3",
            3,
            [
                tool_use("tc_read1", "Read", {"file_path": "/r.py", "offset": 10, "limit": 50}),
                tool_use("tc_read2", "Read", {"file_path": "/r2.py"}),
                tool_use(
                    "tc_nb1",
                    "NotebookEdit",
                    {"notebook_path": "/n.ipynb", "new_source": "x=1", "cell_id": "c1", "edit_mode": "replace"},
                ),
                tool_use("tc_nb2", "NotebookEdit", {"notebook_path": "/n2.ipynb", "new_source": ""}),
                tool_use(
                    "tc_grep1",
                    "Grep",
                    {"pattern": "def .*", "path": "/src", "glob": "*.py", "type": "py", "output_mode": "content"},
                ),
                tool_use("tc_grep2", "Grep", {"pattern": "x"}),
                tool_use("tc_glob1", "Glob", {"pattern": "**/*.rs", "path": "/r"}),
            ],
        ),
        edge_assistant(
            "edge-a4",
            4,
            [
                tool_use(
                    "tc_task1",
                    "Agent",
                    {"prompt": "explore", "subagent_type": "Explore", "model": "sonnet", "name": "scout", "run_in_background": True},
                ),
                tool_use("tc_task2", "Task", {"prompt": "launch", "agent_type": "general"}),
                tool_use("tc_task3", "Agent", {"prompt": "degrade"}),
                tool_use(
                    "tc_wf1",
                    "Workflow",
                    {"script": "return 1", "args": {"k": [1, 2]}, "name": "wf", "resumeFromRunId": "r1"},
                ),
                tool_use("tc_wf2", "Workflow", {"scriptPath": "/w.py"}),
                tool_use("tc_wf3", "Workflow", {"script_path": "/w2.py"}),
                tool_use("tc_skill1", "Skill", {"skill": "codex", "args": "--review"}),
                tool_use("tc_skill2", "Skill", {}),
                tool_use("tc_tcr1", "TaskCreate", {"subject": "s", "description": "d"}),
                tool_use("tc_tup1", "TaskUpdate", {"taskId": "7", "status": "completed", "subject": "s2"}),
                tool_use("tc_tup2", "TaskUpdate", {"task_id": "8"}),
                tool_use("tc_epm1", "ExitPlanMode", {"plan": "the plan"}),
                tool_use("tc_epm2", "ExitSpecMode", {"plan": "the spec"}),
            ],
        ),
        edge_assistant(
            "edge-a5",
            5,
            [
                tool_use("tc_other1", "FancyTool", {"anything": [1, {"a": None}], "n": 3}),
                tool_use("tc_other2", "mcp__semble__search", {"query": "q"}),
                tool_use("tc_other3", "TodoWrite", {"todos": []}),
                tool_use(
                    "tc_ask1",
                    "AskUserQuestion",
                    {
                        "questions": [
                            {
                                "question": "Q1?",
                                "header": "H",
                                "multiSelect": True,
                                "options": [{"label": "A"}, {"label": 2}, "junk", {"label": "B"}],
                            },
                            {"question": "Q2?", "options": []},
                            {"noquestion": 1},
                            "bare",
                        ]
                    },
                ),
                tool_use("tc_ask2", "AskUserQuestion", {"questions": {"not": "a list"}}),
                tool_use("tc_arr1", "Bash", [1, 2]),
            ],
        ),
    ]
    results = [
        edge_result(
            "edge-r1",
            1,
            "tc_bash1",
            {"stdout": "s", "stderr": "", "backgroundTaskId": "bg1", "interrupted": True, "noOutputExpected": True},
        ),
        edge_result(
            "edge-r2",
            2,
            "tc_write1",
            {
                "content": "print('hi')\n",
                "filePath": "/w.py",
                "originalFile": "old\n",
                "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-old", "+new"]}],
                "userModified": True,
            },
        ),
        edge_result(
            "edge-r3",
            3,
            "tc_read1",
            {"file": {"filePath": "/r.py", "content": "data", "numLines": 1}, "type": "text"},
        ),
        edge_result(
            "edge-r4",
            4,
            "tc_task1",
            {
                "agentId": "ag1",
                "agentType": "Explore",
                "status": "completed",
                "totalDurationMs": 1234,
                "totalTokens": 99,
                "totalToolUseCount": 3,
                "toolStats": {"Bash": {"count": 2}},
                "usage": {"input_tokens": 5},
                "content": [{"type": "text", "text": "done"}],
                "prompt": "explore",
                "resolvedModel": "sonnet",
            },
        ),
        edge_result(
            "edge-r5",
            5,
            "tc_task2",
            {
                "agentId": "ag2",
                "outputFile": "/tmp/o",
                "isAsync": True,
                "canReadOutputFile": True,
                "description": "d",
                "prompt": "launch",
                "status": "running",
                "resolvedModel": "opus",
            },
        ),
        edge_result("edge-r6", 6, "tc_task3", {"weird": 1}),
        edge_result("edge-r7", 7, "tc_skill1", {"commandName": "codex", "success": True, "allowedTools": ["Bash", "Read"]}),
        edge_result("edge-r8", 8, "tc_skill2", {"commandName": "x", "allowedTools": "nope"}),
        edge_result(
            "edge-r9",
            9,
            "tc_ask1",
            {
                "questions": [{"question": "Q1?", "header": "H", "multiSelect": False, "options": [{"label": "A"}]}],
                "answers": {"Q1?": "A", "Q2?": 5},
                "annotations": {"Q1?": {"preview": "p", "notes": 7}, "Q2?": "notdict"},
            },
        ),
        edge_result("edge-r10", 10, "toolu_orphan", {"a": 1}),
        raw_envelope(
            uuid="edge-r11",
            timestamp="2026-01-03T05:06:00.000Z",
            type="user",
            message={
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "tc_read2", "content": "later", "is_error": False},
                    {"type": "text", "text": "interleaved"},
                    {"type": "tool_result", "tool_use_id": "tc_grep2", "content": [{"type": "text", "text": "hit"}], "is_error": True},
                    {"type": "text", "text": "tail"},
                ],
            },
        ),
    ]
    return calls + results


def edge_traps_entries() -> list[dict[str, Any]]:
    return [
        edge_assistant(
            "trap-a1",
            1,
            [
                tool_use(
                    "tc_num1",
                    "NumTool",
                    {
                        "big": 9223372036854775807,
                        "u64max": 18446744073709551615,
                        "tiny": 1e-07,
                        "tie": 698957826421429.2,
                        "big_exp": 2.5e300,
                        "negzerof": -0.0,
                        "ratio": 1.5,
                    },
                ),
                {"type": "text", "text": "café \U0001f916 x́ \x1c\x1d done"},
            ],
        ),
        raw_envelope(
            uuid="trap-u1",
            timestamp="2026-01-03T06:05:01.000Z",
            type="user",
            message={"role": "user", "content": f"\x1c{INTERRUPT_MARKER} via C0 whitespace"},
        ),
        raw_envelope(
            uuid="trap-u2",
            timestamp="2026-01-03T06:05:02.000Z",
            interruptedMessageId="msg_abc123",
            message={"role": "user", "content": "plain interruption by id"},
            type="user",
        ),
        raw_envelope(
            uuid="trap-u3",
            timestamp="2026-01-03T06:05:03.000Z",
            type="user",
            message={"role": "user", "content": '<teammate-message teammate_id="lead">do the thing</teammate-message>'},
        ),
        raw_envelope(
            uuid="trap-u4",
            timestamp="0000-01-01T00:00:00.000Z",
            type="user",
            message={"role": "user", "content": "year zero drops at materialization"},
        ),
        raw_envelope(
            uuid="trap-u5",
            timestamp="2026-01-03T06:05:05.000Z",
            type="user",
            message={"role": "user", "content": "survivor after the year-zero drop"},
        ),
    ]


PRINT_MAXIMAL: list[dict[str, Any]] = [
    {
        "type": "system",
        "subtype": "init",
        "session_id": "sess-p",
        "tools": ["Bash", "Read"],
        "skills": ["codex"],
        "mcp_servers": [{"name": "semble", "status": "connected"}],
        "plugins": [{"name": "cc-notes", "path": "/p", "source": "marketplace"}],
    },
    {"type": "user", "session_id": "sess-p", "uuid": "pm-1", "message": {"role": "user", "content": "hello"}},
    {
        "type": "user",
        "session_id": "sess-p",
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": "t"},
                {"type": "tool_result", "tool_use_id": "toolu_p1", "content": "ok", "is_error": False},
            ],
        },
    },
    {
        "type": "assistant",
        "session_id": "sess-p",
        "uuid": "pm-2",
        "message": {
            "model": "claude-opus-4-8",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_p1", "name": "Bash", "input": {"command": "ls"}},
            ],
        },
    },
    {"type": "assistant", "session_id": "sess-p", "message": {"content": [{"type": "thinking", "thinking": "think"}]}},
    {
        "type": "result",
        "total_cost_usd": 0.1234,
        "num_turns": 3,
        "is_error": False,
        "result": "final text",
        "session_id": "sess-p",
        "fast_mode_state": "enabled",
        "stop_reason": "end_turn",
        "permission_denials": [{"tool_name": "Bash", "tool_input": {"command": "rm"}}],
        "structured_output": {"k": [1, 2]},
        "modelUsage": {
            "claude-opus-4-8": {
                "inputTokens": 10,
                "outputTokens": 2,
                "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 1,
                "webSearchRequests": 0,
                "costUSD": 0.12,
                "contextWindow": 200000,
                "maxOutputTokens": 32000,
            },
            "claude-haiku-4-5": {
                "inputTokens": 1,
                "outputTokens": 1,
                "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0,
                "webSearchRequests": 2,
                "costUSD": 0.001,
                "contextWindow": 200000,
                "maxOutputTokens": 8192,
            },
        },
        "usage": {
            "input_tokens": 11,
            "output_tokens": 3,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 1,
            "cache_creation": {"ephemeral_5m_input_tokens": 1, "ephemeral_1h_input_tokens": 0},
            "service_tier": "standard",
            "inference_geo": "us",
            "server_tool_use": {"web_search_requests": 2, "web_fetch_requests": 0},
        },
    },
]

PRINT_MINIMAL: list[dict[str, Any]] = [
    {
        "type": "result",
        "total_cost_usd": 0.0,
        "num_turns": 0,
        "is_error": True,
        "session_id": "sess-q",
        "permission_denials": [],
        "modelUsage": {},
        "usage": {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
    }
]


def jsonl(entries: list[dict[str, Any]], *extra_lines: bytes) -> bytes:
    return b"".join([orjson.dumps(e) + b"\n" for e in entries] + [line + b"\n" for line in extra_lines])


def write_edge_fixtures() -> list[Path]:
    EDGE_DIR.mkdir(parents=True, exist_ok=True)
    (EDGE_DIR / "edge_core.jsonl").write_bytes(fixture_bytes())
    (EDGE_DIR / "edge_tools.jsonl").write_bytes(jsonl(edge_tools_entries()))
    (EDGE_DIR / "edge_traps.jsonl").write_bytes(jsonl(edge_traps_entries(), DUP_KEY_LINE, HUGE_INT_LINE))
    (EDGE_DIR / "print_maximal.json").write_bytes(orjson.dumps(PRINT_MAXIMAL))
    (EDGE_DIR / "print_minimal.json").write_bytes(orjson.dumps(PRINT_MINIMAL))
    return [EDGE_DIR / name for name in ("edge_core.jsonl", "edge_tools.jsonl", "edge_traps.jsonl")]


def build_golden() -> dict[str, Any]:
    edge_files = write_edge_fixtures()
    corpus_files = sorted(CORPUS_DIR.rglob("*.jsonl"))
    if not corpus_files:
        raise SystemExit("no corpus under .fixtures/corpus — run scripts/gen_corpus.py first")
    files = []
    for path, mode in [(p, "full") for p in edge_files] + [(p, "sampled") for p in corpus_files]:
        events = TranscriptParser.parse_file(path)
        if not events:
            raise SystemExit(f"{path} parsed to zero events — a whole-file parse failure in a fixture")
        files.append(
            {"file": str(path.relative_to(REPO_ROOT)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            | viewgolden.file_record(events, mode=mode)
        )
    print_results = []
    for path in [EDGE_DIR / "print_maximal.json", EDGE_DIR / "print_minimal.json", REPO_ROOT / "tests" / "testdata" / "haiku_envelope.json"]:
        raw = path.read_bytes()
        print_results.append(
            {
                "file": str(path.relative_to(REPO_ROOT)),
                "sha256": hashlib.sha256(raw).hexdigest(),
                "proj": viewgolden.project(_parser_rs.parse_print_result(raw), reprs=True),
            }
        )
    return {
        "version": viewgolden.GOLDEN_VERSION,
        "str_cap": viewgolden.STR_CAP,
        "sample_cap": viewgolden.SAMPLE_CAP,
        "files": files,
        "print_results": print_results,
    }


def main() -> None:
    golden = build_golden()
    GOLDEN_PATH.write_text(json.dumps(golden, indent=1, sort_keys=False) + "\n")
    total = sum(len(f["events"]) for f in golden["files"])
    print(f"wrote {GOLDEN_PATH.relative_to(REPO_ROOT)}: {len(golden['files'])} files, {total} events, {GOLDEN_PATH.stat().st_size:,} bytes")


if __name__ == "__main__":
    main()
