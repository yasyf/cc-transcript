"""Freeze the Python sentiment bucketing into ``tests/testdata/buckets_golden.json``.

Serializes the ``ConversationBucketer.bucket_events`` output for a curated battery of
synthetic transcripts — the bench corpus transcripts are multi-megabyte, so curated
cases embed their own events — covering the substantive-user/assistant window rule,
junk-user dropping (interrupt/structural/stop-hook/bash-echo), the sub-``MIN_USER_TURNS``
and short-ack drops, time-gap index splits, multi-session first-appearance ordering,
and codepoint-counted user length. Each bucket is projected to its ``session_id``,
``bucket_index``, ``bucket_start_ms``, and member ``uuids``;
``tests/test_buckets_parity.py`` asserts the Rust ``bucket_events`` port reproduces it
and that the Python reference still does.

Run: ``uv run --no-sync python scripts/gen_buckets_golden.py``
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

from cc_transcript.parser import parse_events_from_bytes
from cc_transcript.sentiment.buckets import ConversationBucketer
from scripts.gen_corpus import REPO_ROOT

GOLDEN = REPO_ROOT / "tests" / "testdata" / "buckets_golden.json"


def user(session: str, ts: str, text: str, uuid: str) -> dict[str, Any]:
    return {
        "type": "user",
        "uuid": uuid,
        "parentUuid": None,
        "sessionId": session,
        "timestamp": ts,
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "2.1.7",
        "entrypoint": "cli",
        "message": {"role": "user", "content": text},
    }


def assistant(session: str, ts: str, uuid: str) -> dict[str, Any]:
    return {
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": None,
        "sessionId": session,
        "timestamp": ts,
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "2.1.7",
        "entrypoint": "cli",
        "message": {"role": "assistant", "model": "m", "content": [{"type": "text", "text": "working"}]},
    }


@dataclass(frozen=True)
class Case:
    id: str
    records: tuple[dict[str, Any], ...]


CASES: tuple[Case, ...] = (
    Case(
        "simple-one-bucket",
        (
            user("s", "2026-01-06T09:01:00.000Z", "please fix the parser", "u1"),
            assistant("s", "2026-01-06T09:01:30.000Z", "a1"),
            user("s", "2026-01-06T09:02:00.000Z", "and the tests", "u2"),
        ),
    ),
    Case(
        "junk-interrupt-dropped",
        (
            user("s", "2026-01-06T09:00:00.000Z", "fix the bug please", "u1"),
            user("s", "2026-01-06T09:00:10.000Z", "[Request interrupted by user]", "u2"),
            assistant("s", "2026-01-06T09:00:20.000Z", "a1"),
            user("s", "2026-01-06T09:00:30.000Z", "handle the sidechain too", "u3"),
        ),
    ),
    Case(
        "junk-structural-and-stop-hook-dropped",
        (
            user("s", "2026-01-06T09:00:00.000Z", "<system-reminder>injected</system-reminder>", "u1"),
            user("s", "2026-01-06T09:00:05.000Z", "Stop hook feedback: do it again", "u2"),
            user("s", "2026-01-06T09:00:10.000Z", "the real substantive prompt", "u3"),
            assistant("s", "2026-01-06T09:00:20.000Z", "a1"),
            user("s", "2026-01-06T09:00:30.000Z", "another real prompt here", "u4"),
        ),
    ),
    Case(
        "junk-bash-echo-dropped",
        (
            user("s", "2026-01-06T09:00:00.000Z", "<bash-input>ls -la</bash-input>", "u1"),
            user("s", "2026-01-06T09:00:05.000Z", "first substantive prompt", "u2"),
            assistant("s", "2026-01-06T09:00:15.000Z", "a1"),
            user("s", "2026-01-06T09:00:25.000Z", "second substantive prompt", "u3"),
        ),
    ),
    Case(
        "below-min-user-turns",
        (
            user("s", "2026-01-06T09:00:00.000Z", "just one real prompt", "u1"),
            assistant("s", "2026-01-06T09:00:10.000Z", "a1"),
        ),
    ),
    Case(
        "short-acks-no-substance",
        (
            user("s", "2026-01-06T09:00:00.000Z", "ok", "u1"),
            user("s", "2026-01-06T09:00:10.000Z", "yes", "u2"),
            assistant("s", "2026-01-06T09:00:20.000Z", "a1"),
        ),
    ),
    Case(
        "window-without-assistant-dropped",
        (
            user("s", "2026-01-06T09:00:00.000Z", "first substantive prompt", "u1"),
            user("s", "2026-01-06T09:00:10.000Z", "second substantive prompt", "u2"),
        ),
    ),
    Case(
        "time-gap-two-indices",
        (
            user("s", "2026-01-06T09:00:00.000Z", "first substantive prompt", "u1"),
            assistant("s", "2026-01-06T09:00:30.000Z", "a1"),
            user("s", "2026-01-06T09:10:00.000Z", "second substantive prompt", "u2"),
            assistant("s", "2026-01-06T09:10:30.000Z", "a2"),
        ),
    ),
    Case(
        "two-sessions-first-appearance-order",
        (
            user("b", "2026-01-06T09:00:00.000Z", "session b first prompt", "b1"),
            user("a", "2026-01-06T09:00:05.000Z", "session a first prompt", "a1"),
            assistant("b", "2026-01-06T09:00:10.000Z", "b2"),
            assistant("a", "2026-01-06T09:00:15.000Z", "a2"),
            user("b", "2026-01-06T09:00:20.000Z", "session b second prompt", "b3"),
            user("a", "2026-01-06T09:00:25.000Z", "session a second prompt", "a3"),
        ),
    ),
    Case(
        "unicode-codepoint-length",
        (
            user("s", "2026-01-06T09:00:00.000Z", "héllo", "u1"),
            assistant("s", "2026-01-06T09:00:10.000Z", "a1"),
            user("s", "2026-01-06T09:00:20.000Z", "漢字テスト", "u2"),
        ),
    ),
)


def to_bytes(records: tuple[dict[str, Any], ...]) -> bytes:
    return b"\n".join(json.dumps(record).encode() for record in records)


def project(records: tuple[dict[str, Any], ...]) -> list[dict[str, Any]]:
    return [
        {
            "session_id": bucket.session_id,
            "bucket_index": int(bucket.bucket_index),
            "bucket_start_ms": int(bucket.bucket_start.timestamp() * 1000),
            "uuids": [event.meta.uuid for event in bucket.events],
        }
        for bucket in ConversationBucketer.bucket_events(parse_events_from_bytes(to_bytes(records)))
    ]


def main() -> None:
    data = [{"id": case.id, "records": list(case.records), "buckets": project(case.records)} for case in CASES]
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} golden bucketings to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
