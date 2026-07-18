from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GENERATOR = ROOT / "scripts" / "gen_export_stub.py"


def test_export_stub_is_fresh() -> None:
    result = subprocess.run(
        [sys.executable, str(GENERATOR), "--check"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr or result.stdout


def test_every_export_resolves() -> None:
    probe = """
import cc_transcript

failures = []
for name in sorted(cc_transcript.EXPORTS):
    try:
        value = getattr(cc_transcript, name)
    except Exception as error:
        failures.append(f"{name}: {error!r}")
    else:
        if value is None:
            failures.append(f"{name}: resolved to None")
if failures:
    raise RuntimeError("failed exports:\\n" + "\\n".join(failures))
"""
    result = subprocess.run([sys.executable, "-c", probe], cwd=ROOT, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr or result.stdout
