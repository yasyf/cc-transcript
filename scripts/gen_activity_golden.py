"""Freeze the Python ``SessionActivity`` lift into ``tests/testdata/activity_golden.json``.

Projects ``SessionActivity.from_events`` over the first :data:`MAX_EVENTS` parsed events of
every file in the deterministic bench corpus (``.fixtures/corpus``, regenerated via
``scripts/gen_corpus.py``). Events are sourced through the Rust parser (``stream_parse``) on
both this generator and the parity test, so only the *lift* — turn boundaries, tool-use
records, edit records, result indexing, and same-file hunk overlaps — is under test, not the
parser. Closing batteries of hand-built ``hunk_overlap`` and ``overlap_between`` cases pin the
normalization (whitespace collapse, blank-line drop, CRLF, unicode) and composition the corpus
edits never exercise.

A later run plus ``git diff`` shows Python-side drift, and ``tests/test_activity_parity.py``
asserts the Rust ``activity_lift`` / ``activity_hunk_overlap`` ports reproduce the same
structure. Every timestamp projects to an epoch-millisecond int, lossless for the corpus's
millisecond-precision stamps and identical across Python ``datetime`` and Rust ``chrono``.

Run: ``uv run --no-sync python scripts/gen_activity_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from cc_transcript import _native
from cc_transcript.activity import SessionActivity, hunk_overlap, result_index
from cc_transcript.ids import SessionId
from cc_transcript.tools import Hunk
from scripts.gen_corpus import DEFAULT_OUT as CORPUS
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from cc_transcript.activity import Edit, ToolUse, Turn
    from cc_transcript.models import TranscriptEvent

GOLDEN = REPO_ROOT / "tests" / "testdata" / "activity_golden.json"

# The window every file is capped to: the first MAX_EVENTS parsed events lift in full, so
# turns / tool uses / edits / result index / overlaps stay mutually consistent and bounded.
MAX_EVENTS = 150

# (a_old, a_new, b_old, b_new): hand-built Hunk pairs beyond the corpus, chosen to exercise
# every branch of the overlap normalization the corpus's constant edits never reach.
HUNK_CASES: tuple[tuple[str, str, str, str], ...] = (
    ("", "x = 1\ny = 2", "x = 1\ny = 2", ""),
    ("", "x = 1\nz = 3", "x = 1\ny = 2", ""),
    ("", "  x  =  1  ", "x = 1", ""),
    ("", "x = 1\n\n   \n", "x = 1", ""),
    ("", "", "x = 1", ""),
    ("", "a = 1", "b = 2", ""),
    ("", "keep\r\ndrop", "keep", ""),
    ("", "α = 1\nβ = 2", "α = 1", ""),
    ("return None", "return result", "return None", ""),
    ("", "a\x1fb", "a b", ""),
    ("", "a\x0bb", "a", ""),
)

OVERLAP_BETWEEN_CASES: tuple[tuple[tuple[Hunk, ...], tuple[Hunk, ...]], ...] = (
    ((), (Hunk("unused", ""),)),
    ((Hunk("", "unused"),), ()),
    ((Hunk("", "x = 1\ny = 2"),), (Hunk("x = 1", ""),)),
    (
        (Hunk("", "first"), Hunk("", "target\nextra")),
        (Hunk("miss", ""), Hunk("target\nextra", "")),
    ),
)


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


def jsonl(*lines: str) -> str:
    return "".join(f"{line}\n" for line in lines)


def whole(sec: int) -> str:
    return f"2026-01-01T00:00:{sec:02d}.000Z"


# Raw dup-key edit inputs (json.dumps can't emit duplicate keys); last wins on both sides.
DUP_KEY_ASSISTANT = (
    '{"type":"assistant","uuid":"a0","sessionId":"syn","timestamp":"2026-01-01T00:00:01.000Z",'
    '"message":{"model":"m","content":['
    '{"type":"tool_use","id":"e1","name":"Edit","input":'
    '{"file_path":"/first.py","file_path":"/last.py","old_string":"a","old_string":"b",'
    '"new_string":"x","new_string":"y"}},'
    '{"type":"tool_use","id":"w1","name":"Write","input":'
    '{"file_path":"/w1.py","file_path":"/w2.py","content":"c1","content":"c2"}},'
    '{"type":"tool_use","id":"m1","name":"MultiEdit","input":'
    '{"file_path":"/m1.py","file_path":"/m2.py","edits":'
    '[{"old_string":"p","new_string":"q","old_string":"pp","new_string":"qq"}]}}'
    "]}}"
)

# Hand-built transcripts pinning branches the corpus never reaches.
SYNTHETIC_CASES: dict[str, str] = {
    "empty": "",
    "prelude_only": jsonl(res("r0", whole(0), "ghost"), usr("um", whole(1), "meta", isMeta=True)),
    "user_flags": jsonl(
        usr("u0", whole(0), "real prompt one"),
        usr("um", whole(1), "meta note", isMeta=True),
        usr("us", whole(2), "sidechain note", isSidechain=True),
        usr("uc", whole(3), "compact recap", isCompactSummary=True),
        usr("ua", whole(4), "<teammate-message from='mate'>ping</teammate-message>"),
        usr("ur", whole(5), 'Another Claude session sent a message: <agent-message from="x">ping</agent-message>'),
        usr("u1", whole(6), "real prompt two"),
    ),
    "eof_interruption": jsonl(
        usr("u0", whole(0), "do it"),
        asst("a0", whole(1), use("t1", "Bash", command="make")),
        usr("i0", whole(2), "[Request interrupted by user]"),
    ),
    "control_ws_prompt": jsonl(
        usr("u0", whole(0), "real"),
        usr("uw", whole(1), "\x1c\x1d\x1e\x1f"),
        usr("u1", whole(2), "next"),
    ),
    "interrupted_message_id": jsonl(
        usr("u0", whole(0), "real prompt"),
        usr("ui", whole(1), "silent stop", interruptedMessageId="m1"),
        usr("u1", whole(2), "next prompt"),
    ),
    "dup_result_index": jsonl(
        usr("u0", whole(0), "go"),
        asst("a0", whole(1), use("t1", "Bash", command="ls")),
        res("r1", whole(2), "t1", content="first", is_error=True),
        res("r2", whole(3), "t1", content="second", is_error=False),
    ),
    "subms_durations": jsonl(
        usr("u0", "2026-01-01T00:00:00.000000Z", "go"),
        asst("a0", "2026-01-01T00:00:01.000000Z", use("t1", "Bash", command="a")),
        res("r1", "2026-01-01T00:00:01.000600Z", "t1"),
        asst("a1", "2026-01-01T00:00:02.000000Z", use("t2", "Bash", command="b")),
        res("r2", "2026-01-01T00:00:02.001500Z", "t2"),
    ),
    "year_zero_drops": jsonl(
        usr("uz", "0000-01-01T00:00:00.000Z", "year zero prompt"),
        usr("u1", "2026-01-01T00:00:00.000Z", "real prompt"),
        asst("a1", "2026-01-01T00:00:01.000Z", use("t1", "Bash", command="ls")),
        res("rz", "0000-01-01T00:00:02.000Z", "t1"),
    ),
    "edit_variants": jsonl(
        usr("u0", whole(0), "edit stuff"),
        asst(
            "a0",
            whole(1),
            use("e1", "Edit", file_path="/a.py", old_string="x", new_string="y"),
            use(
                "m1",
                "MultiEdit",
                file_path="/b.py",
                edits=[{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}],
            ),
            use("w1", "Write", file_path="/c.py", content="hello"),
            use("c1", "Create", file_path="/d.py", content="created"),
            use("n1", "NotebookEdit", notebook_path="/e.ipynb", new_source="cell"),
            use("b1", "Bash", command="ls"),
        ),
    ),
    "dup_key_edits": jsonl(usr("u0", whole(0), "edit"), DUP_KEY_ASSISTANT),
}


def ms(stamp: datetime) -> int:
    return round(stamp.timestamp() * 1000)


def ms_or_none(stamp: datetime | None) -> int | None:
    return None if stamp is None else ms(stamp)


def tool_use_to_dict(use: ToolUse) -> dict[str, object]:
    return {
        "event_uuid": use.ref.event_uuid,
        "tool_use_id": use.ref.tool_use_id,
        "name": use.call.name,
        "ts_ms": ms(use.ts),
        "has_result": use.result is not None,
        "result_is_error": None if use.result is None else use.result.is_error,
        "result_ts_ms": ms_or_none(use.result_ts),
        "duration_ms": use.duration_ms,
    }


def edit_to_dict(edit: Edit) -> dict[str, object]:
    return {
        "file_path": edit.file_path,
        "tool": edit.tool,
        "hunks": [{"old": hunk.old, "new": hunk.new} for hunk in edit.hunks],
        "event_uuid": edit.ref.event_uuid,
        "tool_use_id": edit.ref.tool_use_id,
        "turn_index": edit.turn_index,
        "ts_ms": ms(edit.ts),
    }


def turn_to_dict(turn: Turn) -> dict[str, object]:
    return {
        "index": turn.index,
        "prompt": turn.prompt,
        "started_at_ms": ms_or_none(turn.started_at),
        "ended_at_ms": ms_or_none(turn.ended_at),
        "event_count": len(turn.events),
        "tool_uses": [tool_use_to_dict(use) for use in turn.tool_uses],
        "edits": [edit_to_dict(edit) for edit in turn.edits],
    }


def overlaps_of(activity: SessionActivity) -> list[dict[str, object]]:
    edits = list(activity.edits)
    return [
        {
            "a_tool_use_id": edits[i].ref.tool_use_id,
            "b_tool_use_id": edits[j].ref.tool_use_id,
            "overlap": max(
                (hunk_overlap(a, b) for a in edits[i].hunks for b in edits[j].hunks), default=0.0
            ),
        }
        for i in range(len(edits))
        for j in range(i + 1, len(edits))
        if edits[i].file_path == edits[j].file_path
    ]


def events_of(path: Path) -> list[TranscriptEvent]:
    parsed = _native.stream_parse([(str(path), 1.0)], 1).recv()
    return [] if parsed is None else list(parsed.events)


def project_file(path: Path) -> dict[str, object]:
    events = events_of(path)[:MAX_EVENTS]
    activity = SessionActivity.from_events(SessionId("golden"), events)
    return {
        "turn_count": len(activity.turns),
        "turns": [turn_to_dict(turn) for turn in activity.turns],
        "result_index": [
            {"tool_use_id": tool_use_id, "result_ts_ms": ms_or_none(stamp), "is_error": block.is_error}
            for tool_use_id, (block, stamp) in result_index(events).items()
        ],
        "hunk_overlaps": overlaps_of(activity),
    }


def project_jsonl(text: str) -> dict[str, object]:
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8") as handle:
        handle.write(text)
        path = Path(handle.name)
    try:
        return project_file(path)
    finally:
        path.unlink()


def hunk_case(a_old: str, a_new: str, b_old: str, b_new: str) -> dict[str, object]:
    return {
        "a_old": a_old,
        "a_new": a_new,
        "b_old": b_old,
        "b_new": b_new,
        "overlap": hunk_overlap(Hunk(a_old, a_new), Hunk(b_old, b_new)),
    }


def overlap_between_case(incorrect: tuple[Hunk, ...], correction: tuple[Hunk, ...]) -> dict[str, object]:
    return {
        "incorrect": [{"old": hunk.old, "new": hunk.new} for hunk in incorrect],
        "correction": [{"old": hunk.old, "new": hunk.new} for hunk in correction],
        "overlap": max((hunk_overlap(a, b) for a in incorrect for b in correction), default=0.0),
    }


def corpus_files() -> list[Path]:
    return sorted(CORPUS.rglob("*.jsonl"))


def main() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)
    data = {
        "max_events": MAX_EVENTS,
        "lifts": {str(path.relative_to(CORPUS)): project_file(path) for path in corpus_files()},
        "synthetic": {name: project_jsonl(text) for name, text in SYNTHETIC_CASES.items()},
        "hunk_overlap_cases": [hunk_case(*case) for case in HUNK_CASES],
        "overlap_between_cases": [overlap_between_case(*case) for case in OVERLAP_BETWEEN_CASES],
    }
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(
        f"wrote {len(data['lifts'])} lift + {len(SYNTHETIC_CASES)} synthetic projections "
        f"+ {len(HUNK_CASES)} hunk cases + {len(OVERLAP_BETWEEN_CASES)} overlap-between cases "
        f"to {GOLDEN.relative_to(REPO_ROOT)}"
    )


if __name__ == "__main__":
    main()
