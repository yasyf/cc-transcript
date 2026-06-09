from __future__ import annotations

from pathlib import Path
from typing import Any

import anyio
import orjson

from cc_transcript.backend import ParsedTranscript
from cc_transcript.parser import PythonBackend


def envelope(uuid: str) -> dict[str, Any]:
    return {
        "type": "user",
        "uuid": uuid,
        "parentUuid": None,
        "sessionId": "s1",
        "timestamp": "2026-01-02T03:04:05.000Z",
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "1.0.0",
        "isSidechain": False,
        "entrypoint": "cli",
        "message": {"role": "user", "content": f"prompt {uuid}"},
    }


def write_transcript(path: Path, uuids: list[str]) -> None:
    path.write_bytes(b"\n".join(orjson.dumps(envelope(u)) for u in uuids))


def test_parse_batch_streams_all_transcripts(tmp_path: Path) -> None:
    a = tmp_path / "a.jsonl"
    b = tmp_path / "b.jsonl"
    write_transcript(a, ["a1", "a2"])
    write_transcript(b, ["b1"])

    async def collect() -> list[ParsedTranscript]:
        return [parsed async for parsed in PythonBackend().parse_batch([(a, 10.0), (b, 20.0)], prefetch=4)]

    results = anyio.run(collect)
    by_path = {p.path: p for p in results}
    assert set(by_path) == {a, b}
    assert by_path[a].mtime == 10.0
    assert tuple(e.meta.uuid for e in by_path[a].events) == ("a1", "a2")  # type: ignore[union-attr]
    assert by_path[b].mtime == 20.0
    assert len(by_path[b].events) == 1


def test_parse_batch_skips_unparseable_files(tmp_path: Path) -> None:
    good = tmp_path / "good.jsonl"
    bad = tmp_path / "bad.jsonl"
    write_transcript(good, ["g1"])
    bad.write_bytes(orjson.dumps({"type": "user", "message": {"role": "user", "content": "x"}}))  # no uuid -> KeyError

    async def collect() -> list[ParsedTranscript]:
        return [parsed async for parsed in PythonBackend().parse_batch([(good, 10.0), (bad, 20.0)], prefetch=4)]

    assert {p.path for p in anyio.run(collect)} == {good}


def test_parse_batch_empty_yields_nothing() -> None:
    async def collect() -> list[ParsedTranscript]:
        return [parsed async for parsed in PythonBackend().parse_batch([], prefetch=4)]

    assert anyio.run(collect) == []


def test_parse_batch_early_break_cleans_up(tmp_path: Path) -> None:
    paths: list[tuple[Path, float]] = []
    for i in range(10):
        p = tmp_path / f"t{i}.jsonl"
        write_transcript(p, [f"u{i}"])
        paths.append((p, float(i)))

    # Breaking out of the stream early closes the async generator mid-flight; the
    # producer task group must tear down without "cancel scope in a different task".
    async def first_only() -> ParsedTranscript | None:
        async for parsed in PythonBackend().parse_batch(paths, prefetch=2):
            return parsed
        return None

    assert anyio.run(first_only) is not None
