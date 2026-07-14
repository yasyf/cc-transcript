"""Freeze the Python facts.py analytics into ``tests/testdata/facts_golden.json``.

Projects :func:`cc_transcript.facts.tool_facts` plus the aggregators
(:func:`~cc_transcript.facts.command_prefix_counts`, :func:`~cc_transcript.facts.mcp_summary`)
over the first :data:`MAX_EVENTS` parsed events of every bench-corpus file (``.fixtures/corpus``)
plus a battery of hand-built synthetic transcripts pinning the branches the corpus never reaches
(command-prefix ordering, MCP server/tool ranking, user-rejection denials, sub-millisecond
durations, file-path calls, dup-key last-wins). Events are sourced through the Rust parser on both
this generator and the parity test, so only the *facts* projection is under test, not the parser.

``tests/test_facts_parity.py`` asserts the Rust ``tool_facts`` port reproduces the same projection
and that the Python reference still projects to the frozen golden. The aggregate lists are ordered,
so Counter.most_common tie-ordering and the server sort are under test, not just membership.

Run: ``uv run --no-sync python scripts/gen_facts_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript import facts as cc_facts
from cc_transcript.backend import ParsedTranscript
from cc_transcript.filterspec import DENIAL_PREFIX, USER_SAID_MARKER, USER_SAID_TRAILER
from scripts.gen_corpus import DEFAULT_OUT as CORPUS
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from datetime import datetime

    from cc_transcript.facts import ToolFact
    from cc_transcript.models import TranscriptEvent

GOLDEN = REPO_ROOT / "tests" / "testdata" / "facts_golden.json"

# Matches gen_activity_golden.MAX_EVENTS: the first MAX_EVENTS parsed events project in full.
MAX_EVENTS = 150


def env(kind: str, uuid: str, ts: str, **extra: object) -> dict[str, object]:
    return {"type": kind, "uuid": uuid, "sessionId": "syn", "timestamp": ts} | extra


def usr(uuid: str, ts: str, content: object, **flags: object) -> str:
    return json.dumps(env("user", uuid, ts, **flags) | {"message": {"role": "user", "content": content}})


def asst(uuid: str, ts: str, *blocks: dict[str, object]) -> str:
    return json.dumps(env("assistant", uuid, ts) | {"message": {"model": "m", "content": list(blocks)}})


def use(tid: str, name: str, **inp: object) -> dict[str, object]:
    return {"type": "tool_use", "id": tid, "name": name, "input": inp}


def res(uuid: str, ts: str, tid: str, *, content: str = "ok", is_error: bool = False) -> str:
    block = {"type": "tool_result", "tool_use_id": tid, "content": content, "is_error": is_error}
    return json.dumps(env("user", uuid, ts) | {"message": {"role": "user", "content": [block]}})


def denial_res(uuid: str, ts: str, tid: str, said: str) -> str:
    content = f"{DENIAL_PREFIX}.\n{USER_SAID_MARKER}{said}\n{USER_SAID_TRAILER} will guide you."
    return res(uuid, ts, tid, content=content, is_error=True)


def jsonl(*lines: str) -> str:
    return "".join(f"{line}\n" for line in lines)


def whole(sec: int) -> str:
    return f"2026-01-01T00:00:{sec:02d}.000Z"


# Raw dup-key command input (json.dumps can't emit duplicate keys); last wins.
DUP_KEY_ASSISTANT = (
    '{"type":"assistant","uuid":"a0","sessionId":"syn","timestamp":"2026-01-01T00:00:01.000Z",'
    '"message":{"model":"m","content":['
    '{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"git status","command":"npm run build"}}'
    "]}}"
)

SYNTHETIC_CASES: dict[str, str] = {
    "empty": "",
    "prefixes_and_mcp": jsonl(
        usr("u0", whole(0), "do a lot"),
        asst(
            "a0",
            whole(1),
            use("t1", "Bash", command="git push"),
            use("t2", "Bash", command="git commit -m x"),
            use("t3", "Bash", command="git push"),
            use("t4", "Bash", command="ls -la"),
            use("t5", "mcp__semble__search", query="a"),
            use("t6", "mcp__semble__search", query="b"),
            use("t7", "mcp__semble__find_related", ref="c"),
            use("t8", "mcp__cc_notes__note_add", body="d"),
            use("t9", "mcp__cc_notes__note_show", id="e"),
        ),
    ),
    "denial_and_error": jsonl(
        usr("u0", whole(0), "risky"),
        asst("a0", whole(1), use("t1", "Bash", command="rm -rf /tmp/x")),
        denial_res("r1", whole(2), "t1", "please stop that"),
        asst("a1", whole(3), use("t2", "Read", file_path="/x.py")),
        res("r2", whole(4), "t2", content="boom", is_error=True),
    ),
    "durations": jsonl(
        usr("u0", "2026-01-01T00:00:00.000000Z", "time it"),
        asst("a0", "2026-01-01T00:00:01.000000Z", use("t1", "Bash", command="a")),
        res("r1", "2026-01-01T00:00:01.000600Z", "t1"),
        asst("a1", "2026-01-01T00:00:02.000000Z", use("t2", "Bash", command="b")),
        res("r2", "2026-01-01T00:00:02.001500Z", "t2"),
    ),
    "file_calls": jsonl(
        usr("u0", whole(0), "touch files"),
        asst(
            "a0",
            whole(1),
            use("t1", "Edit", file_path="/a.py", old_string="x", new_string="y"),
            use("t2", "Write", file_path="/b.py", content="hello"),
            use("t3", "Read", file_path="/c.py"),
            use("t4", "NotebookEdit", notebook_path="/d.ipynb", new_source="cell"),
        ),
    ),
    "dup_key_command": jsonl(usr("u0", whole(0), "run"), DUP_KEY_ASSISTANT),
}


def ms(stamp: datetime) -> int:
    return round(stamp.timestamp() * 1000)


def fact_dict(f: ToolFact) -> dict[str, object]:
    return {
        "session_id": f.session_id,
        "tool_use_id": f.tool_use_id,
        "tool": f.tool,
        "command_prefixes": list(f.command_prefixes),
        "command": f.command,
        "mcp_server": f.mcp_server,
        "mcp_tool": f.mcp_tool,
        "mcp_access": f.mcp_access,
        "file_path": f.file_path,
        "is_error": f.is_error,
        "denied": f.denied,
        "denial_kind": f.denial_kind,
        "user_said": f.user_said,
        "duration_ms": f.duration_ms,
        "ts_ms": ms(f.ts),
    }


def pairs_list(pairs: dict[str, int], key: str) -> list[dict[str, object]]:
    return [{key: name, "count": count} for name, count in pairs.items()]


def mcp_list(summary: dict[str, dict[str, object]]) -> list[dict[str, object]]:
    return [
        {
            "server": server,
            "read": rollup["read"],
            "write": rollup["write"],
            "total": rollup["total"],
            "tools": pairs_list(rollup["tools"], "tool"),
        }
        for server, rollup in summary.items()
    ]


def events_of(path: Path) -> list[TranscriptEvent]:
    parsed = _native.stream_parse([(str(path), 1.0)], 1).recv()
    return [] if parsed is None else list(parsed.events)


def project_file(path: Path) -> dict[str, object]:
    events = events_of(path)[:MAX_EVENTS]
    facts = list(cc_facts.tool_facts([ParsedTranscript(path=path, mtime=1.0, events=tuple(events))]))
    return {
        "facts": [fact_dict(f) for f in facts],
        "command_prefix_counts": pairs_list(cc_facts.command_prefix_counts(facts), "prefix"),
        "mcp_summary": mcp_list(cc_facts.mcp_summary(facts)),
    }


def project_jsonl(text: str) -> dict[str, object]:
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8") as handle:
        handle.write(text)
        path = Path(handle.name)
    try:
        return project_file(path)
    finally:
        path.unlink()


def corpus_files() -> list[Path]:
    return sorted(CORPUS.rglob("*.jsonl"))


def main() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)
    data = {
        "max_events": MAX_EVENTS,
        "files": {str(path.relative_to(CORPUS)): project_file(path) for path in corpus_files()},
        "synthetic": {name: project_jsonl(text) for name, text in SYNTHETIC_CASES.items()},
    }
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data['files'])} file + {len(SYNTHETIC_CASES)} synthetic projections to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
