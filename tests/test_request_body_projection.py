from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

from cc_transcript.parser import project_request_body


HEADS = ("a" * 40, "b" * 40, "c" * 64)


@pytest.mark.parametrize("text", [
    '{}',
    '{"bypass_rules":true,"sha":"' + HEADS[0] + '"}',
    '{"bypass_rules":true,"bypass_rules":false}',
    '{"bypass_rules":false,"\\u0062ypass_rules":true}',
    '{"nested":{"bypass_rules":true},"bypass_rules":1}',
    '{"bypass_rules":true,"ignored":[NaN,Infinity,-Infinity,"\\ud800"]}',
    '{"bypass_rules":true,"sha":"' + 'a' * 4096 + '"}',
    '{"bypass_rules":true,"sha":"\\ud800"}',
    '{"bypass_rules":true,"sha":"wrong","sha":"' + HEADS[1] + '"}',
    '{"bypass_rules":true,"ignored":{"' + 'x' * 65536 + '":"é🦀"}}',
    '[{"bypass_rules":true}]',
    'null',
    'NaN',
    '{"bypass_rules":true} false',
    '{"bypass_rules":true,}',
    '{"bypass_rules":true,"ignored":NaNx}',
])
def test_request_body_projection_matches_existing_recognition(tmp_path, text):
    path = tmp_path / "request.json"
    path.write_text(text)
    try:
        expected = json.loads(text)
    except ValueError:
        expected = None
    actual = project_request_body(path)
    if not isinstance(expected, dict):
        assert actual is None
        return
    assert actual is not None
    assert actual["bypass_rules"] is (expected.get("bypass_rules") is True)
    for head in HEADS:
        assert (actual["sha"] == head) is (expected.get("sha") == head)


def test_request_body_projection_does_not_publish_incomplete_input(tmp_path):
    path = tmp_path / "request.json"
    for raw in (b'{"bypass_rules":true', b'{"bypass_rules":true,"ignored":"\xff"}', b'{"bypass_rules":true,"ignored":"\xf0\x9f"}'):
        path.write_bytes(raw)
        assert project_request_body(path) is None


@pytest.mark.skipif(not hasattr(os, "mkfifo"), reason="POSIX named pipe control")
def test_request_body_projection_rejects_nonregular_without_blocking(tmp_path):
    fifo = tmp_path / "pipe"
    os.mkfifo(fifo)
    regular = tmp_path / "regular.json"
    regular.write_text('{"bypass_rules":true}')
    symlink = tmp_path / "linked.json"
    symlink.symlink_to(regular)
    script = '''
import json, sys
from cc_transcript.parser import project_request_body
print(json.dumps([project_request_body(path) for path in sys.argv[1:]]))
'''
    completed = subprocess.run([sys.executable, "-c", script, str(fifo), str(tmp_path), str(symlink)], check=True, capture_output=True, text=True, timeout=10)
    assert json.loads(completed.stdout) == [None, None, {"bypass_rules": True, "sha": None}]


@pytest.mark.skipif(sys.platform != "linux", reason="Linux peak RSS units and independent process accounting")
@pytest.mark.parametrize("transport", ["identity", "curl-data"])
def test_request_body_projection_memory_is_independent_of_large_tokens(tmp_path, transport):
    reports = []
    script = '''
import json, resource, sys
from pathlib import Path
from cc_transcript.parser import project_request_body
if sys.argv[2] == "legacy":
    text = Path(sys.argv[1]).read_text()
    if sys.argv[3] == "curl-data":
        text = text.replace("\\r", "").replace("\\n", "").replace("\\x00", "")
    body = json.loads(text)
    assert body["bypass_rules"] is True and body["sha"] == "a" * 40
else:
    result = project_request_body(sys.argv[1], transport=sys.argv[3])
    assert result == {"bypass_rules": True, "sha": "a" * 40}, result
peak = next(line.split() for line in Path("/proc/self/status").read_text().splitlines() if line.startswith("VmHWM:"))
assert len(peak) == 3 and peak[2] == "kB", peak
print(json.dumps({"peak_rss_kib": int(peak[1]), "historical_ru_maxrss_kib": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}))
'''
    for size in (4 * 1024 * 1024, 64 * 1024 * 1024):
        path = tmp_path / f"request-{size}.json"
        with path.open("wb") as output:
            output.write(b'{"bypass_rules":false,"ignored":"')
            block = b"x" * 8192 if transport == "identity" else b"x\r\n\0" * 2048
            for _ in range(size // len(block)):
                output.write(block)
            output.write(b'","')
            for _ in range(size // len(block)):
                output.write(block)
            output.write(b'": [NaN, Infinity, -Infinity], "sha":"' + b"a" * 40 + b'", "bypass_rules":true}')
        report = {"source_bytes": path.stat().st_size, "transport": transport}
        for mode in ("native", "legacy"):
            completed = subprocess.run([sys.executable, "-c", script, str(path), mode, transport], check=True, capture_output=True, text=True, timeout=120, env=os.environ.copy())
            measured = json.loads(completed.stdout)
            report[mode] = measured["peak_rss_kib"]
            report[mode + "_historical_ru_maxrss"] = measured["historical_ru_maxrss_kib"]
        reports.append(report)
        path.unlink()
    assert reports[1]["native"] <= reports[0]["native"] + 16 * 1024, reports
    assert reports[1]["legacy"] >= reports[0]["legacy"] + 32 * 1024, reports
    assert reports[1]["legacy"] >= reports[1]["native"] + 32 * 1024, reports
    print(json.dumps({"request_projection_memory": reports}), flush=True)


def test_request_body_transport_rejects_unknown_before_open(tmp_path):
    with pytest.raises(ValueError, match="unknown request body transport"):
        project_request_body(tmp_path / "does-not-exist", transport="unknown")


@pytest.mark.parametrize("broken", [False, True])
@pytest.mark.parametrize("flag", ["-d", "--data", "--data-binary", "--json", "--data-raw"])
def test_request_body_projection_matches_actual_curl_wire(tmp_path, flag, broken):
    wire = []

    class Receiver(BaseHTTPRequestHandler):
        def do_PUT(self):
            wire.append(self.rfile.read(int(self.headers["Content-Length"])))
            self.send_response(204)
            self.end_headers()

        def log_message(self, format, *args):
            pass

    path = tmp_path / "request.json"
    flag_bytes = b"tr\r\n\0ue" if broken else b"true"
    path.write_bytes(b'{"bypass_rules":' + flag_bytes + b',"sha":"' + b"a" * 40 + b'"}')
    with HTTPServer(("127.0.0.1", 0), Receiver) as server:
        server.timeout = 5
        receiver = threading.Thread(target=server.handle_request)
        receiver.start()
        try:
            subprocess.run(["curl", "--disable", "--noproxy", "*", "--silent", "--show-error", "--connect-timeout", "2", "--max-time", "4", "--request", "PUT", flag, "@" + str(path), f"http://127.0.0.1:{server.server_port}/"], check=True, capture_output=True, timeout=6)
        finally:
            receiver.join(timeout=6)
        assert not receiver.is_alive()
    assert len(wire) == 1
    try:
        expected = json.loads(wire[0])
    except ValueError:
        expected = None
    if flag == "--data-raw":
        assert wire[0] == ("@" + str(path)).encode()
        assert expected is None
        return
    transport = "curl-data" if flag in {"-d", "--data"} else "identity"
    actual = project_request_body(path, transport=transport)
    assert actual == expected
    assert path.read_bytes() == b'{"bypass_rules":' + flag_bytes + b',"sha":"' + b"a" * 40 + b'"}'
