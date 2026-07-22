"""Record filter projections in ``tests/testdata/filter_golden.json``.

Run: ``uv run --no-sync python scripts/gen_filter_golden.py``
"""

from __future__ import annotations

import json
from pathlib import Path
from tempfile import TemporaryDirectory

from tests.support import fixture_bytes, project_survivor
from tests.test_filter_parity import PRESETS, battery_bytes, rust_filtered

REPO_ROOT = Path(__file__).resolve().parent.parent
GOLDEN = REPO_ROOT / "tests" / "testdata" / "filter_golden.json"


def project(raw: bytes, path: Path) -> dict[str, list[dict[str, str | bool]]]:
    path.write_bytes(raw)
    return {
        name: [project_survivor(event) for event in rust_filtered(path, spec)]
        for name, spec in PRESETS.items()
    }


def main() -> None:
    with TemporaryDirectory() as directory:
        root = Path(directory)
        data = {
            "battery": project(battery_bytes(), root / "battery.jsonl"),
            "fixture": project(fixture_bytes(), root / "fixture.jsonl"),
        }
    GOLDEN.write_text(json.dumps(data, indent=1) + "\n", encoding="utf-8")
    print(f"wrote filter projections to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
