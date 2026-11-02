from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess

BASE = "c001a9d8e66c86769608ed17d49fe825885e3e8b"
SNAPSHOT = Path("rust/crates/core/src/snapshot.rs")
CONTROLS = Path("rust/crates/core/src/snapshot_publication_shared_admission_tests.rs")
ANCHOR = '    include!("snapshot_load_charge_audit_tests.rs");'
APPEND = '\n    include!("snapshot_publication_shared_admission_tests.rs");'
TESTS = (
    "shared_publication_admission_counts_raw_allocations_once_and_reuses_source",
    "shared_publication_admission_reuses_219mb_source_with_other_generations",
)
MARKER = "shared raw publication admission refused:"
ROW = "SHARED_PUBLICATION_ADMISSION "


def git(repo: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def main() -> None:
    assert os.environ.get("GITHUB_ACTIONS") == "true", "native controls run only in CI"
    os.umask(0o077)
    repo = Path(__file__).resolve().parents[1]
    output = Path(os.environ["RUNNER_TEMP"]) / "shared-publication-evidence"
    output.mkdir(mode=0o700, exist_ok=True)
    report_path = output / "old-source.json"
    report = {
        "status": "SHARED_PUBLICATION_CONTROLS_PENDING",
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
    assert hashlib.sha256(original.encode()).hexdigest() == "8250e4802d97891ffcad4d0960a3ead54fae50c6111b4f5bf10ced9b90f48f9e"
    assert original.count(ANCHOR) == 1
    overlay = original.replace(ANCHOR, ANCHOR + APPEND)
    (old / SNAPSHOT).write_text(overlay)
    (old / CONTROLS).write_bytes((repo / CONTROLS).read_bytes())
    assert overlay.replace(APPEND, "", 1) == original
    argv = ["cargo", "test", "--locked", "-p", "cc-transcript-core", "--no-default-features", "--lib", "shared_publication_admission_",
            "--", "--nocapture", "--test-threads=2"]
    env = os.environ | {"CARGO_BUILD_JOBS": "2", "CARGO_TARGET_DIR": str(repo / "target")}
    with (output / "old-negative.log").open("w") as log:
        process = subprocess.Popen(argv, cwd=old, env=env, stdout=log, stderr=subprocess.STDOUT)
        returncode = process.wait()
    negative = (output / "old-negative.log").read_text()
    positive = (output / "positive.log").read_text()
    valid = (
        returncode == 101
        and "running 2 tests" in negative
        and f"test snapshot::tests::{TESTS[0]} ... ok" in negative
        and f"test snapshot::tests::{TESTS[1]} ... FAILED" in negative
        and "test result: FAILED. 1 passed; 1 failed;" in negative
        and negative.count(MARKER) == 1
        and negative.count('Some("retained_limit")') == 1
        and negative.count(ROW) == 1
        and "running 2 tests" in positive
        and all(f"test snapshot::tests::{test} ... ok" in positive for test in TESTS)
        and positive.count(ROW) == 2
    )
    assert git(old, "diff", "--name-only").splitlines() == [str(SNAPSHOT)]
    assert git(old, "ls-files", "--others", "--exclude-standard").splitlines() == [str(CONTROLS)]
    assert (old / SNAPSHOT).read_text().replace(APPEND, "", 1) == original
    report |= {
        "status": "PAIRED_SHARED_PUBLICATION_CONTROLS_PASS" if valid else "PAIRED_SHARED_PUBLICATION_CONTROLS_FAIL",
        "base_tree": git(old, "rev-parse", "HEAD^{tree}"),
        "base_snapshot_sha256": hashlib.sha256(original.encode()).hexdigest(),
        "overlay_snapshot_sha256": hashlib.sha256(overlay.encode()).hexdigest(),
        "tracked_production_inverse_exact": True,
        "old_pid": process.pid,
        "old_returncode": returncode,
        "negative_marker_present": MARKER in negative,
        "old_positive_control": TESTS[0],
        "old_negative_control": TESTS[1],
        "old_positive_rows": [line for line in negative.splitlines() if ROW in line],
        "current_rows": [line for line in positive.splitlines() if ROW in line],
    }
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    if not valid:
        raise RuntimeError("old retained_limit and current zero-read source reuse controls did not match")
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
