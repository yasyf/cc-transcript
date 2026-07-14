"""Freeze the Python notification-queue replay into ``tests/testdata/notifications_golden.json``.

The deterministic bench corpus carries no ``queue-operation`` records, so this uses a
curated battery of synthetic transcripts exercising every queue verb
(enqueue/dequeue/remove/popAll), task-notification user delivery, queued-command
attachment delivery, the empty-enqueue slot, and a plain no-delivery conversation.
Each case freezes the Python ``Notifications.from_events`` replay (queued/delivered/
enqueued); ``tests/test_notifications_parity.py`` asserts the Rust
``notifications_replay`` port reproduces it and that the Python reference still does.

Run: ``uv run --no-sync python scripts/gen_notifications_golden.py``
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

from cc_transcript.filterspec import TASK_NOTIFICATION_MARKER
from cc_transcript.notifications import Notifications, tool_use_marker
from cc_transcript.parser import parse_events_from_bytes
from scripts.gen_corpus import REPO_ROOT

GOLDEN = REPO_ROOT / "tests" / "testdata" / "notifications_golden.json"

TID = "toolu_bg01"
T1 = "toolu_first"
T2 = "toolu_second"
USER_MSG = "please run the full test suite"


def meta(uuid: str) -> dict[str, Any]:
    return {
        "uuid": uuid,
        "parentUuid": None,
        "sessionId": "22222222-2222-2222-2222-222222222222",
        "timestamp": "2026-01-06T09:00:00.000Z",
        "cwd": "/repo",
        "gitBranch": "main",
        "version": "2.1.7",
        "entrypoint": "cli",
    }


def notif(tool_use_id: str, *, body: str = "background task finished") -> str:
    return f"{TASK_NOTIFICATION_MARKER}{body} {tool_use_marker(tool_use_id)}</task-notification>"


def queue_op(operation: str, content: str = "") -> dict[str, Any]:
    return {"type": "queue-operation", "operation": operation, "content": content}


def user(text: str, uuid: str = "u") -> dict[str, Any]:
    return {"type": "user", **meta(uuid), "message": {"role": "user", "content": text}}


def assistant(text: str, uuid: str = "a") -> dict[str, Any]:
    return {"type": "assistant", **meta(uuid), "message": {"role": "assistant", "model": "m", "content": [{"type": "text", "text": text}]}}


def attachment(prompt: str, uuid: str = "att") -> dict[str, Any]:
    return {"type": "attachment", **meta(uuid), "attachment": {"type": "queued_command", "prompt": prompt, "commandMode": "prompt"}}


@dataclass(frozen=True)
class Case:
    id: str
    records: tuple[dict[str, Any], ...]


CASES: tuple[Case, ...] = (
    Case("enqueue-only", (queue_op("enqueue", notif(TID)),)),
    Case("dequeue-then-user-delivery", (queue_op("enqueue", notif(TID)), queue_op("dequeue"), user(notif(TID)))),
    Case("attachment-delivery", (attachment(notif(TID)),)),
    Case("enqueue-and-attachment", (queue_op("enqueue", notif(TID)), attachment(notif(TID)))),
    Case("remove", (queue_op("enqueue", notif(TID)), queue_op("remove"))),
    Case("popall-spares-notif", (queue_op("enqueue", notif(TID)), queue_op("enqueue", USER_MSG), queue_op("popAll", USER_MSG))),
    Case(
        "two-notifs-first-delivered",
        (queue_op("enqueue", notif(T1)), queue_op("enqueue", notif(T2)), queue_op("dequeue"), user(notif(T1))),
    ),
    Case("plain-user-delivery-no-queue", (user(notif(TID)),)),
    Case("no-delivery", (user("just working, no notification here"),)),
    Case("empty-enqueue-slot", (queue_op("enqueue", ""), queue_op("enqueue", notif(TID)), queue_op("dequeue"))),
    Case("attachment-empty-prompt", (attachment(""),)),
    Case(
        "conversation-with-delivery",
        (user("do the work"), assistant("on it"), queue_op("enqueue", notif(TID)), queue_op("dequeue"), user(notif(TID))),
    ),
    Case("no-queue-ops-plain-conversation", (user("fix the bug"), assistant("done"))),
)


def to_bytes(records: tuple[dict[str, Any], ...]) -> bytes:
    return b"\n".join(json.dumps(record).encode() for record in records)


def replay(records: tuple[dict[str, Any], ...]) -> dict[str, list[str]]:
    n = Notifications.from_events(parse_events_from_bytes(to_bytes(records)))
    return {"queued": list(n.queued), "delivered": list(n.delivered), "enqueued": list(n.enqueued)}


def main() -> None:
    data = [{"id": case.id, "records": list(case.records), "expected": replay(case.records)} for case in CASES]
    GOLDEN.write_text(json.dumps(data, indent=2, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {len(data)} golden notification replays to {GOLDEN.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
