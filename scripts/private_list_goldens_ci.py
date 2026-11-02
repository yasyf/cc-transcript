from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import runpy
import shutil
import sys


assert os.environ.get("GITHUB_ACTIONS") == "true"
scope = runpy.run_path(str(Path(__file__).with_name("record_cli_golden.py")))
cli = Path(sys.executable).with_name("cc-transcript")
assert cli.is_file()
scope["run"].__globals__["CLI"] = cli
out = Path(sys.argv[1]).resolve()
out.mkdir(mode=0o700)
committed = scope["GOLDEN_DIR"]
shutil.copyfile(committed / "manifest.json", out / "manifest.json")
names = frozenset({"list", "list_json", "list_codex", "list_codex_contains", "list_all", "list_all_json", "list_all_invalid_claude_root", "list_all_invalid_codex_root", "list_codex_invalid_root"})
scope["record"](out, names)
actual = json.loads((out / "manifest.json").read_text())
expected = json.loads((committed / "manifest.json").read_text())
for name in sorted(names):
    for suffix in ("out", "err"):
        assert (out / f"{name}.{suffix}").read_bytes() == (committed / f"{name}.{suffix}").read_bytes(), (name, suffix)
    assert actual[name] == expected[name], name
print(json.dumps({"status": "PASS_EXISTING_NINE_LIST_GOLDENS", "checkout_sha": os.environ["GITHUB_SHA"], "cases": sorted(names), "interpreter": sys.executable, "cli": str(cli), "cli_sha256": hashlib.sha256(cli.read_bytes()).hexdigest()}))
