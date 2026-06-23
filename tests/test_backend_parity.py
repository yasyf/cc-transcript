from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any

import anyio
import orjson
import pytest

from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.parser import PythonBackend, load_rust_backend, parse_events_from_bytes, parse_print_result

if TYPE_CHECKING:
    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.models import TranscriptEvent

REAL_CORPUS_SAMPLE = 25
TESTDATA = Path(__file__).parent / "testdata"
RUST_BACKEND = load_rust_backend()
requires_rust = pytest.mark.skipif(RUST_BACKEND is None, reason="_parser_rs extension is not built")


def envelope(**overrides: Any) -> dict[str, Any]:
    return {
        "uuid": "uuid-1",
        "parentUuid": "uuid-0",
        "sessionId": "sess-1",
        "timestamp": "2026-01-02T03:04:05.000Z",
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "1.2.3",
        "isSidechain": False,
        "entrypoint": "cli",
    } | overrides


def fixture_entries() -> list[dict[str, Any]]:
    return [
        envelope(type="user", message={"role": "user", "content": "  fix the bug  "}),
        envelope(
            type="user",
            parentUuid=None,
            version="",
            message={
                "role": "user",
                "content": [
                    {"type": "text", "text": "here is context"},
                    {"type": "text", "text": "and more"},
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok output", "is_error": False},
                    {"type": "document", "source": {"kind": "file"}},
                ],
            },
        ),
        envelope(
            type="user",
            isSidechain=True,
            isMeta=True,
            message={
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_2",
                        "content": [
                            {"type": "text", "text": "line a"},
                            {"type": "tool_reference", "id": "x"},
                            {"type": "image", "source": {"data": "..."}},
                            {"type": "text", "text": "line b"},
                        ],
                        "is_error": True,
                    }
                ],
            },
        ),
        envelope(type="user", message={"role": "user", "content": "[Request interrupted by user]"}),
        envelope(
            type="user",
            toolUseResult={"isAsync": True, "status": "async_launched"},
            message={
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_async", "content": "launched", "is_error": False}
                ],
            },
        ),
        envelope(
            type="user",
            isCompactSummary=True,
            isVisibleInTranscriptOnly=True,
            message={"role": "user", "content": "compact unicode héllo 🤖 漢字"},
        ),
        envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-7",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "let me read"},
                    {"type": "thinking", "thinking": "hmm 🤔"},
                    {
                        "type": "tool_use",
                        "id": "toolu_9",
                        "name": "Read",
                        "input": {"file_path": "/x", "limit": 100, "ratio": 1.5, "flag": True, "none": None},
                    },
                ],
            },
        ),
        envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-7",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "done"}],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 7,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 25437,
                    "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 25437},
                    "service_tier": "standard",
                    "inference_geo": "not_available",
                    "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                },
            },
        ),
        envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "<synthetic>",
                "stop_reason": None,
                "content": [{"type": "text", "text": "noop"}],
            },
        ),
        envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"}}
                ],
            },
        ),
        envelope(
            type="assistant",
            message={
                "role": "assistant",
                "model": "claude-opus-4-8",
                "stop_reason": None,
                "content": [{"type": "future_block", "payload": {"n": 1, "deep": [True, None, 2.5]}}],
            },
        ),
        envelope(type="system", subtype="stop_hook_summary", content="hook ran"),
        envelope(type="system", subtype="turn_duration"),
        {"type": "mode", "mode": "normal", "sessionId": "sess-1"},
        {"type": "permission-mode", "permissionMode": "bypassPermissions", "sessionId": "sess-1"},
        {"type": "summary", "summary": "did stuff", "leafUuid": "uuid-x", "nested": {"a": [1, 2, 3], "b": 1.5}},
        envelope(type="attachment", attachment={"kind": "file", "size": 9999999999}),
        {"type": "queue-operation", "operation": "enqueue", "items": []},
        {"type": "file-history-snapshot", "snapshot": {"files": ["a", "b"]}},
        {"type": "ai-title", "title": "Some Title"},
        {"type": "last-prompt", "prompt": "do the thing"},
        {"type": "started", "ts": 123},
        {"type": "result", "ok": True, "count": 0},
    ]


def fixture_bytes() -> bytes:
    return b"\n".join(
        [
            orjson.dumps(fixture_entries()[0]),
            b"",
            b"   ",
            b"{not valid json",
            *(orjson.dumps(e) for e in fixture_entries()),
        ]
    )


def real_corpus() -> list[Path]:
    if not CLAUDE_PROJECTS_DIR.exists():
        return []
    paths = sorted(CLAUDE_PROJECTS_DIR.rglob("*.jsonl"), key=lambda p: p.stat().st_size)
    if not paths:
        return []
    agents = [p for p in paths if p.name.startswith("agent-")][:5]
    others = [p for p in paths if not p.name.startswith("agent-")]
    spread = [others[i] for i in range(0, len(others), max(1, len(others) // REAL_CORPUS_SAMPLE))]
    return list(dict.fromkeys(agents + spread))[:REAL_CORPUS_SAMPLE]


def rust_events(path: Path) -> list[TranscriptEvent]:
    assert RUST_BACKEND is not None
    from cc_transcript import _parser_rs

    out = _parser_rs.stream_parse([(str(path), 1.0)], 1).recv()
    assert out is not None
    return out[2]


def assert_parity(path: Path) -> None:
    expected = parse_events_from_bytes(path.read_bytes())
    actual = rust_events(path)
    assert len(actual) == len(expected), f"event count diverged for {path}: rust={len(actual)} python={len(expected)}"
    for i, (a, e) in enumerate(zip(actual, expected, strict=True)):
        assert a == e, f"event {i} diverged for {path}\n  python={e!r}\n  rust={a!r}"


@requires_rust
def test_fixture_corpus_parity(tmp_path: Path) -> None:
    path = tmp_path / "fixture.jsonl"
    path.write_bytes(fixture_bytes())
    expected = parse_events_from_bytes(path.read_bytes())
    assert len(expected) == len(fixture_entries()) + 1
    assert_parity(path)


@requires_rust
@pytest.mark.parametrize("path", real_corpus(), ids=lambda p: f"{p.parent.name}/{p.name}")
def test_real_corpus_parity(path: Path) -> None:
    assert_parity(path)


@requires_rust
def test_real_corpus_is_present() -> None:
    if not real_corpus():
        pytest.skip(f"no transcripts under {CLAUDE_PROJECTS_DIR}")
    assert real_corpus()


@requires_rust
def test_parse_batch_parity(tmp_path: Path) -> None:
    from cc_transcript.rust import RustBackend

    paths: list[tuple[Path, float]] = []
    for i, entry in enumerate(fixture_entries()):
        p = tmp_path / f"t{i}.jsonl"
        p.write_bytes(orjson.dumps(entry))
        paths.append((p, float(i)))

    async def drain(backend: PythonBackend | RustBackend) -> dict[Path, ParsedTranscript]:
        return {p.path: p async for p in backend.parse_batch(paths, prefetch=4)}

    py = anyio.run(drain, PythonBackend())
    rs = anyio.run(drain, RustBackend())
    assert set(rs) == set(py)
    for path, parsed in py.items():
        assert rs[path].mtime == parsed.mtime
        assert rs[path].events == parsed.events


@requires_rust
@pytest.mark.parametrize(
    "content",
    [{"not": "a list"}, 5, None, True],
    ids=["dict", "number", "null", "bool"],
)
def test_non_array_content_is_skipped_not_panics(tmp_path: Path, content: Any) -> None:
    from cc_transcript import _parser_rs

    path = tmp_path / "bad.jsonl"
    path.write_bytes(orjson.dumps(envelope(type="user", message={"role": "user", "content": content})))
    # A file whose events cannot be materialized is skipped, not propagated — and
    # crucially does not panic across the FFI boundary, so the stream just ends.
    assert _parser_rs.stream_parse([(str(path), 1.0)], 1).recv() is None


@requires_rust
def test_malformed_file_is_skipped_across_ffi(tmp_path: Path) -> None:
    from cc_transcript import _parser_rs

    bad = tmp_path / "bad.jsonl"
    bad.write_bytes(orjson.dumps(envelope(type="user", message={"role": "user", "content": {"x": "y"}})))
    assert _parser_rs.stream_parse([(str(bad), 1.0)], 4).recv_many(32) == []


@requires_rust
def test_stream_skips_bad_files_without_dropping_good_ones(tmp_path: Path) -> None:
    paths: list[tuple[Path, float]] = []
    for i in range(6):
        bad = tmp_path / f"bad{i}.jsonl"
        bad.write_bytes(b'{"type": "user", "message": {"role": "user", "content": "x"}}')  # no uuid -> skipped
        paths.append((bad, float(i)))
    good_names: list[str] = []
    for i in range(6):
        good = tmp_path / f"good{i}.jsonl"
        good.write_bytes(orjson.dumps(envelope(type="user", uuid=f"g{i}", message={"role": "user", "content": "hi"})))
        good_names.append(good.name)
        paths.append((good, float(100 + i)))

    async def drain() -> list[str]:
        assert RUST_BACKEND is not None
        return [parsed.path.name async for parsed in RUST_BACKEND.parse_batch(paths, prefetch=4)]

    assert sorted(anyio.run(drain)) == sorted(good_names)


@requires_rust
def test_print_result_parity() -> None:
    from cc_transcript import _parser_rs

    raw = (TESTDATA / "haiku_envelope.json").read_bytes()
    assert _parser_rs.parse_print_result(raw) == parse_print_result(raw)
