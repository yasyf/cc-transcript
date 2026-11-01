from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess

BASE = "0b5e130b7db1343f4948c8a4235a951ea0332d6c"
SNAPSHOT = Path("rust/crates/core/src/snapshot.rs")
CONTROLS = Path("rust/crates/core/src/snapshot_skill_demand_controls.rs")
ANCHOR = '    include!("snapshot_query_hydration_tests.rs");'
APPEND = '\n    include!("snapshot_skill_demand_controls.rs");'
TESTS = (
    "snapshot::tests::typed_skills_cold_codex_has_no_generic_lowering",
    "snapshot::tests::typed_skills_capability_does_not_answer_tool_queries",
)
MARKERS = ("TYPED_SKILLS_NO_CODEX_LOWERING", "TYPED_SKILLS_CAPABILITY_NO_INDEX")


def git(repo: Path, *args: str) -> str:
    argv = ["git", "-C", str(repo), *args]
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, text=True)
    stdout, _ = process.communicate()
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, argv, output=stdout)
    return stdout.strip()


def main() -> None:
    os.umask(0o077)
    repo = Path(__file__).resolve().parents[1]
    output = Path(os.environ["RUNNER_TEMP"]) / "typed-skills-evidence"
    output.mkdir(mode=0o700, exist_ok=True)
    report_path = output / "old-source.json"
    report: dict[str, object] = {
        "status": "OLD_SOURCE_CONTROLS_PENDING",
        "base": BASE,
        "current_checkout": git(repo, "rev-parse", "HEAD"),
        "current_tree": git(repo, "rev-parse", "HEAD^{tree}"),
        "controls_sha256": hashlib.sha256((repo / CONTROLS).read_bytes()).hexdigest(),
    }
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    git(repo, "fetch", "--no-tags", "origin", BASE)
    old = output / "old-production"
    git(repo, "worktree", "add", "--detach", str(old), BASE)
    original = (old / SNAPSHOT).read_text()
    if original.count(ANCHOR) != 1:
        raise RuntimeError("old production test anchor differs")
    (old / SNAPSHOT).write_text(original.replace(ANCHOR, ANCHOR + APPEND))
    (old / CONTROLS).write_bytes((repo / CONTROLS).read_bytes())
    if (old / SNAPSHOT).read_text().replace(APPEND, "", 1) != original:
        raise RuntimeError("old production overlay changes more than the test include")
    report |= {
        "base_tree": git(old, "rev-parse", "HEAD^{tree}"),
        "base_snapshot_sha256": hashlib.sha256(original.encode()).hexdigest(),
        "overlay_snapshot_sha256": hashlib.sha256((old / SNAPSHOT).read_bytes()).hexdigest(),
    }
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    argv = ["cargo", "test", "--locked", "-p", "cc-transcript-core", "--no-default-features", "--lib",
            "typed_skills_", "--", "--nocapture", "--test-threads=2"]
    env = os.environ | {"CARGO_BUILD_JOBS": "2", "CARGO_TARGET_DIR": str(repo / "target")}
    log_path = output / "old-negative.log"
    with log_path.open("w") as log:
        process = subprocess.Popen(argv, cwd=old, env=env, stdout=log, stderr=subprocess.STDOUT)
        returncode = process.wait()
    log = log_path.read_text()
    report |= {"old_pid": process.pid, "old_returncode": returncode, "expected_tests": TESTS,
               "markers": {marker: log.count(marker) for marker in MARKERS}}
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    if git(old, "diff", "--name-only").splitlines() != [str(SNAPSHOT)]:
        raise RuntimeError("old production tracked sources changed outside the test include")
    if git(old, "ls-files", "--others", "--exclude-standard").splitlines() != [str(CONTROLS)]:
        raise RuntimeError("old production has an unexpected untracked overlay")
    if (old / SNAPSHOT).read_text().replace(APPEND, "", 1) != original:
        raise RuntimeError("old production source changed during the native controls")
    valid = (
        returncode == 101
        and "running 2 tests" in log
        and "test result: FAILED. 0 passed; 2 failed;" in log
        and all(f"test {name} ... FAILED" in log for name in TESTS)
        and all(marker in log for marker in MARKERS)
    )
    report["status"] = "OLD_PRODUCTION_NEGATIVE_CONTROLS_PASS" if valid else "OLD_PRODUCTION_NEGATIVE_CONTROLS_FAIL"
    report["tracked_production_inverse_exact"] = True
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    if not valid:
        raise RuntimeError("old production did not fail both exact Skills controls")
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
