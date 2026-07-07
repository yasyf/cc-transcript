from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING

import pytest

from cc_transcript.activity import SessionActivity, native_user_classifier
from cc_transcript.ids import EventUuid, SessionId
from cc_transcript.models import AssistantEvent, CcVersion, EntryMeta, OtherEvent, UserEvent
from cc_transcript.notifications import NOTIFICATION_MARKER, Notifications, tool_use_marker
from cc_transcript.query import Session

if TYPE_CHECKING:
    from cc_transcript.activity import UserClassifier
    from cc_transcript.models import TranscriptEvent

BASE = datetime(2026, 1, 1, 12, 0, 0, tzinfo=UTC)
SESSION = SessionId("22222222-2222-2222-2222-222222222222")

TID = "toolu_bg01"
T1 = "toolu_first"
T2 = "toolu_second"
COLON_TID = "3f2a1b7c:inner"
USER_MSG = "please run the full test suite"


def meta(uuid: str, *, secs: int = 0, is_meta: bool = False, is_sidechain: bool = False) -> EntryMeta:
    return EntryMeta(
        uuid=EventUuid(uuid),
        parent_uuid=None,
        session_id=SESSION,
        timestamp=BASE + timedelta(seconds=secs),
        cwd="/repo",
        git_branch="main",
        cc_version=CcVersion("1.2.3"),
        is_sidechain=is_sidechain,
        is_meta=is_meta,
        entrypoint="cli",
        is_compact_summary=False,
        is_visible_in_transcript_only=False,
    )


def user(uuid: str, text: str = "", *, secs: int = 0) -> UserEvent:
    return UserEvent(meta=meta(uuid, secs=secs), text=text, blocks=(), interrupted=False)


def assistant(uuid: str, text: str = "", *, secs: int = 0) -> AssistantEvent:
    return AssistantEvent(
        meta=meta(uuid, secs=secs), model="claude-opus-4-8", text=text, blocks=(), stop_reason=None, usage=None
    )


def queue_op(operation: str, content: str = "") -> OtherEvent:
    return OtherEvent(type="queue-operation", raw={"operation": operation, "content": content})


def enqueue(content: str) -> OtherEvent:
    return queue_op("enqueue", content)


def dequeue() -> OtherEvent:
    return queue_op("dequeue")


def remove() -> OtherEvent:
    return queue_op("remove")


def pop_all(content: str) -> OtherEvent:
    return queue_op("popAll", content)


def attachment(prompt: str) -> OtherEvent:
    return OtherEvent(type="attachment", raw={"attachment": {"type": "queued_command", "prompt": prompt}})


def notif(tool_use_id: str, *, body: str = "background task finished") -> str:
    return f"{NOTIFICATION_MARKER}{body} {tool_use_marker(tool_use_id)}</task-notification>"


def session(*events: TranscriptEvent, user_classifier: UserClassifier = native_user_classifier) -> Session:
    return Session.from_activity(SessionActivity.from_events(SESSION, events, user_classifier=user_classifier))


@pytest.mark.parametrize(
    ("events", "tool_use_id", "completed", "pending", "has_pending"),
    [
        pytest.param(
            (enqueue(notif(TID)),),
            TID,
            False,
            True,
            True,
            id="enqueue-only-is-a-misfire",
        ),
        pytest.param(
            (enqueue(notif(TID)), dequeue(), user("u1", notif(TID))),
            TID,
            True,
            False,
            False,
            id="dequeue-then-user-delivery-completes",
        ),
        pytest.param(
            (enqueue(notif(TID)), remove(), attachment(notif(TID))),
            TID,
            True,
            False,
            False,
            id="remove-then-queued-command-attachment-completes",
        ),
        pytest.param(
            (OtherEvent(type="attachment", raw={"type": "attachment", "attachment": None}), enqueue(notif(TID))),
            TID,
            False,
            True,
            True,
            id="null-attachment-payload-does-not-crash",
        ),
        pytest.param(
            (enqueue(notif(TID)), remove()),
            TID,
            True,
            False,
            False,
            id="remove-without-delivery-counts-as-dropped-completed",
        ),
        pytest.param(
            (enqueue(notif(TID)), enqueue(USER_MSG), pop_all(USER_MSG)),
            TID,
            False,
            True,
            True,
            id="popall-of-user-message-spares-the-notification",
        ),
        pytest.param(
            (enqueue(notif(T1)), enqueue(notif(T2)), dequeue(), user("u1", notif(T1))),
            T1,
            True,
            False,
            True,
            id="two-notifs-first-delivered-completes-first",
        ),
        pytest.param(
            (enqueue(notif(T1)), enqueue(notif(T2)), dequeue(), user("u1", notif(T1))),
            T2,
            False,
            True,
            True,
            id="two-notifs-first-delivered-second-still-pending",
        ),
        pytest.param(
            (user("u1", notif(TID)),),
            TID,
            True,
            False,
            False,
            id="no-queue-ops-plain-user-delivery-completes",
        ),
        pytest.param(
            (user("u1", "just working, no notification here"),),
            TID,
            False,
            False,
            False,
            id="no-queue-ops-no-delivery-not-completed",
        ),
        pytest.param(
            (enqueue(notif(COLON_TID)),),
            COLON_TID,
            False,
            True,
            True,
            id="colon-bearing-id-is-pending",
        ),
        pytest.param(
            (enqueue(notif(COLON_TID)), dequeue(), user("u1", notif(COLON_TID))),
            COLON_TID,
            True,
            False,
            False,
            id="colon-bearing-id-completes",
        ),
        pytest.param(
            (enqueue(""), enqueue(notif(TID)), dequeue()),
            TID,
            False,
            True,
            True,
            id="empty-enqueue-occupies-a-slot",
        ),
    ],
)
def test_queue_protocol(
    events: tuple[TranscriptEvent, ...],
    tool_use_id: str,
    completed: bool,
    pending: bool,
    has_pending: bool,
) -> None:
    notifications = session(*events).notifications
    assert notifications.completed(tool_use_id) is completed
    assert notifications.pending(tool_use_id) is pending
    assert notifications.has_pending is has_pending


def test_popall_subtracts_by_containment_never_clears_all() -> None:
    notifications = session(enqueue(notif(TID)), enqueue(USER_MSG), pop_all(USER_MSG)).notifications
    assert notifications.enqueued == (notif(TID), USER_MSG)
    assert notifications.queued == (notif(TID),)
    assert notifications.delivered == ()
    assert notifications.has_pending is True
    assert notifications.pending(TID) is True
    assert notifications.completed(TID) is False


def test_delivery_is_scanned_off_events_not_turn_prompt() -> None:
    events = (user("u0", "do the work"), assistant("a0", "on it"), user("u1", notif(TID)))

    native = session(*events)
    assert any(NOTIFICATION_MARKER in turn.prompt for turn in native.turns)
    assert native.notifications.completed(TID) is True

    def folds_notification_deliveries(event: UserEvent) -> bool:
        return native_user_classifier(event) and NOTIFICATION_MARKER not in event.text

    folded = session(*events, user_classifier=folds_notification_deliveries)
    assert all(NOTIFICATION_MARKER not in turn.prompt for turn in folded.turns)
    assert folded.notifications.delivered == (notif(TID),)
    assert folded.notifications.completed(TID) is True


def test_has_pending_counts_task_id_only_notification() -> None:
    text = f"{NOTIFICATION_MARKER}a background task <task-id>bg-7</task-id></task-notification>"
    notifications = session(enqueue(text)).notifications
    assert notifications.has_pending is True
    assert notifications.pending("bg-7") is False
    assert notifications.completed("bg-7") is False


def test_session_property_and_public_export() -> None:
    import cc_transcript

    assert cc_transcript.Notifications is Notifications
    notifications = session(enqueue(notif(TID))).notifications
    assert isinstance(notifications, Notifications)
    assert notifications.enqueued == (notif(TID),)
    assert notifications.queued == (notif(TID),)
