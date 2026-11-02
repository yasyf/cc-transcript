from __future__ import annotations

from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import runpy
import shlex
import sqlite3
import sys
import tempfile


assert os.environ.get("GITHUB_ACTIONS") == "true"
out = Path(sys.argv[1]).resolve()
assert Path(os.environ["RUNNER_TEMP"]).resolve() in out.parents
out.mkdir(mode=0o700)
old_cli = Path(sys.argv[2]).resolve()
old_bootstrap = Path(sys.argv[3]).resolve()
old_proof = json.loads(old_bootstrap.read_text())
assert old_proof["status"] == "PASS_PRIVATE_OFFICIAL35_BASELINE_BOOTSTRAP"
assert old_proof["operator_sha256"] == "96027dba4c910e156b3e12f5e13703e3aa032eef6b124657bd7aed0d2e4f9b13"
assert old_cli == old_bootstrap.parent / "work/sdk35/bin/cc-transcript"
assert old_cli.is_file()
PrivateWatchman = runpy.run_path(str(Path(__file__).with_name("private_watchman.py")))["PrivateWatchman"]
controls = []
with PrivateWatchman(out / "service") as server:
    os.environ["WATCHMAN_SOCK"] = str(server.socket)
    sys.argv = [str(Path(__file__).with_name("private_list_index_ci.py")), str(out / "cases")]
    case = runpy.run_path(sys.argv[0])
    capture = case["capture"]
    case_out = case["out"]

    def pair(label: str, arguments: list[str], home: Path, expected_code: int = 0) -> str:
        capture(label + "-old", arguments, home, executable=old_cli, expected_code=expected_code)
        current = capture(label + "-new", arguments, home, expected_code=expected_code)
        for suffix in ("stdout", "stderr"):
            assert (case_out / f"{label}-old.{suffix}").read_bytes() == (case_out / f"{label}-new.{suffix}").read_bytes(), (label, suffix)
        return current

    def cache_state(path: Path) -> tuple[int, str, int]:
        with closing(sqlite3.connect(path)) as connection:
            return connection.execute("SELECT revision,clock,total FROM listing_state WHERE singleton=1").fetchone()

    def replace_clock(path: Path, clock: str) -> None:
        with closing(sqlite3.connect(path)) as connection:
            with connection:
                connection.execute("UPDATE listing_state SET clock=? WHERE singleton=1", (clock,))

    for population in (32, 10000):
        with tempfile.TemporaryDirectory(prefix=f"continuity-{population}-", dir=out) as directory:
            home = Path(directory)
            (home / "tmp").mkdir()
            root = home / ".claude/projects"
            files = []
            for position in range(population):
                path = root / f"project-{position // 100:03d}" / f"session-{position:05d}.jsonl"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
                stamp = 1_735_689_600 + position
                os.utime(path, ns=(stamp * 1_000_000_000, stamp * 1_000_000_000))
                files.append(path)
            arguments = ["--limit", "25", "--json"]
            pair(f"n{population}-continuity-cold", arguments, home)
            identity = hashlib.sha256(os.fsencode(root.resolve())).hexdigest()
            database = home / ".cache/cc-transcript/list-v1" / identity / "listing.sqlite3"
            original_clock = cache_state(database)[1]
            files[0].write_bytes(b"old-inplace-append")
            stamp = 1_735_689_600 + population + 10
            os.utime(files[0], ns=(stamp * 1_000_000_000, stamp * 1_000_000_000))
            pair(f"n{population}-continuity-append", arguments, home)
            replace_clock(database, original_clock)
            pair(f"n{population}-continuity-replay", arguments, home)
            assert cache_state(database)[2] == population
            foreign = home / "foreign-root"
            foreign.mkdir()
            server.request(["watch", str(foreign)])
            foreign_clock = server.request(["query", str(foreign), {"fields": ["name"]}])["clock"]
            files[1].unlink()
            replace_clock(database, foreign_clock)
            fresh = server.request(["query", str(root), {"since": foreign_clock, "fields": ["name"]}])
            assert fresh["is_fresh_instance"] is True
            pair(f"n{population}-continuity-foreign-clock", arguments, home)
            assert cache_state(database)[2] == population - 1
            before_recrawl = cache_state(database)[1]
            files[2].unlink()
            recrawl_ack = server.request(["debug-recrawl", str(root)])
            recrawl = server.request(["query", str(root), {"since": before_recrawl, "fields": ["name", "exists"]}])
            deleted = files[2].relative_to(root).as_posix()
            if recrawl["is_fresh_instance"]:
                assert not any(entry["name"] == deleted and entry["exists"] for entry in recrawl["files"])
            else:
                assert {"name": deleted, "exists": False} in recrawl["files"]
            (out / f"n{population}-recrawl-change-observation.json").write_text(json.dumps({
                "purpose": "change-continuity after a recrawl request",
                "ack": recrawl_ack,
                "query_clock": recrawl["clock"],
                "is_fresh_instance": recrawl["is_fresh_instance"],
                "warning": recrawl.get("warning"),
                "deleted": deleted,
                "completion_proof": "not inferred from the ACK or cookie",
            }, sort_keys=True) + "\n")
            pair(f"n{population}-continuity-recrawl-change", arguments, home)
            assert cache_state(database)[2] == population - 2
            files[3].unlink()
            server.stop()
            server.start()
            pair(f"n{population}-continuity-restart", arguments, home)
            assert cache_state(database)[2] == population - 3
            external = home / "external.jsonl"
            external.touch()
            os.utime(external, ns=(1_000_000_000, 1_000_000_000))
            link = root / "old-link.jsonl"
            link.symlink_to(external)
            pair(f"n{population}-continuity-low-link", arguments, home)
            os.utime(external, ns=((stamp + 1) * 1_000_000_000, (stamp + 1) * 1_000_000_000))
            top = pair(f"n{population}-continuity-external-append", arguments, home)
            assert json.loads(top.splitlines()[0])["path"] == str(link)
            missing_target = home / "initially-missing.jsonl"
            dangling = root / "dangling.jsonl"
            dangling.symlink_to(missing_target)
            pair(f"n{population}-continuity-dangling", arguments, home)
            missing_target.touch()
            os.utime(missing_target, ns=((stamp + 2) * 1_000_000_000, (stamp + 2) * 1_000_000_000))
            top = pair(f"n{population}-continuity-new-target", arguments, home)
            assert json.loads(top.splitlines()[0])["path"] == str(dangling)
            negative = root / "pre-epoch.jsonl"
            negative.touch()
            os.utime(negative, ns=(-100_000_000, -100_000_000))
            pair(f"n{population}-continuity-pre-epoch", ["--all", "--json"], home)
            for name in ("a", "a.b"):
                path = root / name / "tie.jsonl"
                path.parent.mkdir()
                path.touch()
                os.utime(path, ns=((stamp + 3) * 1_000_000_000, (stamp + 3) * 1_000_000_000))
            top = pair(f"n{population}-continuity-path-ties", arguments, home)
            assert [json.loads(line)["path"] for line in top.splitlines()[:2]] == [str(root / "a/tie.jsonl"), str(root / "a.b/tie.jsonl")]
            pair(f"n{population}-continuity-project", ["--project", "a.b", "--json"], home)
            pair(f"n{population}-continuity-contains", ["--contains", "dangling", "--json"], home)
            unreadable = root / "project-000"
            assert os.getuid() != 0
            try:
                unreadable.chmod(0)
                pair(f"n{population}-continuity-unreadable-project", ["--limit", "25"], home)
            finally:
                unreadable.chmod(0o700)
            pair(f"n{population}-continuity-restored-project", ["--limit", "25"], home)
            unavailable = capture(f"n{population}-continuity-unavailable", arguments, home,
                                  expected_code=1, watchman_socket=home / "absent-socket")
            assert unavailable == ""
            stub_directory = home / "bin"
            stub_directory.mkdir()
            stub = stub_directory / "watchman"
            stub.write_text("#!/bin/sh\nexec " + shlex.quote(str(server.binary)) + " " + shlex.quote("--sockname=" + str(server.socket)) + ' "$@"\n')
            stub.chmod(0o700)
            label = f"n{population}-continuity-private-routed-discovery"
            capture(label + "-old", arguments, home, executable=old_cli)
            capture(label + "-new", arguments, home, watchman_socket=None, search_path=str(stub_directory))
            for suffix in ("stdout", "stderr"):
                assert (case_out / f"{label}-old.{suffix}").read_bytes() == (case_out / f"{label}-new.{suffix}").read_bytes()
            stub.write_text('#!/bin/sh\nprintf \'%s\\n\' "$@" > "$HOME/discovery-argv"\nexit 73\n')
            stub.chmod(0o700)
            undiscovered = capture(f"n{population}-continuity-no-autostart", arguments, home,
                                   expected_code=1, watchman_socket=None, search_path=str(stub_directory))
            assert undiscovered == ""
            assert (home / "discovery-argv").read_text().splitlines() == [
                "--no-spawn", "--no-local", "--output-encoding=bser-v2", "get-sockname"]
            with closing(sqlite3.connect(database)) as connection:
                with connection:
                    connection.execute("PRAGMA user_version=2")
            assert capture(f"n{population}-continuity-schema-refusal", arguments, home, expected_code=1) == ""
            assert "unsupported transcript listing index schema" in (case_out / f"n{population}-continuity-schema-refusal.stderr").read_text()
            with closing(sqlite3.connect(database)) as connection:
                with connection:
                    connection.execute("PRAGMA user_version=1")
            pair(f"n{population}-continuity-schema-restored", arguments, home)
            assert server.request(["watch-del", str(root)])["watch-del"] is True
            root.rename(home / "retained-old-root")
            root.mkdir()
            replacement = root / "replacement.jsonl"
            replacement.touch()
            os.utime(replacement, ns=((stamp + 4) * 1_000_000_000, (stamp + 4) * 1_000_000_000))
            replaced = pair(f"n{population}-continuity-root-replacement", arguments, home)
            assert [json.loads(line)["path"] for line in replaced.splitlines()] == [str(replacement)]
            assert cache_state(database)[2] == 1
            controls.append({"population": population, "fresh": fresh["is_fresh_instance"],
                             "recrawl_fresh": recrawl["is_fresh_instance"], "replay_population": population,
                             "current_cache": cache_state(database)})
        assert not home.exists()
    rows = case["rows"]
    assert len(rows) == 108
    assert all(row["state"] == "REAPED" and row["exit_code"] == row["expected_exit_code"] for row in rows)
assert all(row["state"] == "REAPED" and row["exit_code"] == 0 for row in server.children)
joined_rows = []
for row in rows:
    files = {}
    for suffix in ("stdout", "stderr"):
        path = case_out / f"{row['label']}.{suffix}"
        raw = path.read_bytes()
        files[suffix] = {"path": str(path), "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
    joined_rows.append(row | {"raw_files": files})
server_generations = [row for row in server.children if "generation" in row]
control_children = [row for row in server.children if "sequence" in row]
server_user = sum(row["owned_interval_child_user_seconds"] for row in server_generations) - sum(row["user_seconds"] for row in rows) - sum(row["user_seconds"] for row in control_children)
server_system = sum(row["owned_interval_child_system_seconds"] for row in server_generations) - sum(row["system_seconds"] for row in rows) - sum(row["system_seconds"] for row in control_children)
report = {"status": "PASS_PRIVATE_INDEXED_LIST_CONTINUITY_SUBSET", "checkout_sha": os.environ["GITHUB_SHA"],
          "old_official": old_proof, "controls": controls, "list_children": joined_rows,
          "private_server_children": server.children,
          "private_server_cpu_residual": {"user_seconds": server_user, "system_seconds": server_system,
                                          "scope": "Summed stopped-server child-accounting intervals minus all recorded list and control child CPU; diagnostic aggregate across both populations, restarts and recovery controls."},
          "limits": ["Known ignored-prefix coverage compatibility remains unresolved; this is not full product acceptance.",
                     "debug-recrawl exercises fresh-domain replacement; actual OS kernel overflow is not induced.",
                     "Timing/rusage is diagnostic; no fixed gain, syscall-count, physical IO or cold-cache claim."]}
(out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report), flush=True)
