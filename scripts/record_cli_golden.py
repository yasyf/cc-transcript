"""Record the hermetic CLI golden transcripts under ``tests/testdata/cli_golden/``.

Regenerates the deterministic corpus (``scripts/gen_corpus.py``), then runs the
current Python CLI over it and captures exact stdout + exit code for each command,
so a later phase re-runs this script and ``git diff`` shows any behavioral drift.

Hermeticity: every command is scoped with a relative ``--root .fixtures/corpus`` or
an explicit relative transcript path (so ``display_path`` stays relative and portable,
never an absolute worktree/home path), ``--width`` is pinned, and ``TZ=UTC`` fixes the
``list`` mtime formatting. The corpus filenames are seed-deterministic, so the exact
argv reproduces across machines.

Invocation matrix (name -> argv, cwd = repo root, binary = .venv/bin/cc-transcript):
  list         list --root .fixtures/corpus
  list_json    list --root .fixtures/corpus --json
  show         show <smallest> --width 100
  show_json    show <smallest> --json
  show_head5   show <smallest> --head 5 --width 100
  grep_match   grep "tests are green" --root .fixtures/corpus --width 100 --max-matches 10
  grep_nomatch grep zzzz_no_such_pattern_xyzzy --root .fixtures/corpus   (exit 1)
  stats        stats --root .fixtures/corpus --all
  stats_json   stats --root .fixtures/corpus --all --json
  tools        tools <smallest>
  commands     commands <smallest>

Run: ``uv run --no-sync python scripts/record_cli_golden.py``
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import orjson

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS_REL = Path(".fixtures") / "corpus"
GOLDEN_DIR = REPO_ROOT / "tests" / "testdata" / "cli_golden"
CLI = REPO_ROOT / ".venv" / "bin" / "cc-transcript"


def regenerate_corpus() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)


def smallest_transcript() -> str:
    files = sorted((REPO_ROOT / CORPUS_REL).rglob("*.jsonl"))
    smallest = min(files, key=lambda p: (p.stat().st_size, str(p)))
    return str(smallest.relative_to(REPO_ROOT))


def matrix(smallest: str) -> dict[str, list[str]]:
    root = str(CORPUS_REL)
    return {
        "list": ["list", "--root", root],
        "list_json": ["list", "--root", root, "--json"],
        "show": ["show", smallest, "--width", "100"],
        "show_json": ["show", smallest, "--json"],
        "show_head5": ["show", smallest, "--head", "5", "--width", "100"],
        "grep_match": ["grep", "tests are green", "--root", root, "--width", "100", "--max-matches", "10"],
        "grep_nomatch": ["grep", "zzzz_no_such_pattern_xyzzy", "--root", root],
        "stats": ["stats", "--root", root, "--all"],
        "stats_json": ["stats", "--root", root, "--all", "--json"],
        "tools": ["tools", smallest],
        "commands": ["commands", smallest],
    }


def run(argv: list[str]) -> tuple[bytes, int]:
    proc = subprocess.run(
        [str(CLI), *argv],
        cwd=REPO_ROOT,
        env=os.environ | {"TZ": "UTC"},
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    return proc.stdout, proc.returncode


def record() -> None:
    regenerate_corpus()
    GOLDEN_DIR.mkdir(parents=True, exist_ok=True)
    for existing in GOLDEN_DIR.glob("*.out"):
        existing.unlink()
    manifest: dict[str, dict[str, object]] = {}
    for name, argv in matrix(smallest_transcript()).items():
        stdout, code = run(argv)
        (GOLDEN_DIR / f"{name}.out").write_bytes(stdout)
        manifest[name] = {"argv": argv, "exit_code": code, "stdout_bytes": len(stdout)}
    (GOLDEN_DIR / "manifest.json").write_bytes(
        orjson.dumps(manifest, option=orjson.OPT_INDENT_2 | orjson.OPT_SORT_KEYS)
    )
    print(f"recorded {len(manifest)} golden commands to {GOLDEN_DIR.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    record()
