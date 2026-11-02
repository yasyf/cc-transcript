from __future__ import annotations

import email.parser
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tomllib
import zipfile


assert os.environ.get("GITHUB_ACTIONS") == "true" and sys.version_info[:2] == (3, 13)
os.umask(0o077)
repository = Path(__file__).resolve().parents[1]
out = Path(sys.argv[1]).resolve()
old = Path(sys.argv[2]).resolve()
assert Path(os.environ["RUNNER_TEMP"]).resolve() in out.parents
out.mkdir(mode=0o700)
receipts = out / "receipts"
receipts.mkdir()
wheels = tuple((repository / "dist").glob("*.whl"))
assert len(wheels) == 1
wheel = wheels[0]
version = tomllib.loads((repository / "pyproject.toml").read_text())["project"]["version"]
with zipfile.ZipFile(wheel) as archive:
    metadata_name = next(name for name in archive.namelist() if name.endswith(".dist-info/METADATA"))
    metadata = email.parser.Parser().parsestr(archive.read(metadata_name).decode())
    assert metadata["Name"] == "cc-transcript" and metadata["Version"] == version
    native_hash = hashlib.sha256(archive.read("cc_transcript/_native.abi3.so")).hexdigest()
children = []


def run(label: str, arguments: list[str]) -> None:
    with (receipts / f"{label}.stdout").open("xb") as stdout, (receipts / f"{label}.stderr").open("xb") as stderr:
        child = subprocess.Popen(arguments, cwd=repository, stdout=stdout, stderr=stderr)
        record = {"label": label, "pid": child.pid, "ppid": os.getpid(), "argv": arguments}
        try:
            (receipts / f"{label}.held.json").write_text(json.dumps(record))
            code = child.wait()
        finally:
            child.wait()
    record.update(state="REAPED", exit_code=code)
    (receipts / f"{label}.exit.json").write_text(json.dumps(record))
    children.append(record)
    assert code == 0, record


venv = out / "candidate"
run("venv", [sys.executable, "-I", "-B", "-m", "venv", "--without-pip", str(venv)])
python = str(venv / "bin/python")
uv = shutil.which("uv")
assert uv is not None
run("install", [uv, "pip", "install", "--python", python, "--no-build", str(wheel)])
run("continuity", [python, "-I", "-B", str(repository / "scripts/private_list_continuity_ci.py"), str(out / "continuity"), str(old / "work/sdk35/bin/cc-transcript"), str(old / "bootstrap_result.json")])
run("goldens", [python, "-I", "-B", str(repository / "scripts/private_watchman.py"), str(out / "golden-service"), "--", python, "-I", "-B", str(repository / "scripts/private_list_goldens_ci.py"), str(out / "goldens")])
result = json.loads((out / "continuity/result.json").read_text())
assert result["status"] == "PASS_PRIVATE_INDEXED_LIST_CONTINUITY_SUBSET"
native = json.loads((out / "continuity/cases/result.json").read_text())
assert native["native_sha256"] == native_hash
proof = {"status": "PASS_PRIVATE_CANDIDATE_BUILD_AND_CASE_CHARACTERIZATION", "checkout_sha": os.environ["GITHUB_SHA"],
         "informational_version": version, "wheel_sha256": hashlib.sha256(wheel.read_bytes()).hexdigest(),
         "native_sha256": native_hash, "Cargo_lock_sha256": hashlib.sha256((repository / "Cargo.lock").read_bytes()).hexdigest(),
         "python": sys.version, "platform": [platform.system(), platform.machine()], "children": children,
         "continuity_result_sha256": hashlib.sha256((out / "continuity/result.json").read_bytes()).hexdigest(),
         "limits": ["CI-built candidate, not an official release or installed-fix acceptance.",
                    "Known ignored-prefix coverage compatibility remains unresolved.",
                    "Recrawl control is not an induced OS kernel overflow.",
                    "Native timings include warmable caches and provide no universal gain or physical-byte attribution."]}
(out / "result.json").write_text(json.dumps(proof, indent=2) + "\n")
print(json.dumps(proof), flush=True)
