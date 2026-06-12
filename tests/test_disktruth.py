from __future__ import annotations

import json
from functools import partial
from typing import TYPE_CHECKING, Any

import anyio
import pytest

from cc_transcript.context import SchemaError
from cc_transcript.disktruth import (
    AttributionRange,
    DiskTruth,
    FileAttribution,
    TreeTurn,
    export_activity,
    load_export,
)
from cc_transcript.ids import SessionId

if TYPE_CHECKING:
    from pathlib import Path

SESSION = SessionId("22222222-2222-2222-2222-222222222222")

EXPORT_PAYLOAD: dict[str, Any] = {
    "schema": "cc-review.activity/1",
    "session_id": str(SESSION),
    "turns": [
        {
            "turn_id": 7,
            "repo_root": "/repo",
            "started_at_ms": 1_767_323_045_000,
            "ended_at_ms": 1_767_323_105_000,
            "tree_start": "aaa111",
            "tree_end": "bbb222",
            "status": "closed",
        },
        {
            "turn_id": 8,
            "repo_root": "/repo",
            "started_at_ms": 1_767_323_200_000,
            "ended_at_ms": 0,
            "tree_start": "bbb222",
            "tree_end": "",
            "status": "open",
        },
    ],
    "attributions": [
        {
            "review_id": "rev-1",
            "version": 2,
            "file_path": "src/app.py",
            "ranges": [
                {"start": 1, "end": 4, "turn_id": 7},
                {"start": 9, "end": 9, "turn_id": None},
            ],
        }
    ],
}

EXPECTED = DiskTruth(
    session_id=SESSION,
    turns=(
        TreeTurn(
            turn_id=7,
            repo_root="/repo",
            started_at_ms=1_767_323_045_000,
            ended_at_ms=1_767_323_105_000,
            tree_start="aaa111",
            tree_end="bbb222",
            status="closed",
        ),
        TreeTurn(
            turn_id=8,
            repo_root="/repo",
            started_at_ms=1_767_323_200_000,
            ended_at_ms=0,
            tree_start="bbb222",
            tree_end="",
            status="open",
        ),
    ),
    attributions=(
        FileAttribution(
            review_id="rev-1",
            version=2,
            file_path="src/app.py",
            ranges=(AttributionRange(start=1, end=4, turn_id=7), AttributionRange(start=9, end=9, turn_id=None)),
        ),
    ),
)


def export_bytes(payload: dict[str, Any]) -> bytes:
    return json.dumps(payload).encode()


def fake_binary(tmp_path: Path, payload: dict[str, Any]) -> str:
    (tmp_path / "export.json").write_bytes(export_bytes(payload))
    script = tmp_path / "cc-review"
    script.write_text(
        "#!/bin/sh\n"
        f'[ "$1 $2 $3 $4" = "export activity --session {SESSION}" ] || exit 3\n'
        f"cat {tmp_path / 'export.json'}\n"
    )
    script.chmod(0o755)
    return str(script)


def test_load_export_round_trip() -> None:
    assert load_export(export_bytes(EXPORT_PAYLOAD)) == EXPECTED


@pytest.mark.parametrize(
    "data",
    [
        pytest.param(export_bytes(EXPORT_PAYLOAD | {"schema": "cc-review.activity/2"}), id="unknown_version"),
        pytest.param(export_bytes({k: v for k, v in EXPORT_PAYLOAD.items() if k != "schema"}), id="missing_schema"),
        pytest.param(b"[]", id="not_an_object"),
    ],
)
def test_load_export_rejects_bad_schema(data: bytes) -> None:
    with pytest.raises(SchemaError, match="cc-review.activity/1"):
        load_export(data)


def test_load_export_missing_field_fails_loud() -> None:
    truncated = EXPORT_PAYLOAD | {"turns": [{k: v for k, v in EXPORT_PAYLOAD["turns"][0].items() if k != "tree_end"}]}
    with pytest.raises(KeyError, match="tree_end"):
        load_export(export_bytes(truncated))


def test_export_activity_absent_binary_returns_none() -> None:
    assert anyio.run(partial(export_activity, SESSION, binary="cc-review-definitely-not-installed")) is None


def test_export_activity_nonzero_exit_returns_none() -> None:
    assert anyio.run(partial(export_activity, SESSION, binary="false")) is None


def test_export_activity_parses_stdout(tmp_path: Path) -> None:
    binary = fake_binary(tmp_path, EXPORT_PAYLOAD)
    assert anyio.run(partial(export_activity, SESSION, binary=binary)) == EXPECTED


def test_export_activity_raises_on_unknown_schema(tmp_path: Path) -> None:
    binary = fake_binary(tmp_path, EXPORT_PAYLOAD | {"schema": "cc-review.activity/9"})
    with pytest.raises(SchemaError, match="cc-review.activity/1"):
        anyio.run(partial(export_activity, SESSION, binary=binary))
