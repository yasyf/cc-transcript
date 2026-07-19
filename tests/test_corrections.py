from __future__ import annotations

import pathlib
from typing import Any

import pytest

from cc_transcript.corrections import Correction, CorrectionLog
from cc_transcript.ids import EventUuid
from tests.support import ANCHOR, DIGEST_A, DIGEST_B, DIGEST_C, OTHER_SESSION, SESSION, correction

pytestmark = pytest.mark.anyio

OTHER_ANCHOR = EventUuid("anchor-2")

NO_FIX_FIELDS: dict[str, Any] = {
    "correction_origin": None,
    "correction_file": None,
    "correction_old": None,
    "correction_new": None,
    "correction_commit": None,
    "overlap": 0.0,
    "detail": {},
}


async def open_log(tmp_path: pathlib.Path) -> CorrectionLog:
    return await CorrectionLog.open(tmp_path / "corrections.db")


async def test_for_anchor_filters_by_anchor(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    await log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR))
    await log.append(correction(incorrect_digest=DIGEST_B, anchor_uuid=ANCHOR))
    await log.append(correction(incorrect_digest=DIGEST_C, anchor_uuid=OTHER_ANCHOR))
    assert {c.incorrect_digest for c in await log.for_anchor(SESSION, ANCHOR)} == {DIGEST_A, DIGEST_B}
    assert [c.incorrect_digest for c in await log.for_anchor(SESSION, OTHER_ANCHOR)] == [DIGEST_C]


async def test_by_digest_is_the_cross_consumer_join(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    await log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=ANCHOR))
    await log.append(correction(incorrect_digest=DIGEST_A, anchor_uuid=OTHER_ANCHOR))
    await log.append(correction(incorrect_digest=DIGEST_B, anchor_uuid=ANCHOR))
    assert {c.anchor_uuid for c in await log.by_digest(SESSION, incorrect_digest=DIGEST_A)} == {ANCHOR, OTHER_ANCHOR}
    assert await log.by_digest(SESSION, incorrect_digest=DIGEST_C) == ()
    assert await log.by_digest(OTHER_SESSION, incorrect_digest=DIGEST_A) == ()


async def test_no_correction_row_round_trips_nulls(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    row = correction(**NO_FIX_FIELDS)
    await log.append(row)
    assert await log.for_session(SESSION) == (row,)


async def test_review_correction_round_trips_text_and_null_digest(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    row = Correction(
        ts_ms=2_000,
        session_id=SESSION,
        source="cc-review",
        anchor_uuid=EventUuid("review:r1:7"),
        incorrect_digest=None,
        incorrect_file="/a.py",
        incorrect_old="",
        incorrect_new="pip install requests",
        correction_origin="review",
        correction_text="use uv add, not pip install",
        detail={"repo": "github.com/yasyf/x"},
    )
    await log.append(row)
    (stored,) = await log.for_session(SESSION)
    assert stored == row
    assert stored.incorrect_digest is None and stored.correction_text == "use uv add, not pip install"


async def test_for_repo_filters_on_detail_repo(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    await log.append(correction(detail={"repo": "repo-a"}))
    await log.append(correction(incorrect_digest=DIGEST_B, anchor_uuid=OTHER_ANCHOR, detail={"repo": "repo-b"}))
    assert {c.incorrect_digest for c in await log.for_repo("repo-a")} == {DIGEST_A}
    assert {c.incorrect_digest for c in await log.for_repo("repo-b")} == {DIGEST_B}
    assert await log.for_repo("repo-c") == ()


async def test_since_reads_forward_from_a_cursor(tmp_path: pathlib.Path) -> None:
    log = await open_log(tmp_path)
    await log.append(correction(ts_ms=1_000, incorrect_digest=DIGEST_A))
    await log.append(correction(ts_ms=2_000, incorrect_digest=DIGEST_B, source="captain-hook"))
    await log.append(correction(ts_ms=3_000, incorrect_digest=DIGEST_C))
    assert [c.ts_ms for c in await log.since(1_000)] == [2_000, 3_000]
    assert [c.ts_ms for c in await log.since(0, source="captain-hook")] == [2_000]
