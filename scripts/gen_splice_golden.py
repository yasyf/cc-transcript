"""Record the native splice surface in ``tests/testdata/splice_golden.json``.

Run: ``uv run --no-sync python scripts/gen_splice_golden.py``
"""

from __future__ import annotations

import json

from cc_transcript.command import parse_command_line
from scripts.gen_corpus import REPO_ROOT
from tests.test_splice_parity import line_to_dict

GOLDEN = REPO_ROOT / "tests" / "testdata" / "splice_golden.json"

def main() -> None:
    entries = json.loads(GOLDEN.read_text(encoding="utf-8"))
    data = [
        entry | {"splice_layer": line_to_dict(parse_command_line(entry["command"]))}
        for entry in entries
    ]
    GOLDEN.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} splice-layer golden entries to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
