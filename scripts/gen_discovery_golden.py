"""Freeze the Python transcript-discovery walk into ``tests/testdata/discovery_golden.json``.

Builds synthesized project-dir trees in throwaway tmp dirs — nested projects, a macOS
resource fork, a hidden sidechain, a symlinked file (a cc-pool second spelling) and a
symlinked directory, with ``os.utime``-pinned integer mtimes — and freezes what each
discovery function returns: ``find_transcripts`` (every ``*.jsonl`` sorted, forks
included), ``find_in`` (name/freshness/limit filtering), ``resolve``
(newest-mtime real path after symlink dedup), and ``subagent_paths`` /
``subagent_transcripts``. Paths are frozen relative to the tree root, so nothing depends
on the tmp location.

A later run plus ``git diff`` shows Python-side drift, and ``tests/test_discovery_parity.py``
asserts the Rust ``discovery_*`` ports reproduce every result.

Run: ``uv run --no-sync python scripts/gen_discovery_golden.py``
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING


if TYPE_CHECKING:
    from collections.abc import Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
GOLDEN = REPO_ROOT / "tests" / "testdata" / "discovery_golden.json"

# Each scenario builds one tree (files then symlinks) and runs one function. mtimes are
# integers so the reconstructed float is exact across Python and Rust.
DISCOVERY_SCENARIOS: dict[str, dict[str, object]] = {
    "find_transcripts": {
        "func": "find_transcripts",
        "tree": [
            {"file": "projA/s1.jsonl", "mtime": 1000},
            {"file": "projA/s2.jsonl", "mtime": 1002},
            {"file": "projA/._fork.jsonl", "mtime": 999},
            {"file": "projA/nested/s3.jsonl", "mtime": 1001},
            {"file": "projA/notes.txt", "mtime": 1000},
            {"file": "real/s5.jsonl", "mtime": 1004},
            {"symlink": "projB/s5.jsonl", "target": "real/s5.jsonl"},
            {"symlink": "linkdir", "target": "real"},
        ],
        "args": {},
    },
    "find_in_filtered": {
        "func": "find_in",
        "tree": [
            {"file": "projA/alpha.jsonl", "mtime": 1000},
            {"file": "projA/beta.jsonl", "mtime": 1002},
            {"file": "projA/alpha-two.jsonl", "mtime": 1001},
            {"file": "projA/nested/alpha-three.jsonl", "mtime": 1003},
        ],
        "args": {"directory": ".", "name_contains": "alpha", "limit": 2, "known_mtimes": {"projA/alpha.jsonl": 1000}},
    },
    "find_transcript_newest": {
        "func": "find_transcript",
        "tree": [
            {"file": "projA/sess-x.jsonl", "mtime": 1000},
            {"file": "projC/sess-x.jsonl", "mtime": 1005},
            {"file": "projA/other.jsonl", "mtime": 2000},
            {"symlink": "projB/sess-x.jsonl", "target": "projA/sess-x.jsonl"},
        ],
        "args": {"session_id": "sess-x"},
    },
    "find_transcript_miss": {
        "func": "find_transcript",
        "tree": [{"file": "projA/other.jsonl", "mtime": 1000}],
        "args": {"session_id": "ghost"},
    },
    "subagents": {
        "func": "subagents",
        "tree": [
            {"file": "projA/sess.jsonl", "mtime": 1000},
            {"file": "projA/sess/subagents/agent-t1.jsonl", "mtime": 1001},
            {"file": "projA/sess/subagents/agent-t2.jsonl", "mtime": 1002},
            {"file": "projA/sess/subagents/._fork.jsonl", "mtime": 999},
            {"file": "projA/sess/subagents/.hidden.jsonl", "mtime": 998},
        ],
        "args": {"path": "projA/sess.jsonl"},
    },
}

# (path, expected): is_subagent_path turns on the .jsonl suffix AND an agent- prefix.
IS_SUBAGENT_CASES: tuple[tuple[str, bool], ...] = (
    ("a/b/agent-tool7.jsonl", True),
    ("a/b/agent-.jsonl", True),
    ("a/b/s.jsonl", False),
    ("a/b/agent-tool7.txt", False),
    ("agent-tool7", False),
    ("agent-tool7.jsonl", True),
)


def build_tree(root: Path, tree: Sequence[dict[str, object]]) -> None:
    for op in tree:
        if "file" in op:
            path = root / str(op["file"])
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("x", encoding="utf-8")
            stamp = int(op["mtime"]) * 1_000_000_000  # type: ignore[arg-type]
            os.utime(path, ns=(stamp, stamp))
        else:
            link = root / str(op["symlink"])
            link.parent.mkdir(parents=True, exist_ok=True)
            os.symlink(root / str(op["target"]), link)


def rel(base: Path, path: str) -> str:
    return str(Path(path).relative_to(base))


def python_find_transcripts(root: Path) -> list[Path]:
    import cc_transcript.discovery as disc

    return disc.discover(root)


def python_find_in(directory: Path, name_contains: str | None, limit: int | None, known: dict[str, float] | None) -> list[tuple[Path, float]]:
    from cc_transcript.discovery import find_in

    return find_in(directory, name_contains=name_contains, limit=limit, known_mtimes=known)


def python_find_transcript(root: Path, session_id: str) -> Path | None:
    from cc_transcript.discovery import TRANSCRIPT_MEMO, resolve
    from cc_transcript.ids import SessionId

    TRANSCRIPT_MEMO.clear()
    return resolve(SessionId(session_id), root=root)


def run_python(root: Path, scenario: dict[str, object]) -> dict[str, object]:
    from cc_transcript.discovery import subagent_paths, subagent_transcripts

    build_tree(root, scenario["tree"])  # type: ignore[arg-type]
    args = scenario["args"]
    assert isinstance(args, dict)
    match scenario["func"]:
        case "find_transcripts":
            return {"transcripts": [rel(root, str(path)) for path in python_find_transcripts(root)]}
        case "find_in":
            directory = root / str(args["directory"])
            known = {str(root / key): value for key, value in (args.get("known_mtimes") or {}).items()} or None
            result = python_find_in(directory, args.get("name_contains"), args.get("limit"), known)
            return {"found": [[rel(root, str(path)), mtime] for path, mtime in result]}
        case "find_transcript":
            hit = python_find_transcript(root, str(args["session_id"]))
            return {"hit": None if hit is None else rel(root.resolve(), str(hit))}
        case "subagents":
            path = root / str(args["path"])
            return {
                "paths": [rel(root, str(p)) for p in subagent_paths(path)],
                "transcripts": {str(tid): rel(root, str(p)) for tid, p in subagent_transcripts(path).items()},
            }
        case func:
            raise ValueError(f"unknown discovery func {func!r}")


def is_subagent_cases_python() -> list[dict[str, object]]:
    from cc_transcript.discovery import is_subagent_path

    return [{"path": path, "result": is_subagent_path(Path(path))} for path, _ in IS_SUBAGENT_CASES]


def project_scenario(name: str) -> dict[str, object]:
    with tempfile.TemporaryDirectory() as tmp:
        return run_python(Path(tmp), DISCOVERY_SCENARIOS[name])


def main() -> None:
    data = {
        "scenarios": {name: project_scenario(name) for name in DISCOVERY_SCENARIOS},
        "is_subagent_path": is_subagent_cases_python(),
    }
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(DISCOVERY_SCENARIOS)} discovery scenarios + {len(IS_SUBAGENT_CASES)} is_subagent cases to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
