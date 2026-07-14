"""Freeze the Python query.py ``Session`` battery into ``tests/testdata/query_golden.json``.

Projects a deterministic battery over :class:`~cc_transcript.query.Session` — tool-call
counts, touched/edited files, failures, commands, prompts, ``has_*`` predicates, window
event-lengths, and per-:class:`~cc_transcript.query.FileRef` ``is_test``/``suffix`` — over the
first :data:`MAX_EVENTS` parsed events of every bench-corpus file (``.fixtures/corpus``) plus a
battery of hand-built synthetic transcripts pinning the branches the corpus never reaches. Events
are sourced through the Rust parser on both this generator and the parity test, so only the
*query* is under test, not the parser.

``tests/test_query_parity.py`` asserts the Rust ``query_session`` port reproduces the same
projection and that the Python reference still projects to the frozen golden.

Run: ``uv run --no-sync python scripts/gen_query_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _parser_rs
from cc_transcript.activity import SessionActivity
from cc_transcript.ids import SessionId
from cc_transcript.query import Session
from scripts.gen_corpus import DEFAULT_OUT as CORPUS
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from cc_transcript.models import TranscriptEvent

GOLDEN = REPO_ROOT / "tests" / "testdata" / "query_golden.json"

# Matches gen_activity_golden.MAX_EVENTS: the first MAX_EVENTS parsed events query in full.
MAX_EVENTS = 150


def env(kind: str, uuid: str, ts: str, **extra: object) -> dict[str, object]:
    return {"type": kind, "uuid": uuid, "sessionId": "syn", "timestamp": ts} | extra


def usr(uuid: str, ts: str, content: object, **flags: object) -> str:
    return json.dumps(env("user", uuid, ts, **flags) | {"message": {"role": "user", "content": content}})


def asst(uuid: str, ts: str, *blocks: dict[str, object]) -> str:
    return json.dumps(env("assistant", uuid, ts) | {"message": {"model": "m", "content": list(blocks)}})


def txt(text: str) -> dict[str, object]:
    return {"type": "text", "text": text}


def use(tid: str, name: str, **inp: object) -> dict[str, object]:
    return {"type": "tool_use", "id": tid, "name": name, "input": inp}


def res(uuid: str, ts: str, tid: str, *, content: str = "ok", is_error: bool = False) -> str:
    block = {"type": "tool_result", "tool_use_id": tid, "content": content, "is_error": is_error}
    return json.dumps(env("user", uuid, ts) | {"message": {"role": "user", "content": [block]}})


def jsonl(*lines: str) -> str:
    return "".join(f"{line}\n" for line in lines)


def whole(sec: int) -> str:
    return f"2026-01-01T00:00:{sec:02d}.000Z"


# Raw dup-key edit input (json.dumps can't emit duplicate keys); last wins per key.
DUP_KEY_ASSISTANT = (
    '{"type":"assistant","uuid":"a0","sessionId":"syn","timestamp":"2026-01-01T00:00:01.000Z",'
    '"message":{"model":"m","content":['
    '{"type":"tool_use","id":"e1","name":"Edit","input":'
    '{"file_path":"/first.py","file_path":"/last.py","old_string":"a","new_string":"x"}},'
    '{"type":"tool_use","id":"b1","name":"Bash","input":'
    '{"command":"echo a","command":"echo b"}}'
    "]}}"
)

SYNTHETIC_CASES: dict[str, str] = {
    "empty": "",
    "windows_demo": jsonl(
        usr("u0", whole(0), "do the thing"),
        asst("a0", whole(1), use("t1", "Read", file_path="/src/app.py")),
        asst("a1", whole(2), use("t2", "Bash", command="ls")),
        res("r2", whole(3), "t2"),
        usr("u1", whole(4), "now write"),
        asst("a2", whole(5), use("t3", "Write", file_path="/out.py", content="hello")),
        res("r3", whole(6), "t3"),
        asst("a3", whole(7), use("t4", "Bash", command="git push -f")),
        res("r4", whole(8), "t4"),
        usr("u2", whole(9), "and edit"),
        asst("a4", whole(10), use("t5", "Edit", file_path="/out.py", old_string="x", new_string="y")),
        asst("a5", whole(11), use("t6", "Read", file_path="/repo/tests/test_out.py")),
    ),
    "task_skill": jsonl(
        usr("u0", whole(0), "delegate the work"),
        asst(
            "a0",
            whole(1),
            use("t1", "Task", agent_type="explorer", prompt="look"),
            use("t2", "Skill", skill="commit"),
            use("t3", "Skill", skill="other"),
        ),
    ),
    "override_invalidated": jsonl(
        usr("u0", whole(0), "OVERRIDE the setting"),
        asst("a0", whole(1), use("t1", "Edit", file_path="/a.py", old_string="x", new_string="y")),
    ),
    "override_active": jsonl(
        usr("u0", whole(0), "OVERRIDE the setting"),
        asst("a0", whole(1), use("t1", "Bash", command="ls")),
    ),
    "user_said_case": jsonl(
        usr("u0", whole(0), "Please FIX the Error now"),
        asst("a0", whole(1), txt("on it")),
    ),
    "assistant_trunc": jsonl(
        usr("u0", whole(0), "hi"),
        asst("a0", whole(1), txt("\x1f\x1f" + "α" * 100 + "\x1f")),
        asst("a1", whole(2), txt("   ")),
        asst("a2", whole(3), txt("short answer")),
    ),
    "failed_calls": jsonl(
        usr("u0", whole(0), "run"),
        asst("a0", whole(1), use("t1", "Bash", command="ok cmd")),
        res("r1", whole(2), "t1"),
        asst("a1", whole(3), use("t2", "Bash", command="bad cmd")),
        res("r2", whole(4), "t2", content="boom", is_error=True),
        asst("a2", whole(5), use("t3", "Read", file_path="/x.py")),
        res("r3", whole(6), "t3", content="boom", is_error=True),
    ),
    "dup_key_input": jsonl(usr("u0", whole(0), "edit and run"), DUP_KEY_ASSISTANT),
    "mcp_edit": jsonl(
        usr("u0", whole(0), "use the mcp editor"),
        asst("a0", whole(1), use("t1", "mcp__server__ccx_code_edit", file="/z.py", content="q")),
    ),
    "fileref_variety": jsonl(
        usr("u0", whole(0), "touch files"),
        asst(
            "a0",
            whole(1),
            use("t1", "Read", file_path="/repo/tests/test_app.py"),
            use("t2", "Read", file_path="/repo/conftest.py"),
            use("t3", "Read", file_path="/repo/src/main.rs"),
            use("t4", "Read", file_path="/repo/.bashrc"),
            use("t5", "Read", file_path="/repo/a.tar.gz"),
            use("t6", "Read", file_path="/repo/data/α.β"),
            use("t7", "Read", file_path="/repo/tests/helpers/util.py"),
        ),
    ),
}


def project_session(session: Session) -> dict[str, object]:
    tc = session.tool_calls
    return {
        "tool_calls": tc.count(),
        "tool_calls_with_errors": tc.with_errors.count(),
        "files_touched": [str(f) for f in session.files_touched],
        "edited_files": [str(f) for f in session.edited_files],
        "count_failures": session.count_failures(),
        "commands": list(session.commands()),
        "first_prompt": session.first_prompt,
        "user_text": session.user_text,
        "len": len(session),
        "bool": bool(session),
        "has_tool": {name: session.has_tool(name, subagents=False) for name in ("Bash", "Edit|Write", "Read", "Task", "Skill")},
        "has_command": [session.has_command(*argv, subagents=False) for argv in (["git", "push"], ["ls"])],
        "has_edit_to": session.has_edit_to("*.py", subagents=False),
        "has_read": session.has_read("test", subagents=False),
        "has_skill": session.has_skill("commit", "codex", subagents=False),
        "user_said": session.user_said("fix", "error"),
        "assistant_text": session.assistant_text(3, 80),
        "has_override": session.has_override("OVERRIDE", invalidated_by=("Edit", "Write")),
        "windows": {
            "after_write": len(session.after(tool="Write")),
            "before_bash": len(session.before(tool="Bash")),
            "prior": len(session.prior()),
            "recent5": len(session.recent(5)),
            "current_turn": len(session.current_turn),
        },
        "tool_calls_detail": {
            "named_bash": tc.named("Bash").count(),
            "touching_py": tc.touching("*.py").count(),
            "under_src": tc.under("src", "cc_transcript").count(),
            "in_turn0": tc.in_turns(0).count(),
            "first_name": first.call.name if (first := tc.first()) else None,
            "last_name": last.call.name if (last := tc.last()) else None,
            "files": [str(f) for f in tc.files()],
        },
        "file_refs": [{"path": f.path, "is_test": f.is_test, "suffix": f.suffix} for f in session.files_touched],
    }


def events_of(path: Path) -> list[TranscriptEvent]:
    parsed = _parser_rs.stream_parse([(str(path), 1.0)], 1).recv()
    return [] if parsed is None else list(parsed.events)


def project_file(path: Path) -> dict[str, object]:
    events = events_of(path)[:MAX_EVENTS]
    activity = SessionActivity.from_events(SessionId("golden"), events)
    return project_session(Session.from_activity(activity, path=None))


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
        "queries": {str(path.relative_to(CORPUS)): project_file(path) for path in corpus_files()},
        "synthetic": {name: project_jsonl(text) for name, text in SYNTHETIC_CASES.items()},
    }
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data['queries'])} query + {len(SYNTHETIC_CASES)} synthetic projections to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
