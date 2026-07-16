"""Freeze the Python v13.2 splice layer into ``tests/testdata/splice_golden.json``.

Records, from the Python ``CommandLine`` reference (``cc_transcript.command``), the
byte-span splice surface that P6 ports to the Rust core: per-part ``Command.span``,
``Occurrence`` fields (``prev_op``/``next_op``/``piped`` including the ``PIPE_GAP_RE``
raw-byte-gap fallback), ``CommandLine.splice`` results, the parse-derived span-less
``ValueError`` leg, and ``CommandLine.rewrite_occurrences``. The corpus is the
command-prefix pins, the ``gen_command_golden`` edge cases, and splice-specific shapes
(redirects, absorbed trailing words, heredocs, subshells, unicode, ``|&``/newline pipes).

A later run plus ``git diff`` shows Python-side drift; ``tests/test_splice_parity.py``
asserts the flipped native surface reproduces the frozen structure. The out-of-order and
overlap ``ValueError`` legs need hand-built spans and stay as construction tests in
``tests/test_command.py``.

Run: ``uv run --no-sync python scripts/gen_splice_golden.py``
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING

from cc_transcript.command import parse_command_line
from scripts.gen_command_golden import EDGE_CASES
from scripts.gen_corpus import REPO_ROOT

if TYPE_CHECKING:
    from cc_transcript.command import CommandLine, Occurrence

GOLDEN = REPO_ROOT / "tests" / "testdata" / "splice_golden.json"

# Splice-layer shapes beyond the pins and edge cases: span extraction under redirects,
# absorbed trailing words (span None), the piped heuristic, and multibyte offsets.
SPLICE_CASES: tuple[tuple[str, str], ...] = (
    ("absorbed-trailing-word", "echo a >out b"),
    ("leading-redirect", ">out echo hi"),
    ("both-edge-redirects", "<in echo hi >out"),
    ("redirect-outside-span", "cat a > b; echo x"),
    ("only-redirects-empty-exe", "2>&1 > out.txt"),
    ("pipe-simple", "foo | bar"),
    ("pipe-ampersand", "a |& b"),
    ("pipe-chain", "cat f | grep x | wc -l"),
    ("newline-statements", "a\nb"),
    ("and-not-piped", "a && b"),
    ("comment-pipe-gap", "a # x|y\nb"),
    ("quoted-redirect-pipe", "cat a > 'x|y'\nb"),
    ("heredoc-body-pipe", "cat <<'EOF'\nx|y\nEOF\nb"),
    ("test-command-or", "a\n[[ x || y ]]\nb"),
    ("arithmetic-pipe", "a\n((1|2))\nb"),
    ("heredoc-body-later-command", "cat <<'EOF'\nrm -rf /\ngit push --force\nEOF\ngit push --force"),
    ("multibyte-before-span", "echo café; rm x"),
    ("subshell-interior", "(cd src && make)"),
    ("comment-tail", "echo hi # trailing comment"),
    ("repeated-identical", "x; x; x"),
    ("background-and-list", "sleep 5 & echo started; wait"),
)


def part_to_dict(occ: Occurrence) -> dict[str, object]:
    return {
        "executable": occ.command.executable,
        "span": list(occ.command.span) if occ.command.span is not None else None,
        "prev_op": occ.prev_op,
        "next_op": occ.next_op,
        "piped": occ.piped,
    }


def splice_probe(line: CommandLine) -> dict[str, object]:
    spliceable = [occ.index for occ in line.occurrences if occ.command.span is not None]
    single = {str(index): line.splice({index: f"R{index}"}) for index in spliceable}
    combined = line.splice({index: f"R{index}" for index in spliceable}) if spliceable else None
    rewrite = line.rewrite_occurrences(
        lambda occ: f"X{occ.index}" if occ.command.span is not None else None
    )
    error = None
    if (none_index := next((occ.index for occ in line.occurrences if occ.command.span is None), None)) is not None:
        try:
            line.splice({none_index: "X"})
        except ValueError as exc:
            error = str(exc)
    return {"single": single, "combined": combined, "rewrite": rewrite, "error": error}


def line_to_dict(line: CommandLine) -> dict[str, object]:
    return {
        "raw": line.raw,
        "parts": [part_to_dict(occ) for occ in line.occurrences],
        "splice": splice_probe(line),
    }


def collect() -> list[tuple[str, str]]:
    from tests.test_command import load_prefix_pins

    pin_cases, pin_ids = load_prefix_pins()
    seen: set[str] = set()
    tagged = (
        [(f"pin-{pid}", command) for pid, (command, _expected) in zip(pin_ids, pin_cases, strict=True)]
        + [(f"edge-{name}", command) for name, command in EDGE_CASES]
        + [(f"splice-{name}", command) for name, command in SPLICE_CASES]
    )
    return [(cid, command) for cid, command in tagged if not (command in seen or seen.add(command))]


def main() -> None:
    data = [
        {"id": cid, "command": command, "splice_layer": line_to_dict(parse_command_line(command))}
        for cid, command in collect()
    ]
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} splice-layer golden entries to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
