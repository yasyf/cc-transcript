from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any

import anyio
import orjson
import pytest

from cc_transcript.discovery import CLAUDE_PROJECTS_DIR
from cc_transcript.parser import PythonBackend, parse_events_from_bytes, parse_print_result
from tests.support import RUST_BACKEND, fixture_bytes, fixture_entries, real_corpus, requires_rust
from tests.support import raw_envelope as envelope

if TYPE_CHECKING:
    from cc_transcript.backend import ParsedTranscript
    from cc_transcript.models import TranscriptEvent

TESTDATA = Path(__file__).parent / "testdata"


def bare_nonobject_fixture_bytes() -> bytes:
    real = orjson.dumps(fixture_entries()[0])
    return b"\n".join([real, b"42", b'"a bare string"', b"[1, 2, 3]", real])


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
def test_bare_nonobject_lines_skipped_with_parity(tmp_path: Path) -> None:
    path = tmp_path / "scalars.jsonl"
    path.write_bytes(bare_nonobject_fixture_bytes())
    expected = parse_events_from_bytes(path.read_bytes())
    assert len(expected) == 2, "bare scalar and array lines are skipped, two real events remain"
    assert_parity(path)


@requires_rust
def test_agent_injected_field_true_on_both_backends(tmp_path: Path) -> None:
    path = tmp_path / "banner.jsonl"
    path.write_bytes(
        orjson.dumps(
            envelope(
                type="user",
                message={
                    "role": "user",
                    "content": "<teammate-message from='reviewer'>please rebase</teammate-message>",
                },
            )
        )
    )
    (py_event,) = parse_events_from_bytes(path.read_bytes())
    (rust_event,) = rust_events(path)
    assert py_event.is_agent_injected is True
    assert rust_event.is_agent_injected is True


@requires_rust
@pytest.mark.parametrize(
    "content",
    [
        pytest.param("<teammate-message\u0301>", id="combining-mark"),
        pytest.param("why did the transcript contain <teammate-message from=a> above", id="mid-text-mention"),
    ],
)
def test_agent_injected_field_false_on_both_backends(tmp_path: Path, content: str) -> None:
    """A combining mark after the tag name (once Python-only True) and a mid-text mention are
    not banners — both backends must agree is_agent_injected is False."""
    path = tmp_path / "not-a-banner.jsonl"
    path.write_bytes(orjson.dumps(envelope(type="user", message={"role": "user", "content": content})))
    (py_event,) = parse_events_from_bytes(path.read_bytes())
    (rust_event,) = rust_events(path)
    assert py_event.is_agent_injected is False
    assert rust_event.is_agent_injected is False


@requires_rust
@pytest.mark.parametrize("path", real_corpus(), ids=lambda p: f"{p.parent.name}/{p.name}")
def test_real_corpus_parity(path: Path) -> None:
    assert_parity(path)


@requires_rust
def test_tool_use_result_number_types_match_orjson(tmp_path: Path) -> None:
    """`-0` is int 0, `-0.0` is float -0.0, and a beyond-u64 integer is a lossy
    float through both backends — orjson (the PythonBackend decoder) is the
    reference, and equality masks int/float and signed-zero drift, so the types
    and reprs are pinned. Raw bytes, because Python has no int -0 to round-trip
    through orjson."""
    from cc_transcript.models import ToolResultBlock, UserEvent

    path = tmp_path / "zeros.jsonl"
    path.write_bytes(
        b'{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z",'
        b'"toolUseResult":{"neg_zero_int":-0,"neg_zero_float":-0.0,"int":5,"float":1.0,'
        b'"big":18446744073709551616,"exp":1e2},'
        b'"message":{"role":"user","content":'
        b'[{"type":"tool_result","tool_use_id":"t1","content":"x","is_error":false}]}}'
    )

    def payload(events: list[TranscriptEvent]) -> dict[str, Any]:
        (event,) = events
        assert isinstance(event, UserEvent)
        (block,) = (b for b in event.blocks if isinstance(b, ToolResultBlock))
        assert isinstance(block.tool_use_result, dict)
        return dict(block.tool_use_result)

    py, rs = payload(parse_events_from_bytes(path.read_bytes())), payload(rust_events(path))
    expected = {
        "neg_zero_int": 0,
        "neg_zero_float": -0.0,
        "int": 5,
        "float": 1.0,
        "big": 1.8446744073709552e19,
        "exp": 100.0,
    }
    for backend in (py, rs):
        assert {k: (type(v), repr(v)) for k, v in backend.items()} == {
            k: (type(v), repr(v)) for k, v in expected.items()
        }


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
