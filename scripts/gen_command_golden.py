"""Freeze the Python ``CommandLine`` structure into ``tests/testdata/command_golden.json``.

Serializes the parsed command line for a battery of bash command strings — the
command-prefix pins (single-sourced in
``rust/crates/py/data/command_prefix_pins.tsv`` and reached through the
``tests.test_command`` loader), every distinct ``Bash`` command in the deterministic
bench corpus (``.fixtures/corpus``, regenerated via ``scripts/gen_corpus.py``), and
hand-built edge cases (nested subshells, quoting, heredocs, unicode). A later run plus
``git diff`` shows Python-side drift, and ``tests/test_command_parity.py`` asserts the
Rust ``command_parse`` port reproduces the same structure.

Run: ``uv run --no-sync python scripts/gen_command_golden.py``
"""

from __future__ import annotations

import json
import subprocess
import sys
from typing import TYPE_CHECKING

import orjson

from cc_transcript.command import parse_command_line
from scripts.gen_corpus import DEFAULT_OUT as CORPUS
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from cc_transcript.command import Command, CommandLine, Redirect

GOLDEN = REPO_ROOT / "tests" / "testdata" / "command_golden.json"

# Hand-built shapes beyond the pins and corpus, chosen to expose any grammar drift.
EDGE_CASES: tuple[tuple[str, str], ...] = (
    ("nested-subshells", "(cd src && (make && make test)) || echo fail"),
    ("command-substitution-nested", "echo $(git log --format=%H | head -1)"),
    ("heredoc-body-not-commands", "cat <<'EOF'\nrm -rf /\ngit push --force\nEOF"),
    ("process-substitution-both", "diff <(sort a) <(sort b) > out.txt"),
    ("unicode-args", "git commit -m 'héllo 🤖 漢字'"),
    ("unicode-executable", "café --brew"),
    ("pipe-and-redirect-mix", "ENV=prod uv run pytest -k test 2>&1 | tee log | grep -c PASS"),
    ("background-job", "sleep 5 & echo started"),
    ("stacked-wrappers-env", "sudo env FOO=1 BAR=2 timeout 30 nice -n 5 cargo build --release"),
    ("comment-only", "# nothing to run here"),
    ("only-redirect", "2>&1 > out.txt"),
    ("semicolons-and-newlines", "a; b\nc && d || e"),
    ("assignment-only", "FOO=bar BAZ=qux"),
    ("glob-and-braces", "ls -la {src,tests}/*.py"),
    ("here-string", 'grep pattern <<< "$var"'),
    ("subshell-pipeline", "(echo a; echo b) | sort | uniq -c"),
    ("function-def-and-call", "greet() { echo hi; }; greet && greet"),
    ("empty-assignment-value", "EMPTY= make build"),
)


def redirect_to_dict(redirect: Redirect) -> dict[str, object]:
    return {"op": redirect.op, "target": redirect.target, "fd": redirect.fd}


def command_to_dict(cmd: Command) -> dict[str, object]:
    return {
        "raw": cmd.raw,
        "executable": cmd.executable,
        "args": list(cmd.args),
        "env": [list(pair) for pair in cmd.env],
        "redirects": [redirect_to_dict(redirect) for redirect in cmd.redirects],
        "program": cmd.program,
        "unwrapped_argv": list(cmd.unwrapped.argv),
        "prefix": cmd.prefix,
    }


def line_to_dict(line: CommandLine) -> dict[str, object]:
    return {
        "raw": line.raw,
        "parts": [{"op": op, "command": command_to_dict(cmd)} for cmd, op in line.parts],
        "prefixes": list(line.prefixes),
    }


def pin_commands() -> list[tuple[str, str]]:
    from tests.test_command import PREFIX_PIN_CASES, PREFIX_PIN_IDS

    return [(f"pin-{pid}", cmd) for pid, (cmd, _expected) in zip(PREFIX_PIN_IDS, PREFIX_PIN_CASES, strict=True)]


def corpus_commands() -> list[str]:
    seen: dict[str, None] = {}
    for path in sorted(CORPUS.rglob("*.jsonl")):
        for raw in path.read_bytes().splitlines():
            message = orjson.loads(raw).get("message") if raw else None
            content = message.get("content") if isinstance(message, dict) else None
            for block in content if isinstance(content, list) else []:
                if isinstance(block, dict) and block.get("type") == "tool_use" and block.get("name") == "Bash":
                    if isinstance(command := block.get("input", {}).get("command"), str) and command:
                        seen.setdefault(command, None)
    return list(seen)


def collect() -> list[tuple[str, str]]:
    seen: set[str] = set()
    tagged = (
        pin_commands()
        + [(f"corpus-{index:03d}", command) for index, command in enumerate(corpus_commands())]
        + [(f"edge-{name}", command) for name, command in EDGE_CASES]
    )
    return [(cid, command) for cid, command in tagged if not (command in seen or seen.add(command))]


def main() -> None:
    subprocess.run([sys.executable, str(REPO_ROOT / "scripts" / "gen_corpus.py")], check=True, cwd=REPO_ROOT)
    data = [
        {"id": cid, "command": command, "parsed": line_to_dict(parse_command_line(command))}
        for cid, command in collect()
    ]
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} golden command parses to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
