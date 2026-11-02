from __future__ import annotations

from contextlib import AbstractContextManager
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import time
from typing import Self


class PrivateWatchman(AbstractContextManager):
    def __init__(self, directory: Path) -> None:
        assert os.environ.get("GITHUB_ACTIONS") == "true"
        self.directory = directory.resolve()
        program = shutil.which("watchman")
        assert program is not None
        self.binary = Path(program).resolve()
        assert self.binary.is_file()
        self.directory.mkdir(mode=0o700)
        self.socket = self.directory / "socket"
        self.environment = os.environ | {"HOME": str(self.directory), "WATCHMAN_SOCK": str(self.socket)}
        self.children: list[dict[str, object]] = []
        self.server: subprocess.Popen[bytes] | None = None
        self.console = None
        self.counter = 0
        self.generation = 0

    def capture(self, label: str, command: list[str], input_bytes: bytes | None = None,
                environment: dict[str, str] | None = None) -> bytes:
        self.counter += 1
        stem = self.directory / f"{self.counter:03d}-{label}"
        source = stem.with_suffix(".stdin")
        source.write_bytes(input_bytes or b"")
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        with source.open("rb") as stdin, stem.with_suffix(".stdout").open("xb") as stdout, stem.with_suffix(".stderr").open("xb") as stderr:
            child = subprocess.Popen(command, stdin=stdin, stdout=stdout, stderr=stderr,
                                     env=self.environment if environment is None else environment)
            record = {"pid": child.pid, "ppid": os.getpid(), "argv": command,
                      "label": label, "sequence": self.counter}
            try:
                stem.with_suffix(".held.json").write_text(json.dumps(record))
                code = child.wait()
            finally:
                child.wait()
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        record.update(state="REAPED", exit_code=code,
                      user_seconds=after.ru_utime - before.ru_utime,
                      system_seconds=after.ru_stime - before.ru_stime, raw_files={
            suffix: {"path": str(stem.with_suffix("." + suffix)),
                     "sha256": hashlib.sha256(stem.with_suffix("." + suffix).read_bytes()).hexdigest()}
            for suffix in ("stdout", "stderr")})
        stem.with_suffix(".exit.json").write_text(json.dumps(record))
        self.children.append(record)
        assert code == 0, record
        return stem.with_suffix(".stdout").read_bytes()

    def request(self, command: list[object]) -> dict[str, object]:
        output = self.capture("request", [str(self.binary), "--no-spawn", "--no-local", "--sockname=" + str(self.socket), "--output-encoding=json", "--json-command"], json.dumps(command).encode())
        response = json.loads(output)
        assert "error" not in response, response
        return response

    def start(self) -> None:
        assert self.server is None
        self.generation += 1
        self.child_usage_start = resource.getrusage(resource.RUSAGE_CHILDREN)
        self.console = (self.directory / f"server-{self.generation:03d}.console").open("xb")
        argv = [str(self.binary), "--foreground", "--sockname=" + str(self.socket), "--statefile=" + str(self.directory / "state"), "--pidfile=" + str(self.directory / "pid"), "--logfile=" + str(self.directory / "log"), "--log-level=2", "--no-save-state"]
        self.server = subprocess.Popen(argv, stdout=self.console, stderr=subprocess.STDOUT, env=self.environment, start_new_session=True)
        (self.directory / f"server-{self.generation:03d}.held.json").write_text(json.dumps({"pid": self.server.pid, "ppid": os.getpid(), "argv": argv, "binary_sha256": hashlib.sha256(self.binary.read_bytes()).hexdigest()}))
        deadline = time.monotonic() + 10
        while not self.socket.exists() and time.monotonic() < deadline:
            assert self.server.poll() is None
            time.sleep(0.1)
        assert self.socket.exists()
        assert os.getpgid(self.server.pid) == self.server.pid
        assert self.request(["get-pid"])["pid"] == self.server.pid

    def stop(self) -> None:
        assert self.server is not None
        try:
            if self.server.poll() is None:
                assert self.request(["get-pid"])["pid"] == self.server.pid
                self.request(["shutdown-server"])
        finally:
            code = self.server.wait()
            usage = resource.getrusage(resource.RUSAGE_CHILDREN)
            if self.console is not None:
                self.console.close()
            record = {"pid": self.server.pid, "ppid": os.getpid(), "state": "REAPED", "exit_code": code,
                      "generation": self.generation,
                      "owned_interval_child_user_seconds": usage.ru_utime - self.child_usage_start.ru_utime,
                      "owned_interval_child_system_seconds": usage.ru_stime - self.child_usage_start.ru_stime}
            self.children.append(record)
            (self.directory / f"server-{self.generation:03d}.exit.json").write_text(json.dumps(record))
            self.server = None
        assert code == 0

    def __enter__(self) -> Self:
        try:
            self.start()
        except BaseException:
            if self.server is not None:
                self.stop()
            raise
        return self

    def __exit__(self, exception_type: object, exception: object, traceback: object) -> None:
        if self.server is not None:
            self.stop()


if __name__ == "__main__":
    import sys

    assert sys.argv[2] == "--"
    with PrivateWatchman(Path(sys.argv[1])) as server:
        output = server.capture("wrapped", sys.argv[3:], environment=os.environ | {"WATCHMAN_SOCK": str(server.socket)})
        sys.stdout.buffer.write(output)
