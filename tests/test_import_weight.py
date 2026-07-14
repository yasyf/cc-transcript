from __future__ import annotations

import json
import subprocess
import sys

HEAVY = ("anyio", "orjson", "click", "aiosqlite", "pydantic", "loguru", "tree_sitter", "tree_sitter_bash")

# Measured 2.5-6.3ms (cold) on a 2026 arm64 dev machine; ~2x the cold measurement.
IMPORT_BUDGET_MS = 15

ROOT_PROBE = """
import json, sys, time
start = time.perf_counter()
import cc_transcript
elapsed_ms = (time.perf_counter() - start) * 1000
stdlib = set(sys.stdlib_module_names)
roots = {m.split(".")[0] for m in sys.modules if not m.startswith("_")}
package = sorted(m for m in sys.modules if m.split(".")[0] == "cc_transcript")
print(json.dumps([elapsed_ms, sorted(roots - stdlib - {"cc_transcript"}), package]))
"""

PROBE = """
import json, sys
import cc_transcript.ids, cc_transcript.tools
roots = sorted({m.split(".")[0] for m in sys.modules if not m.startswith("_")})
print(json.dumps(roots))
"""

MINING_PROBE = """
import json, sys
import cc_transcript.mining.signals
roots = sorted({m.split(".")[0] for m in sys.modules if not m.startswith("_")})
print(json.dumps(roots))
"""


def test_ids_and_tools_import_stdlib_only() -> None:
    out = subprocess.run([sys.executable, "-c", PROBE], capture_output=True, text=True, check=True)
    loaded = set(json.loads(out.stdout))
    assert not loaded & set(HEAVY), f"hot-path import pulled heavy deps: {sorted(loaded & set(HEAVY))}"
    assert "cc_transcript" in loaded


def test_mining_signals_import_stays_tree_sitter_free() -> None:
    out = subprocess.run([sys.executable, "-c", MINING_PROBE], capture_output=True, text=True, check=True)
    loaded = set(json.loads(out.stdout))
    grammar = loaded & {"tree_sitter", "tree_sitter_bash"}
    assert not grammar, f"mining import pulled the bash grammar: {sorted(grammar)}"
    assert "cc_transcript" in loaded


def test_root_import_loads_the_root_alone_within_budget() -> None:
    runs = [
        json.loads(
            subprocess.run([sys.executable, "-c", ROOT_PROBE], capture_output=True, text=True, check=True).stdout
        )
        for _ in range(3)
    ]
    for _, extras, package in runs:
        assert not extras, f"root import pulled non-stdlib modules: {extras}"
        assert package == ["cc_transcript"], f"root import loaded submodules: {package}"
    assert (best := min(ms for ms, _, _ in runs)) < IMPORT_BUDGET_MS, f"root import took {best:.2f}ms"


def test_lazy_root_resolves_exports_on_demand() -> None:
    probe = """
import json, sys
import cc_transcript
before = "cc_transcript.parser" in sys.modules
ref = cc_transcript.EventRef
after_ids = "cc_transcript.parser" in sys.modules
parse = cc_transcript.parse
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
