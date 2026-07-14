"""Freeze the Python live-tail cursor into ``tests/testdata/watch_golden.json``.

Drives ``cc_transcript.watch.tick`` over a scripted sequence of filesystem states —
append a partial line, complete it, append events, no-op, dedupe, compact (truncate),
rotate in a new file, and drop the garbage/year-zero/mode lines — and records, per
step, the yielded events and the resulting cursor state. File states are synthesized
deterministically in a throwaway tmp tree with fixed contents and ``os.utime``-pinned
integer mtimes, so nothing depends on a corpus and every mtime float is exact on both
sides.

A later run plus ``git diff`` shows Python-side drift, and ``tests/test_watch_parity.py``
asserts the Rust ``WatchTailer`` port reproduces every step's yields and cursor state.

Run: ``uv run --no-sync python scripts/gen_watch_golden.py``
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING

import anyio

if TYPE_CHECKING:
    from collections.abc import Sequence

    from cc_transcript.models import TranscriptEvent

REPO_ROOT = Path(__file__).resolve().parent.parent
GOLDEN = REPO_ROOT / "tests" / "testdata" / "watch_golden.json"


def user(uuid: str, session: str, sec: int, text: str = "hi") -> str:
    return json.dumps(
        {
            "type": "user",
            "uuid": uuid,
            "sessionId": session,
            "timestamp": f"2026-01-01T00:00:{sec:02d}.000Z",
            "message": {"role": "user", "content": text},
        }
    )


def year_zero(uuid: str, session: str) -> str:
    return json.dumps(
        {
            "type": "user",
            "uuid": uuid,
            "sessionId": session,
            "timestamp": "0000-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": "zero"},
        }
    )


def mode(session: str, value: str) -> str:
    return json.dumps({"type": "mode", "sessionId": session, "mode": value})


def other(kind: str = "summary") -> str:
    return json.dumps({"type": kind, "leafUuid": "x"})


def jsonl(*lines: str) -> str:
    return "".join(f"{line}\n" for line in lines)


# "set" truncates, "append" appends verbatim bytes; a newline-less tail stays partial.
SCENARIOS: dict[str, dict[str, object]] = {
    "tail": {
        "from_start": False,
        "steps": [
            {
                "name": "prime",
                "ops": [{"file": "projA/s1.jsonl", "action": "set", "data": jsonl(user("a", "sess-1", 0), user("b", "sess-1", 1)), "mtime": 1000}],
            },
            {
                "name": "partial",
                "ops": [{"file": "projA/s1.jsonl", "action": "append", "data": user("c", "sess-1", 2), "mtime": 1001}],
            },
            {
                "name": "complete",
                "ops": [{"file": "projA/s1.jsonl", "action": "append", "data": "\n" + user("d", "sess-1", 3) + "\n", "mtime": 1002}],
            },
            {"name": "noop", "ops": []},
            {
                "name": "dup_and_new",
                "ops": [{"file": "projA/s1.jsonl", "action": "append", "data": jsonl(user("d", "sess-1", 3), user("e", "sess-1", 4)), "mtime": 1003}],
            },
            {
                "name": "compact",
                "ops": [{"file": "projA/s1.jsonl", "action": "set", "data": jsonl(user("f", "sess-1", 5)), "mtime": 1004}],
            },
            {
                "name": "new_file",
                "ops": [{"file": "projA/s2.jsonl", "action": "set", "data": jsonl(user("g", "sess-2", 6)), "mtime": 1005}],
            },
            {
                "name": "mixed_drops",
                "ops": [
                    {
                        "file": "projA/s2.jsonl",
                        "action": "append",
                        "data": jsonl(year_zero("z", "sess-2"), "this is not json at all", user("h", "sess-2", 7), "[1, 2, 3]", mode("sess-2", "acceptEdits")),
                        "mtime": 1006,
                    }
                ],
            },
        ],
    },
    "from_start": {
        "from_start": True,
        "steps": [
            {
                "name": "prime_all",
                "ops": [{"file": "projA/s1.jsonl", "action": "set", "data": jsonl(user("a", "sess-1", 0), user("b", "sess-1", 1)), "mtime": 2000}],
            },
            {
                "name": "append_more",
                "ops": [{"file": "projA/s1.jsonl", "action": "append", "data": jsonl(user("c", "sess-1", 2)), "mtime": 2001}],
            },
        ],
    },
    "sidechain": {
        "from_start": True,
        "steps": [
            {
                "name": "sidechain_prime",
                "ops": [
                    {
                        "file": "projA/sess-main/subagents/agent-tool7.jsonl",
                        "action": "set",
                        "data": jsonl(other("summary"), user("s1", "sub-sess", 10)),
                        "mtime": 3000,
                    }
                ],
            },
        ],
    },
}


def apply_ops(root: Path, ops: Sequence[dict[str, object]]) -> None:
    for op in ops:
        path = root / str(op["file"])
        match op["action"]:
            case "set":
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(str(op["data"]).encode("utf-8"))
            case "append":
                with path.open("ab") as handle:
                    handle.write(str(op["data"]).encode("utf-8"))
            case action:
                raise ValueError(f"unknown op action {action!r}")
        stamp = int(op["mtime"]) * 1_000_000_000
        os.utime(path, ns=(stamp, stamp))


def rel(root: Path, path: str) -> str:
    return str(Path(path).relative_to(root))


def project_events(root: Path, events: Sequence[tuple[str, str, bool, TranscriptEvent]]) -> list[dict[str, object]]:
    from cc_transcript.filterspec import event_meta

    return [
        {
            "path": rel(root, path),
            "session_id": session_id,
            "is_sidechain": is_sidechain,
            "uuid": meta.uuid if (meta := event_meta(event)) is not None else None,
        }
        for path, session_id, is_sidechain, event in events
    ]


def relativize_state(root: Path, raw: dict[str, object]) -> dict[str, object]:
    cursors = raw["cursors"]
    assert isinstance(cursors, dict)
    return {"primed": raw["primed"], "cursors": {rel(root, path): cursor for path, cursor in cursors.items()}}


def python_state(state: object) -> dict[str, object]:
    return {
        "primed": state.primed,
        "cursors": {
            str(path): {
                "offset": cursor.offset,
                "size": cursor.size,
                "mtime": cursor.mtime,
                "session_id": cursor.session_id,
                "seen": list(cursor.seen),
            }
            for path, cursor in state.cursors.items()
        },
    }


async def run_python_scenario(root: Path, scenario: dict[str, object]) -> list[dict[str, object]]:
    from cc_transcript.watch import TailState, tick

    state = TailState()
    from_start = bool(scenario["from_start"])
    steps = scenario["steps"]
    assert isinstance(steps, list)
    results: list[dict[str, object]] = []
    for step in steps:
        apply_ops(root, step["ops"])
        events = await tick(state, [root], from_start=from_start)
        results.append(
            {
                "name": step["name"],
                "yields": project_events(root, [(str(e.path), e.session_id, e.is_sidechain, e.event) for e in events]),
                "state": relativize_state(root, python_state(state)),
            }
        )
    return results


def project_python(name: str) -> list[dict[str, object]]:
    with tempfile.TemporaryDirectory() as tmp:
        return anyio.run(run_python_scenario, Path(tmp), SCENARIOS[name])


def main() -> None:
    data = {name: project_python(name) for name in SCENARIOS}
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} watch scenarios ({sum(len(steps) for steps in data.values())} steps) to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
