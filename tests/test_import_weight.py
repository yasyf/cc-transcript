from __future__ import annotations

import json
import subprocess
import sys

HEAVY = ("anyio", "orjson", "click", "aiosqlite", "pydantic", "loguru", "spacy", "tree_sitter", "tree_sitter_bash")

PROBE = """
import json, sys
import cc_transcript.ids, cc_transcript.tools
roots = sorted({m.split(".")[0] for m in sys.modules if not m.startswith("_")})
print(json.dumps(roots))
"""


def test_ids_and_tools_import_stdlib_only() -> None:
    out = subprocess.run([sys.executable, "-c", PROBE], capture_output=True, text=True, check=True)
    loaded = set(json.loads(out.stdout))
    assert not loaded & set(HEAVY), f"hot-path import pulled heavy deps: {sorted(loaded & set(HEAVY))}"
    assert "cc_transcript" in loaded


def test_lazy_root_resolves_exports_on_demand() -> None:
    probe = """
import json, sys
import cc_transcript
before = "cc_transcript.parser" in sys.modules
ref = cc_transcript.EventRef
after_ids = "cc_transcript.parser" in sys.modules
parser = cc_transcript.TranscriptParser
after_parser = "cc_transcript.parser" in sys.modules
print(json.dumps([before, after_ids, after_parser]))
"""
    out = subprocess.run([sys.executable, "-c", probe], capture_output=True, text=True, check=True)
    assert json.loads(out.stdout) == [False, False, True]


def test_unknown_attribute_raises() -> None:
    import cc_transcript

    try:
        cc_transcript.NotARealExport
    except AttributeError as error:
        assert "NotARealExport" in str(error)
    else:
        raise AssertionError("expected AttributeError")
