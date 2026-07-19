"""Record the hermetic CLI golden transcripts under ``tests/testdata/cli_golden/``.

Regenerates the deterministic corpus (``scripts/gen_corpus.py``), then runs the
CLI over it and captures exact stdout + stderr + exit code for each
command, so a later phase re-runs this script and ``git diff`` shows any behavioral
drift. An empty ``.err`` file pins "no stderr" as part of the contract.

Hermeticity: every command is scoped with a relative ``--root .fixtures/corpus`` or
an explicit relative transcript path (so the printed transcript path stays relative
and portable, never an absolute worktree/home path), ``--width`` is pinned, ``TZ=UTC`` fixes the
``list`` mtime formatting, and the corrections cases run under ``HOME=.fixtures/home``
— wiped per recording — so the ledger is fresh and its rows carry only pinned values.
The corpus filenames are seed-deterministic, so the exact argv reproduces across
machines.

Invocation matrix (name -> argv, cwd = repo root, binary = .venv/bin/cc-transcript):
  list              list --root .fixtures/corpus
  list_json         list --root .fixtures/corpus --json
  list_codex        list --provider codex --codex-root .fixtures/codex-sessions
  list_codex_contains list --provider codex --codex-root .fixtures/codex-sessions --contains <session suffix>
  list_all          list --provider all --root .fixtures/corpus --codex-root .fixtures/codex-sessions --limit 1
  list_all_json     list --provider all --root .fixtures/corpus --codex-root .fixtures/codex-sessions --limit 1 --json
  list_all_invalid_claude_root list --provider all --root <file> --codex-root .fixtures/codex-sessions   (exit 2)
  list_all_invalid_codex_root list --provider all --root .fixtures/corpus --codex-root <file>   (exit 2)
  list_codex_invalid_root list --provider codex --codex-root <file>   (exit 2)
  show              show <smallest> --width 100
  show_json         show <smallest> --json
  show_head5        show <smallest> --head 5 --width 100
  show_codex        show <current Codex rollout> --width 100
  grep_match        grep "tests are green" --root .fixtures/corpus --width 100 --max-matches 10
  grep_nomatch      grep zzzz_no_such_pattern_xyzzy --root .fixtures/corpus   (exit 1)
  grep_codex        grep python3 <current Codex rollout> --width 100 --max-matches 10
  stats             stats --root .fixtures/corpus --all
  stats_json        stats --root .fixtures/corpus --all --json
  stats_codex       stats <current Codex rollout>
  tools             tools <smallest>
  commands          commands <smallest>
  mcp               mcp --root tests/testdata/mcp_root --all   (committed MCP fixture; the corpus has no MCP uses)
  mcp_json          mcp --root tests/testdata/mcp_root --all --json
  permissions       permissions --root .fixtures/corpus --all
  permissions_json  permissions --root .fixtures/corpus --all --json
  slice             slice --session <smallest session> --since/--until <wide pinned window> --root .fixtures/corpus
  scratchpad_missing scratchpad --session <reserved UUID>, TMPDIR=.fixtures/tmp   (exit 1, not-found on stderr)
  scratchpad_invalid scratchpad --session not-a-uuid   (exit 2, usage error on stderr)
  digest            digest, pinned two-row JSON array on stdin
  digest_check      digest --check tests/testdata/digest_fixtures.json
  digest_badcheck   digest --check <fixture with one corrupted digest>   (exit 1, mismatch on stderr)
  corrections_add   corrections add, every field pinned incl. --ts-ms, HOME=.fixtures/home
  corrections_query corrections query --session sess-golden, same HOME

Skipped commands:
  watch  a poll-forever loop with no bounded run mode; nothing byte-stable to record.
  scratchpad (success leg)  prints an absolute tmp-root path that varies by machine and uid;
    the behavioral tests in tests/test_cli.py cover resolution.

Run: ``uv run --no-sync python scripts/record_cli_golden.py``
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import NamedTuple

import orjson

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS_REL = Path(".fixtures") / "corpus"
CODEX_ROOT_REL = Path(".fixtures") / "codex-sessions"
HOME_REL = Path(".fixtures") / "home"
GOLDEN_DIR = REPO_ROOT / "tests" / "testdata" / "cli_golden"
CLI = REPO_ROOT / ".venv" / "bin" / "cc-transcript"
MCP_ROOT = "tests/testdata/mcp_root"
CODEX_TRANSCRIPT_REL = Path(
    "tests/testdata/codex/rollout-2026-07-16T16-20-00-019f67f0-2a3b-7c4d-8e5f-000000000303.jsonl"
)
CODEX_COMPRESSED_REL = Path(
    "tests/testdata/codex/rollout-2026-07-16T16-22-15-019f6800-3b4c-7d5e-9f60-000000000404.jsonl.zst"
)
INVALID_ROOT_REL = Path("tests/testdata/digest_fixtures.json")
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"
ADDITIVE_CASES = frozenset(
    {
        "grep_codex",
        "list_all",
        "list_all_invalid_claude_root",
        "list_all_invalid_codex_root",
        "list_all_json",
        "list_codex",
        "list_codex_contains",
        "list_codex_invalid_root",
        "show_codex",
        "stats_codex",
    }
)

DIGEST_STDIN = b'[{"tool": "Bash", "input": {"command": "ls"}}, {"tool": "Read", "input": {"file_path": "a.py"}}]'

CORRECTIONS_ADD = [
    "corrections",
    "add",
    "--session",
    "sess-golden",
    "--source",
    "cc-review",
    "--anchor",
    "review:41:7",
    "--incorrect-file",
    "cc_transcript/parser.py",
    "--ts-ms",
    "1769331734148",
    "--origin",
    "review",
    "--incorrect-old",
    "return None",
    "--incorrect-new",
    "return result",
    "--correction-text",
    "use the typed layer",
    "--repo",
    "cc-transcript",
]


class Case(NamedTuple):
    argv: list[str]
    stdin: bytes | None = None
    home: Path | None = None
    env: dict[str, str] | None = None


def regenerate_corpus() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)


def stage_codex_root() -> None:
    root = REPO_ROOT / CODEX_ROOT_REL
    shutil.rmtree(root, ignore_errors=True)
    day = root / "2026" / "07" / "16"
    day.mkdir(parents=True)
    compressed = REPO_ROOT / CODEX_COMPRESSED_REL
    assert compressed.read_bytes()[:4] == ZSTD_MAGIC
    for source_rel, mtime in (
        (CODEX_TRANSCRIPT_REL, 1784218800),
        (CODEX_COMPRESSED_REL, 1784218935),
    ):
        source = REPO_ROOT / source_rel
        target = day / source.name
        shutil.copyfile(source, target)
        os.utime(target, (mtime, mtime))


def smallest_transcript() -> str:
    files = sorted((REPO_ROOT / CORPUS_REL).rglob("*.jsonl"))
    smallest = min(files, key=lambda p: (p.stat().st_size, str(p)))
    return str(smallest.relative_to(REPO_ROOT))


def corrupted_digest_fixture() -> str:
    rows = orjson.loads((REPO_ROOT / "tests" / "testdata" / "digest_fixtures.json").read_bytes())
    rows[0]["digest"] = "0" * 64
    bad = REPO_ROOT / ".fixtures" / "digest_badcheck.json"
    bad.write_bytes(orjson.dumps(rows))
    return str(bad.relative_to(REPO_ROOT))


def matrix(smallest: str) -> dict[str, Case]:
    root = str(CORPUS_REL)
    codex_root = str(CODEX_ROOT_REL)
    codex_transcript = str(CODEX_TRANSCRIPT_REL)
    invalid_root = str(INVALID_ROOT_REL)
    home = REPO_ROOT / HOME_REL
    return {
        "list": Case(["list", "--root", root]),
        "list_json": Case(["list", "--root", root, "--json"]),
        "list_codex": Case(["list", "--provider", "codex", "--codex-root", codex_root]),
        "list_codex_contains": Case(
            [
                "list",
                "--provider",
                "codex",
                "--codex-root",
                codex_root,
                "--contains",
                "000000000303",
            ]
        ),
        "list_codex_invalid_root": Case(["list", "--provider", "codex", "--codex-root", invalid_root]),
        "list_all": Case(
            [
                "list",
                "--provider",
                "all",
                "--root",
                root,
                "--codex-root",
                codex_root,
                "--limit",
                "1",
            ]
        ),
        "list_all_json": Case(
            [
                "list",
                "--provider",
                "all",
                "--root",
                root,
                "--codex-root",
                codex_root,
                "--limit",
                "1",
                "--json",
            ]
        ),
        "list_all_invalid_claude_root": Case(
            [
                "list",
                "--provider",
                "all",
                "--root",
                invalid_root,
                "--codex-root",
                codex_root,
            ]
        ),
        "list_all_invalid_codex_root": Case(
            [
                "list",
                "--provider",
                "all",
                "--root",
                root,
                "--codex-root",
                invalid_root,
            ]
        ),
        "show": Case(["show", smallest, "--width", "100"]),
        "show_json": Case(["show", smallest, "--json"]),
        "show_head5": Case(["show", smallest, "--head", "5", "--width", "100"]),
        "show_codex": Case(["show", codex_transcript, "--width", "100"]),
        "grep_match": Case(["grep", "tests are green", "--root", root, "--width", "100", "--max-matches", "10"]),
        "grep_nomatch": Case(["grep", "zzzz_no_such_pattern_xyzzy", "--root", root]),
        "grep_codex": Case(["grep", "python3", codex_transcript, "--width", "100", "--max-matches", "10"]),
        "stats": Case(["stats", "--root", root, "--all"]),
        "stats_json": Case(["stats", "--root", root, "--all", "--json"]),
        "stats_codex": Case(["stats", codex_transcript]),
        "tools": Case(["tools", smallest]),
        "commands": Case(["commands", smallest]),
        "mcp": Case(["mcp", "--root", MCP_ROOT, "--all"]),
        "mcp_json": Case(["mcp", "--root", MCP_ROOT, "--all", "--json"]),
        "permissions": Case(["permissions", "--root", root, "--all"]),
        "permissions_json": Case(["permissions", "--root", root, "--all", "--json"]),
        "slice": Case(
            [
                "slice",
                "--session",
                Path(smallest).stem,
                "--since",
                "2020-01-01T00:00:00Z",
                "--until",
                "2030-01-01T00:00:00Z",
                "--root",
                root,
            ]
        ),
        "scratchpad_missing": Case(
            ["scratchpad", "--session", "cccccccc-cccc-cccc-cccc-cccccccccccc"],
            env={"TMPDIR": str(REPO_ROOT / ".fixtures" / "tmp")},
        ),
        "scratchpad_invalid": Case(["scratchpad", "--session", "not-a-uuid"]),
        "digest": Case(["digest"], stdin=DIGEST_STDIN),
        "digest_check": Case(["digest", "--check", "tests/testdata/digest_fixtures.json"]),
        "digest_badcheck": Case(["digest", "--check", corrupted_digest_fixture()]),
        "corrections_add": Case(CORRECTIONS_ADD, home=home),
        "corrections_query": Case(["corrections", "query", "--session", "sess-golden"], home=home),
    }


def run(case: Case) -> tuple[bytes, bytes, int]:
    env = os.environ | {"TZ": "UTC"} | (case.env or {}) | ({"HOME": str(case.home)} if case.home else {})
    proc = subprocess.run(
        [str(CLI), *case.argv],
        cwd=REPO_ROOT,
        env=env,
        input=case.stdin,
        capture_output=True,
    )
    return proc.stdout, proc.stderr, proc.returncode


def record(golden_dir: Path, names: frozenset[str] | None = None) -> None:
    regenerate_corpus()
    stage_codex_root()
    shutil.rmtree(REPO_ROOT / HOME_REL, ignore_errors=True)
    (REPO_ROOT / HOME_REL).mkdir(parents=True)
    (REPO_ROOT / ".fixtures" / "tmp").mkdir(parents=True, exist_ok=True)
    golden_dir.mkdir(parents=True, exist_ok=True)
    if names is None:
        for existing in (*golden_dir.glob("*.out"), *golden_dir.glob("*.err")):
            existing.unlink()
        manifest: dict[str, dict[str, object]] = {}
    else:
        manifest = orjson.loads((golden_dir / "manifest.json").read_bytes())
    cases = matrix(smallest_transcript())
    selected = list(cases.items()) if names is None else [(name, case) for name, case in cases.items() if name in names]
    for name, case in selected:
        stdout, stderr, code = run(case)
        (golden_dir / f"{name}.out").write_bytes(stdout)
        (golden_dir / f"{name}.err").write_bytes(stderr)
        manifest[name] = {
            "argv": case.argv,
            "exit_code": code,
            "stdout_bytes": len(stdout),
            "stderr_bytes": len(stderr),
        }
    (golden_dir / "manifest.json").write_bytes(
        orjson.dumps(manifest, option=orjson.OPT_INDENT_2 | orjson.OPT_SORT_KEYS)
    )
    print(f"recorded {len(selected)} golden commands to {golden_dir}")


if __name__ == "__main__":
    record(GOLDEN_DIR, ADDITIVE_CASES)
