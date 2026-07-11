from __future__ import annotations

from typing import TYPE_CHECKING, Any

import orjson
import pytest

from cc_transcript.activity_probe import PendingItem, probe_events, session_activity_probe
from cc_transcript.filterspec import is_agent_injection
from cc_transcript.models import (
    AssistantEvent,
    AttachmentEvent,
    OtherEvent,
    QueuedCommand,
    ToolResultBlock,
    ToolUseBlock,
    UserEvent,
)
from cc_transcript.parser import parse_events_from_bytes
from tests.support import real_corpus, requires_rust, rust_not_disabled

if TYPE_CHECKING:
    from pathlib import Path

    from cc_transcript.models import TranscriptEvent

WAITING_TOOLS = {"Monitor", "ScheduleWakeup", "SendMessage", "TeamCreate"}


def user(text: str, **flags: Any) -> dict[str, Any]:
    return {
        "type": "user",
        "uuid": "u",
        "sessionId": "s1",
        "timestamp": "2026-01-02T03:04:05Z",
        "message": {"role": "user", "content": text},
        **flags,
    }


def tool_use(name: str, tool_id: str, tool_input: dict[str, Any]) -> dict[str, Any]:
    return {
        "type": "assistant",
        "uuid": "a",
        "sessionId": "s1",
        "timestamp": "2026-01-02T03:04:06Z",
        "message": {
            "role": "assistant",
            "model": "m",
            "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": tool_input}],
        },
    }


def tool_result(tool_id: str, *, is_error: bool = False, is_async: bool = False) -> dict[str, Any]:
    return {
        "type": "user",
        "uuid": "r",
        "sessionId": "s1",
        "timestamp": "2026-01-02T03:04:07Z",
        "toolUseResult": {"isAsync": is_async},
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": "done", "is_error": is_error}],
        },
    }


def queue_op(content: str, operation: str = "enqueue") -> dict[str, Any]:
    return {"type": "queue-operation", "operation": operation, "content": content}


def attachment(prompt: str) -> dict[str, Any]:
    return {
        "type": "attachment",
        "uuid": "att",
        "sessionId": "s1",
        "timestamp": "2026-01-02T03:04:08Z",
        "attachment": {"type": "queued_command", "prompt": prompt},
    }


def notification(tool_id: str) -> str:
    return (
        f"<task-notification><task-id>t</task-id><tool-use-id>{tool_id}</tool-use-id>"
        "<status>completed</status></task-notification>"
    )


def delivered(tool_id: str) -> list[dict[str, Any]]:
    """The real delivery lifecycle: enqueue, dequeue, then a user turn carrying the text."""
    text = notification(tool_id)
    return [queue_op(text), queue_op("", operation="dequeue"), user(text)]


def write(tmp_path: Path, lines: list[dict[str, Any]]) -> Path:
    path = tmp_path / "session.jsonl"
    path.write_bytes(b"\n".join(orjson.dumps(line) for line in lines))
    return path


def reference_is_waiting(events: list[TranscriptEvent]) -> bool:
    """Independent re-derivation of captain-hook conditions.py ``is_waiting`` (post-d2e07cc).

    Mirrors exactly: the ``has_pending`` notifications layer plus per-launch
    ``pending_async`` with completion counted at delivery — a notification
    delivered as a user turn or ``queued_command`` attachment, or drained from
    the replayed FIFO, never merely enqueued (notifications.py ``Notifications``).
    Deliberately omitted: the Stop-payload ``background_tasks``/``session_crons``
    layer — hook-side data a transcript never carries. Deliberate divergence:
    compact-summary user lines do not open turns here, leading captain-hook,
    which still lets compaction close the turn. Agent-injected relay banners
    (``is_agent_injection``) likewise never open a turn.
    """
    results = {
        block.tool_use_id: block
        for event in events
        if isinstance(event, UserEvent)
        for block in event.blocks
        if isinstance(block, ToolResultBlock)
    }
    queued: list[str] = []
    enqueued: list[str] = []
    delivered_texts: list[str] = []
    for event in events:
        if isinstance(event, OtherEvent) and event.type == "queue-operation":
            match event.raw.get("operation"):
                case "enqueue":
                    enqueued.append(content := str(event.raw.get("content", "")))
                    queued.append(content)
                case "dequeue" | "remove" if queued:
                    queued.pop(0)
                case "popAll":
                    content = str(event.raw.get("content", ""))
                    queued = [item for item in queued if item not in content]
        if isinstance(event, UserEvent) and "<task-notification>" in event.text:
            delivered_texts.append(event.text)
        elif isinstance(event, AttachmentEvent) and isinstance(event.detail, QueuedCommand):
            delivered_texts.append(event.detail.prompt or "")

    def completed(tool_use_id: str) -> bool:
        marker = f"<tool-use-id>{tool_use_id}</tool-use-id>"
        return any(marker in text for text in delivered_texts) or (
            any(marker in text for text in enqueued) and not any(marker in text for text in queued)
        )

    def ephemeral_wait(block: ToolUseBlock) -> bool:
        if block.name in WAITING_TOOLS:
            return True
        match block.name:
            case "Agent" | "Task" | "Bash" if block.input.get("run_in_background"):
                return True
            case "Agent" | "Task" if "subagent_type" not in block.input:
                return True
        return False

    def pending_async(block: ToolUseBlock) -> bool:
        result = results.get(block.id)
        match block.name:
            case "Agent" | "Task" if result and result.is_async:
                return not completed(block.id)
            case "Workflow" if not (result and result.is_error):
                return not completed(block.id)
        return False

    def tool_calls(window: list[TranscriptEvent]) -> list[ToolUseBlock]:
        return [
            block
            for event in window
            if isinstance(event, AssistantEvent)
            for block in event.blocks
            if isinstance(block, ToolUseBlock)
            if not ((result := results.get(block.id)) and result.is_error)
        ]

    turn_start = next(
        (
            index
            for index in reversed(range(len(events)))
            if isinstance(event := events[index], UserEvent)
            and not (
                event.meta.is_meta
                or event.meta.is_sidechain
                or event.meta.is_compact_summary
                or event.interrupted
                or is_agent_injection(event.text)
            )
            and event.text.strip()
        ),
        0,
    )
    return (
        any("<task-notification>" in text for text in queued)
        or any(ephemeral_wait(block) for block in tool_calls(events[turn_start:]))
        or any(pending_async(block) for block in tool_calls(events))
    )


WORKFLOW = tool_use("Workflow", "wf1", {"script": "return 1"})
BACKGROUND_BASH = tool_use("Bash", "b1", {"command": "make", "run_in_background": True})
TYPED_AGENT = tool_use("Agent", "a1", {"subagent_type": "Explore", "prompt": "look"})

CASES = [
    pytest.param(
        [user("run the workflow"), WORKFLOW, tool_result("wf1")],
        True,
        False,
        (PendingItem("wf1", "Workflow", "pending_async_workflow"),),
        id="pending-workflow",
    ),
    pytest.param(
        [user("run the workflow"), WORKFLOW, tool_result("wf1"), *delivered("wf1")],
        False,
        False,
        (),
        id="workflow-delivered-notification-clears",
    ),
    pytest.param(
        [user("run the workflow"), WORKFLOW, tool_result("wf1"), queue_op(notification("wf1"))],
        True,
        False,
        (PendingItem("wf1", "Workflow", "pending_async_workflow"),),
        id="enqueued-undelivered-notification-still-waiting",
    ),
    pytest.param(
        [
            user("run the workflow"),
            WORKFLOW,
            tool_result("wf1"),
            queue_op(notification("wf1")),
            queue_op("", operation="remove"),
        ],
        False,
        False,
        (),
        id="removed-notification-counts-completed",
    ),
    pytest.param(
        [
            user("run the workflow"),
            WORKFLOW,
            tool_result("wf1"),
            queue_op(notification("wf1")),
            queue_op("run the tests please"),
            queue_op("run the tests please", operation="popAll"),
        ],
        True,
        False,
        (PendingItem("wf1", "Workflow", "pending_async_workflow"),),
        id="popall-drains-command-not-notification",
    ),
    pytest.param(
        [
            user("run the workflow"),
            WORKFLOW,
            tool_result("wf1"),
            queue_op(notification("wf1")),
            queue_op(notification("wf1"), operation="popAll"),
        ],
        False,
        False,
        (),
        id="popall-drains-the-notification-completes",
    ),
    pytest.param(
        [user("hi"), queue_op(notification("tu_ghost"))],
        True,
        False,
        (),
        id="orphan-undelivered-notification-is-waiting",
    ),
    pytest.param(
        [user("go"), TYPED_AGENT, tool_result("a1", is_async=True), attachment(notification("a1"))],
        False,
        False,
        (),
        id="attachment-delivery-completes",
    ),
    pytest.param(
        [user("go"), TYPED_AGENT, tool_result("a1", is_async=True), user(notification("a1"))],
        False,
        False,
        (),
        id="plain-user-delivery-completes",
    ),
    pytest.param(
        [user("run the workflow"), WORKFLOW, tool_result("wf1"), queue_op(notification("wf_other"))],
        True,
        False,
        (PendingItem("wf1", "Workflow", "pending_async_workflow"),),
        id="unrelated-marker-keeps-waiting",
    ),
    pytest.param(
        [user("run the workflow"), WORKFLOW, tool_result("wf1", is_error=True)],
        False,
        False,
        (),
        id="errored-workflow",
    ),
    pytest.param(
        [user("run the workflow"), WORKFLOW],
        True,
        True,
        (PendingItem("wf1", "Workflow", "pending_async_workflow"),),
        id="resultless-workflow-is-also-mid-tool",
    ),
    pytest.param(
        [user("build it"), BACKGROUND_BASH, tool_result("b1")],
        True,
        False,
        (PendingItem("b1", "Bash", "background"),),
        id="background-bash-current-turn",
    ),
    pytest.param(
        [user("build it"), BACKGROUND_BASH, tool_result("b1"), user("now do something else")],
        False,
        False,
        (),
        id="background-bash-previous-turn",
    ),
    pytest.param(
        [user("go"), tool_use("Agent", "a1", {"prompt": "look around"}), tool_result("a1")],
        True,
        False,
        (PendingItem("a1", "Agent", "subagentless_task"),),
        id="subagentless-agent",
    ),
    pytest.param(
        [user("go"), TYPED_AGENT, tool_result("a1")],
        False,
        False,
        (),
        id="typed-agent-sync-result",
    ),
    pytest.param(
        [user("go"), TYPED_AGENT, tool_result("a1", is_async=True), user("while that runs, plan")],
        True,
        False,
        (PendingItem("a1", "Agent", "pending_async_task"),),
        id="async-agent-previous-turn",
    ),
    pytest.param(
        [
            user("go"),
            TYPED_AGENT,
            tool_result("a1", is_async=True),
            user("while that runs, plan"),
            *delivered("a1"),
        ],
        False,
        False,
        (),
        id="async-agent-delivered-notification-clears",
    ),
    pytest.param(
        [user("watch it"), tool_use("Monitor", "m1", {"until": "done"})],
        True,
        True,
        (PendingItem("m1", "Monitor", "waiting_tool"),),
        id="unmatched-waiting-tool",
    ),
    pytest.param(
        [user("choose"), tool_use("AskUserQuestion", "q1", {"questions": []})],
        False,
        False,
        (),
        id="pending-ask-user-question-is-quiet",
    ),
    pytest.param(
        [user("list"), tool_use("Bash", "b1", {"command": "ls"})],
        False,
        True,
        (PendingItem("b1", "Bash", "mid_tool"),),
        id="unmatched-bash-is-mid-tool",
    ),
    pytest.param(
        [user("list"), tool_use("Bash", "b1", {"command": "ls"}), user("moving on")],
        False,
        False,
        (),
        id="unmatched-bash-previous-turn",
    ),
    pytest.param(
        [
            user("build it"),
            BACKGROUND_BASH,
            tool_result("b1"),
            user("injected context", isMeta=True),
            user("sidechain prompt", isSidechain=True),
            user("[Request interrupted by user]"),
            user("   "),
        ],
        True,
        False,
        (PendingItem("b1", "Bash", "background"),),
        id="non-prompt-users-do-not-open-turns",
    ),
    pytest.param(
        [user("build it"), BACKGROUND_BASH, tool_result("b1"), user("compact recap", isCompactSummary=True)],
        True,
        False,
        (PendingItem("b1", "Bash", "background"),),
        id="compact-summary-user-does-not-open-turn",
    ),
    pytest.param(
        [
            user("build it"),
            BACKGROUND_BASH,
            tool_result("b1"),
            user("<teammate-message from='mate'>ping</teammate-message>"),
        ],
        True,
        False,
        (PendingItem("b1", "Bash", "background"),),
        id="agent-injected-banner-does-not-open-turn",
    ),
    pytest.param(
        [
            user("build it"),
            BACKGROUND_BASH,
            tool_result("b1"),
            user("why did the transcript contain <teammate-message from='mate'> above?"),
        ],
        False,
        False,
        (),
        id="mid-text-relay-mention-opens-turn",
    ),
    pytest.param(
        [
            user("go"),
            tool_use("Agent", "a1", {"prompt": "x", "run_in_background": True}),
            tool_result("a1", is_async=True),
        ],
        True,
        False,
        (PendingItem("a1", "Agent", "background"),),
        id="dedupes-by-tool-use-id",
    ),
]


@requires_rust
@rust_not_disabled
@pytest.mark.parametrize(("lines", "is_waiting", "mid_tool", "pending"), CASES)
def test_fixture_probe(
    tmp_path: Path,
    lines: list[dict[str, Any]],
    is_waiting: bool,
    mid_tool: bool,
    pending: tuple[PendingItem, ...],
) -> None:
    path = write(tmp_path, lines)
    probe = session_activity_probe(path)
    assert probe.is_waiting == is_waiting
    assert probe.mid_tool == mid_tool
    assert probe.pending == pending
    events = parse_events_from_bytes(path.read_bytes())
    assert reference_is_waiting(events) == probe.is_waiting
    assert probe_events(events) == probe


@requires_rust
@rust_not_disabled
@pytest.mark.parametrize(("lines", "is_waiting", "mid_tool", "pending"), CASES)
def test_python_twin_parity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    lines: list[dict[str, Any]],
    is_waiting: bool,
    mid_tool: bool,
    pending: tuple[PendingItem, ...],
) -> None:
    """CC_TRANSCRIPT_DISABLE_RUST=1 reproduces the Rust probe for the same transcript."""
    path = write(tmp_path, lines)
    via_rust = session_activity_probe(path)
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    assert session_activity_probe(path) == via_rust


@requires_rust
@rust_not_disabled
def test_empty_session_is_idle(tmp_path: Path) -> None:
    probe = session_activity_probe(write(tmp_path, []))
    assert probe.is_waiting is False
    assert probe.mid_tool is False
    assert probe.pending == ()
    assert probe.last_event_epoch is None


@requires_rust
@rust_not_disabled
def test_last_event_epoch_is_max_meta_timestamp(tmp_path: Path) -> None:
    path = write(tmp_path, [user("hi"), tool_use("Bash", "b1", {"command": "ls"}), queue_op("no meta here")])
    assert session_activity_probe(path).last_event_epoch == 1767323046


@requires_rust
@rust_not_disabled
def test_custom_tool_sets_flow_through(tmp_path: Path) -> None:
    path = write(tmp_path, [user("go"), tool_use("MyWait", "w1", {}), tool_use("MyPrompt", "p1", {})])
    probe = session_activity_probe(
        path,
        waiting_tools=frozenset({"MyWait"}),
        human_facing_tools=frozenset({"MyWait", "MyPrompt"}),
    )
    assert probe.is_waiting is True
    assert probe.mid_tool is False
    assert probe.pending == (PendingItem("w1", "MyWait", "waiting_tool"),)


@requires_rust
@rust_not_disabled
def test_execute_alias_background_is_waiting(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A backgrounded Execute (Bash's alias) classifies like a backgrounded Bash on both backends."""
    path = write(
        tmp_path, [user("build it"), tool_use("Execute", "e1", {"command": "make", "run_in_background": True})]
    )
    via_rust = session_activity_probe(path)
    assert via_rust.is_waiting is True
    assert via_rust.pending == (PendingItem("e1", "Execute", "background"),)
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    assert session_activity_probe(path) == via_rust


@requires_rust
@rust_not_disabled
def test_mcp_prefixed_waiting_tool_is_waiting(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An mcp__<server>__ spelling of a configured waiting tool matches on both backends."""
    path = write(tmp_path, [user("ping the pool"), tool_use("mcp__pool__SendMessage", "s1", {"text": "hi"})])
    via_rust = session_activity_probe(path, waiting_tools=frozenset({"SendMessage"}))
    assert via_rust.is_waiting is True
    assert via_rust.pending == (PendingItem("s1", "mcp__pool__SendMessage", "waiting_tool"),)
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    assert session_activity_probe(path, waiting_tools=frozenset({"SendMessage"})) == via_rust


@requires_rust
@rust_not_disabled
def test_mcp_prefixed_human_facing_tool_is_not_mid_tool(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An mcp__<server>__ spelling of a human-facing tool is the user's move, never mid-tool, on both backends."""
    path = write(tmp_path, [user("choose"), tool_use("mcp__someserver__AskUserQuestion", "q1", {"questions": []})])
    via_rust = session_activity_probe(path)
    assert via_rust.mid_tool is False
    assert via_rust.is_waiting is False
    assert via_rust.pending == ()
    monkeypatch.setenv("CC_TRANSCRIPT_DISABLE_RUST", "1")
    assert session_activity_probe(path) == via_rust


@requires_rust
@rust_not_disabled
@pytest.mark.parametrize("path", real_corpus(), ids=lambda p: f"{p.parent.name}/{p.name}")
def test_real_corpus_probe_parity(path: Path) -> None:
    probe = session_activity_probe(path)
    events = parse_events_from_bytes(path.read_bytes())
    assert reference_is_waiting(events) == probe.is_waiting, f"is_waiting diverged for {path}"
    assert probe_events(events) == probe, f"python twin diverged for {path}"
