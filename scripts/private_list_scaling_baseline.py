import base64
import csv
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import sysconfig
import tempfile
import time


assert os.environ.get("GITHUB_ACTIONS") == "true"
site = Path(sysconfig.get_path("purelib"))
metadata = site / "cc_transcript-14.45.35.dist-info"
assert "Version: 14.45.35\n" in (metadata / "METADATA").read_text()
native = site / "cc_transcript/_native.abi3.so"
native_hash = hashlib.sha256(native.read_bytes()).hexdigest()
native_record = next(row for row in csv.reader((metadata / "RECORD").read_text().splitlines())
                     if row[0] == "cc_transcript/_native.abi3.so")
assert native_record[1] == "sha256=" + base64.urlsafe_b64encode(bytes.fromhex(native_hash)).decode().rstrip("=")
assert int(native_record[2]) == native.stat().st_size
cli = Path(sys.executable).parent / "cc-transcript"
assert "from cc_transcript._native import cli_main" in cli.read_text()
out = Path(sys.argv[1]).resolve()
os.umask(0o077)
out.mkdir()
rows = []


def capture(label, args, home):
    argv = [str(cli), "list", *args]
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "TMPDIR": str(home / "tmp"),
           "TZ": "UTC", "LC_ALL": "C", "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1"}
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter()
    with (out / f"{label}.stdout").open("xb") as stdout, (out / f"{label}.stderr").open("xb") as stderr:
        child = subprocess.Popen(argv, cwd=home, env=env, stdout=stdout, stderr=stderr)
        try:
            (out / f"{label}.held.json").write_text(json.dumps({"pid": child.pid, "ppid": os.getpid(), "argv": argv}))
            code = child.wait()
        finally:
            child.wait()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = {"label": label, "pid": child.pid, "ppid": os.getpid(), "argv": argv,
              "state": "REAPED", "exit_code": code, "wall_seconds": time.perf_counter() - started,
              "user_seconds": after.ru_utime - before.ru_utime,
              "system_seconds": after.ru_stime - before.ru_stime,
              "ru_inblock_delta": after.ru_inblock - before.ru_inblock,
              "ru_oublock_delta": after.ru_oublock - before.ru_oublock}
    (out / f"{label}.exit.json").write_text(json.dumps(result, indent=2) + "\n")
    rows.append(result)
    assert code == 0 and not (out / f"{label}.stderr").read_bytes()
    return (out / f"{label}.stdout").read_text()


def selected(files, limit):
    return sorted(files, key=lambda path: (-files[path][0], path.parts))[:limit]


def check_json(text, files, limit):
    actual = [json.loads(line) for line in text.splitlines()]
    expected = [{"path": str(path), "mtime": float(files[path][0]), "size": files[path][1]}
                for path in selected(files, limit)]
    assert actual == expected


def check_text(text, files, limit, home):
    lines = text.splitlines()
    count = str(len(files)) if len(files) <= limit else f"{limit} of {len(files)}"
    assert lines[-1] == f"{count} transcripts under ~/.claude/projects"
    expected = selected(files, limit)
    assert len(lines) == len(expected) + 1
    for line, path in zip(lines[:-1], expected, strict=True):
        assert line.endswith(" ~/" + str(path.relative_to(home)))


for population in (32, 10000):
    with tempfile.TemporaryDirectory(prefix=f"home-{population}-", dir=out) as directory:
        home = Path(directory)
        (home / "tmp").mkdir()
        root = home / ".claude/projects"
        files = {}
        for index in range(population):
            path = root / f"project-{index // 100:03d}" / f"session-{index:05d}.jsonl"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()
            stamp = 1735689600 + (population if index >= population - 3 else index)
            os.utime(path, ns=(stamp * 1000000000, stamp * 1000000000))
            files[path] = (stamp, 0)
        for name, args, limit in (("default", [], 50), ("limit25", ["--limit", "25"], 25),
                                  ("limit0", ["--limit", "0"], 0)):
            check_text(capture(f"n{population}-{name}-text", args, home), files, limit, home)
            check_json(capture(f"n{population}-{name}-json", [*args, "--json"], home), files, limit)
        old = next(iter(files))
        parent_stamp = old.parent.stat().st_mtime_ns
        old.write_bytes(b"x")
        stamp = 1735689600 + population + 2
        os.utime(old, ns=(stamp * 1000000000, stamp * 1000000000))
        assert old.parent.stat().st_mtime_ns == parent_stamp
        files[old] = (stamp, 1)
        check_json(capture(f"n{population}-old-inplace-append", ["--limit", "25", "--json"], home), files, 25)
        new = root / "project-extra/recent-new.jsonl"
        new.parent.mkdir()
        new.touch()
        stamp += 1
        os.utime(new, ns=(stamp * 1000000000, stamp * 1000000000))
        files[new] = (stamp, 0)
        for change in ("new", "delete-unselected"):
            if change == "delete-unselected":
                victim = next(path for path in files if path not in selected(files, 25))
                victim.unlink()
                del files[victim]
            check_text(capture(f"n{population}-{change}-text", ["--limit", "25"], home), files, 25, home)
            check_json(capture(f"n{population}-{change}-json", ["--limit", "25", "--json"], home), files, 25)
    assert not home.exists()

assert len(rows) == 22 and all(row["state"] == "REAPED" and row["exit_code"] == 0 for row in rows)
report = {"status": "PASS_PRIVATE_OFFICIAL35_BASELINE_CHARACTERIZATION", "version": "14.45.35",
          "native_sha256": native_hash, "native_matches_RECORD": True, "rows": rows,
          "fixture_populations": [32, 10000], "all_fixture_homes_removed": True,
          "limits": ["Diagnostic CPU/wall and rusage block counters only; no fixed performance acceptance.",
                     "No measured metadata-call count, read-byte attribution, physical IO attribution or universal gain claim.",
                     "OS caches are not flushed; fixture creation and native-file verification may warm caches.",
                     "Existing full CLI goldens remain required; this fixture adds exact JSON selection/count/default/zero/ties and old-file append cases."]}
(out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report), flush=True)
