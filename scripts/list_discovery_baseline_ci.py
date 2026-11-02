import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import urllib.request


assert os.environ.get("GITHUB_ACTIONS") == "true"
assert sys.version_info[:2] == (3, 13)
os.umask(0o077)
root = Path(sys.argv[1]).resolve()
root.mkdir()
work = root / "work"
receipts = root / "receipts"
work.mkdir()
receipts.mkdir()
inputs_path = Path(__file__).with_name("list_baseline_sdk35_inputs.json")
inputs = json.loads(inputs_path.read_text())
assert inputs["version"] == "14.45.35"
assert inputs["source"] == "c001a9d8e66c86769608ed17d49fe825885e3e8b"
assert inputs["official_receipt_sha256"] == "7c867686801237602eca0cf72e62d338b2e978a97310d5ca32ada535530c22ab"
tags = {("Darwin", "arm64"): "macosx_11_0_arm64.whl",
        ("Darwin", "x86_64"): "macosx_11_0_x86_64.whl",
        ("Linux", "aarch64"): "manylinux_2_28_aarch64.whl",
        ("Linux", "x86_64"): "manylinux_2_28_x86_64.whl"}
tag = tags[(platform.system(), platform.machine())]
artifact = next(item for item in inputs["wheels"] if item["filename"].endswith(tag))
assert Path(artifact["filename"]).name == artifact["filename"]
assert artifact["url"].startswith("https://files.pythonhosted.org/")
wheel = work / artifact["filename"]
digest = hashlib.sha256()
downloaded = 0
with urllib.request.urlopen(artifact["url"]) as response, wheel.open("xb") as output:
    while block := response.read(min(1048576, artifact["size"] - downloaded + 1)):
        downloaded += len(block)
        assert downloaded <= artifact["size"]
        digest.update(block)
        output.write(block)
assert downloaded == artifact["size"] and digest.hexdigest() == artifact["sha256"]
children = []


def run(label, argv):
    with (receipts / f"{label}.stdout").open("xb") as stdout, (receipts / f"{label}.stderr").open("xb") as stderr:
        child = subprocess.Popen(argv, cwd=work, stdout=stdout, stderr=stderr)
        try:
            (receipts / f"{label}.held.json").write_text(json.dumps({"pid": child.pid, "ppid": os.getpid(), "argv": argv}))
            code = child.wait()
        finally:
            child.wait()
    row = {"label": label, "pid": child.pid, "ppid": os.getpid(), "argv": argv,
           "state": "REAPED", "exit_code": code,
           "stdout_sha256": hashlib.sha256((receipts / f"{label}.stdout").read_bytes()).hexdigest(),
           "stderr_sha256": hashlib.sha256((receipts / f"{label}.stderr").read_bytes()).hexdigest()}
    (receipts / f"{label}.exit.json").write_text(json.dumps(row, indent=2) + "\n")
    children.append(row)
    assert code == 0


venv = work / "sdk35"
run("venv", [sys.executable, "-I", "-B", "-m", "venv", "--without-pip", str(venv)])
python = venv / "bin/python"
uv = shutil.which("uv")
assert uv is not None
run("install", [uv, "pip", "install", "--python", str(python), "--no-build", str(wheel)])
operator = Path(__file__).with_name("private_list_scaling_baseline.py")
run("baseline", [str(python), "-I", "-B", str(operator), str(root / "results")])
result = json.loads((root / "results/result.json").read_text())
assert result["status"] == "PASS_PRIVATE_OFFICIAL35_BASELINE_CHARACTERIZATION"
assert len(result["rows"]) == 22 and all(row["state"] == "REAPED" and row["exit_code"] == 0 for row in result["rows"])
proof = {"status": "PASS_PRIVATE_OFFICIAL35_BASELINE_BOOTSTRAP", "sdk_version": inputs["version"],
         "sdk_source": inputs["source"], "wheel": artifact, "wheel_sha256_verified": digest.hexdigest(),
         "operator_sha256": hashlib.sha256(operator.read_bytes()).hexdigest(),
         "input_sha256": hashlib.sha256(inputs_path.read_bytes()).hexdigest(),
         "github_sha": os.environ["GITHUB_SHA"], "github_run_id": os.environ["GITHUB_RUN_ID"],
         "python": sys.version, "platform": [platform.system(), platform.machine()], "children": children,
         "limits": ["Hosted private baseline only; no host activation or Watchman requirement.",
                    "Normal declared runtime dependencies resolve in the private venv; no extras or model installation.",
                    "Direct controller children and all22 direct list cases are waited; uv internal descendants are not independently inventoried."]}
(root / "bootstrap_result.json").write_text(json.dumps(proof, indent=2) + "\n")
print(json.dumps(proof), flush=True)
