from __future__ import annotations

from typing import TYPE_CHECKING, Any

import orjson
import pytest

from cc_transcript.activity_probe import PendingItem, session_activity_probe
from tests.support import requires_rust

if TYPE_CHECKING:
    from pathlib import Path


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
@pytest.mark.parametrize(("lines", "is_waiting", "mid_tool", "pending"), CASES)
def test_fixture_probe(
    tmp_path: Path,
    lines: list[dict[str, Any]],
    is_waiting: bool,
    mid_tool: bool,
    pending: tuple[PendingItem, ...],
) -> None:
    probe = session_activity_probe(write(tmp_path, lines))
    assert probe.is_waiting == is_waiting
    assert probe.mid_tool == mid_tool
    assert probe.pending == pending


@requires_rust
def test_empty_session_is_idle(tmp_path: Path) -> None:
    probe = session_activity_probe(write(tmp_path, []))
    assert probe.is_waiting is False
    assert probe.mid_tool is False
    assert probe.pending == ()
    assert probe.last_event_epoch is None


@requires_rust
def test_last_event_epoch_is_max_meta_timestamp(tmp_path: Path) -> None:
    path = write(tmp_path, [user("hi"), tool_use("Bash", "b1", {"command": "ls"}), queue_op("no meta here")])
    assert session_activity_probe(path).last_event_epoch == 1767323046


@requires_rust
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
def test_execute_alias_background_is_waiting(tmp_path: Path) -> None:
    """A backgrounded Execute (Bash's alias) classifies like a backgrounded Bash."""
    path = write(
        tmp_path, [user("build it"), tool_use("Execute", "e1", {"command": "make", "run_in_background": True})]
    )
    probe = session_activity_probe(path)
    assert probe.is_waiting is True
    assert probe.pending == (PendingItem("e1", "Execute", "background"),)


@requires_rust
def test_mcp_prefixed_waiting_tool_is_waiting(tmp_path: Path) -> None:
    """An mcp__<server>__ spelling of a configured waiting tool matches."""
    path = write(tmp_path, [user("ping the pool"), tool_use("mcp__pool__SendMessage", "s1", {"text": "hi"})])
    probe = session_activity_probe(path, waiting_tools=frozenset({"SendMessage"}))
    assert probe.is_waiting is True
    assert probe.pending == (PendingItem("s1", "mcp__pool__SendMessage", "waiting_tool"),)


@requires_rust
def test_mcp_prefixed_human_facing_tool_is_not_mid_tool(tmp_path: Path) -> None:
    """An mcp__<server>__ spelling of a human-facing tool is the user's move, never mid-tool."""
    path = write(tmp_path, [user("choose"), tool_use("mcp__someserver__AskUserQuestion", "q1", {"questions": []})])
    probe = session_activity_probe(path)
    assert probe.mid_tool is False
    assert probe.is_waiting is False
    assert probe.pending == ()
