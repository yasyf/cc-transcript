from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

BASE = "b4468afb7259e2fcf2afc8bbd5e9889507355cd8"
SNAPSHOT = Path("rust/crates/core/src/snapshot.rs")
CONTROLS = Path("rust/crates/core/src/snapshot_graph_anchor_tests.rs")
ANCHOR = '    include!("snapshot_common_borrowed_parent_tests.rs");'
APPEND = '\n    include!("snapshot_graph_anchor_tests.rs");'
WALK = """impl PreparedGraph {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
"""
PROBE = """        #[cfg(test)]
        tests::record_prepared_graph_input_anchors(self.sources.len(), self.sidechain_dirs.len());
"""
TESTS = (
    "prepared_graph_anchor_cost_zero",
    "prepared_graph_anchor_cost_small",
    "prepared_graph_anchor_cost_923",
    "prepared_graph_anchor_walks_do_not_hold_shared_state",
)
ALIAS = "prepared_graph_anchor_owners_preserve_slice_aliases"
MARKERS = (
    "prepared graph repeats immutable roster byte accounting",
    "prepared graph counts roster bytes under shared state",
)
ROW = re.compile(
    r"PREPARED_GRAPH_ANCHOR_COST sources=(\d+) directories=(\d+) publication_calls=(\d+) publication_fields=(\d+) publication_state_held=(\d+) release_calls=(\d+) release_fields=(\d+) release_state_held=(\d+)"
)


def git(repo: Path, *args: str) -> str:
    argv = ["git", "-C", str(repo), *args]
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, text=True)
    stdout, _ = process.communicate()
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, argv, output=stdout)
    return stdout.strip()


def rows(log: str) -> list[list[int]]:
    return sorted([list(map(int, match)) for match in ROW.findall(log)])


def expected_rows(old: bool) -> list[list[int]]:
    return sorted(
        [count, count + 1, 4 if old else 1, (4 if old else 1) * (2 * count + 1),
         4 if old else 0, 1 if old else 0, 2 * count + 1 if old else 0, 1 if old else 0]
        for count in (0, 32, 923, 923)
    )


def main() -> None:
    assert os.environ.get("GITHUB_ACTIONS") == "true", "native controls run only in CI"
    os.umask(0o077)
    repo = Path(__file__).resolve().parents[1]
    output = Path(os.environ["RUNNER_TEMP"]) / "graph-anchor-evidence"
    output.mkdir(mode=0o700, exist_ok=True)
    report_path = output / "old-source.json"
    report: dict[str, object] = {
        "status": "GRAPH_ANCHOR_CONTROLS_PENDING",
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
    assert hashlib.sha256(original.encode()).hexdigest() == "b1c40920c1cd3460429f531c992b668a4b5f2f4c6832d049c76531fc4d62e90e"
    assert original.count(ANCHOR) == original.count(WALK) == 1
    overlay = original.replace(ANCHOR, ANCHOR + APPEND).replace(WALK, WALK + PROBE)
    (old / SNAPSHOT).write_text(overlay)
    (old / CONTROLS).write_bytes((repo / CONTROLS).read_bytes())
    assert overlay.replace(APPEND, "", 1).replace(PROBE, "", 1) == original
    report |= {
        "base_tree": git(old, "rev-parse", "HEAD^{tree}"),
        "base_snapshot_sha256": hashlib.sha256(original.encode()).hexdigest(),
        "overlay_snapshot_sha256": hashlib.sha256(overlay.encode()).hexdigest(),
        "tracked_production_inverse_exact": True,
    }
    argv = ["cargo", "test", "--locked", "-p", "cc-transcript-core", "--no-default-features", "--lib",
            "prepared_graph_anchor_", "--", "--nocapture", "--test-threads=2"]
    env = os.environ | {"CARGO_BUILD_JOBS": "2", "CARGO_TARGET_DIR": str(repo / "target")}
    log_path = output / "old-negative.log"
    with log_path.open("w") as log:
        process = subprocess.Popen(argv, cwd=old, env=env, stdout=log, stderr=subprocess.STDOUT)
        returncode = process.wait()
    log = log_path.read_text()
    positive = (output / "positive.log").read_text()
    old_rows = rows(log)
    current_rows = rows(positive)
    report |= {"old_pid": process.pid, "old_returncode": returncode, "old_rows": old_rows,
               "current_rows": current_rows, "markers": {marker: log.count(marker) for marker in MARKERS}}
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    assert git(old, "diff", "--name-only").splitlines() == [str(SNAPSHOT)]
    assert git(old, "ls-files", "--others", "--exclude-standard").splitlines() == [str(CONTROLS)]
    assert (old / SNAPSHOT).read_text().replace(APPEND, "", 1).replace(PROBE, "", 1) == original
    valid = (
        returncode == 101
        and "running 5 tests" in log and "test result: FAILED. 1 passed; 4 failed;" in log
        and all(f"test snapshot::tests::{name} ... FAILED" in log for name in TESTS)
        and f"test snapshot::tests::{ALIAS} ... ok" in log
        and all(marker in log for marker in MARKERS)
        and "running 5 tests" in positive and "test result: ok. 5 passed; 0 failed;" in positive
        and all(f"test snapshot::tests::{name} ... ok" in positive for name in (*TESTS, ALIAS))
        and old_rows == expected_rows(True) and current_rows == expected_rows(False)
    )
    report["status"] = "PAIRED_GRAPH_ANCHOR_CONTROLS_PASS" if valid else "PAIRED_GRAPH_ANCHOR_CONTROLS_FAIL"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    if not valid:
        raise RuntimeError("compiled old5/current1 anchor walks and State exclusion controls did not match")
    print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
