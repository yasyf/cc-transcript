"""Golden parity: the native ``cc_transcript.command`` surface reproduces the v13.2 splice layer.

The golden (``testdata/splice_golden.json``) freezes the Python v13.2 ``CommandLine`` splice
surface — per-part ``Command.span``, ``Occurrence`` ``prev_op``/``next_op``/``piped``,
``CommandLine.splice`` results, the parse-derived span-less ``ValueError`` message, and
``rewrite_occurrences`` — over the command-prefix pins, edge cases, and splice-specific shapes.
The generator that wrote it (``scripts/gen_splice_golden.py``) was removed alongside command.py's
Python bodies in the P6 deletion sweep; the golden is the frozen contract, and this asserts the
flipped native views serialize to the same structure. The probe helpers below were lifted from
that generator so the surface stays independently exercised.

The out-of-order and overlap ``ValueError`` legs need hand-built spans and live as construction
tests in ``tests/test_command.py`` (``TestSplice``).
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from cc_transcript.command import parse_command_line

if TYPE_CHECKING:
    from cc_transcript.command import CommandLine, Occurrence

GOLDEN = json.loads(
    (Path(__file__).resolve().parent / "testdata" / "splice_golden.json").read_text(encoding="utf-8")
)
CASES = [pytest.param(entry, id=entry["id"]) for entry in GOLDEN]


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


@pytest.mark.parametrize("entry", CASES)
def test_splice_surface_matches_golden(entry: dict) -> None:
    assert line_to_dict(parse_command_line(entry["command"])) == entry["splice_layer"]
