"""Direct coverage for the two events-in render bindings.

``render_tool_call_view`` renders an already-parsed native call view (no re-parse); it
must agree with the ``_native.render_tool_call`` JSON gateway for the same call, and it
must reject a pure-Python :class:`FallbackCall` — which is not a native view — with a
``TypeError`` (the facade dispatches ``FallbackCall`` down its own JSON arm). The stub
therefore advertises only ``tools.ToolCall``. ``render_turn_from_events`` renders a turn
from borrowed event views and must match the ``render_turn`` reference over the same
prompt and events, at both a generous and a clipped budget.
"""

from __future__ import annotations

import json

import pytest

from datetime import datetime

from cc_transcript import _native
from cc_transcript.activity import SessionActivity
from cc_transcript.render import Budget, render_tool_call, render_turn
from cc_transcript.tools import FallbackCall, parse_tool_call
from tests import testkit
from tests.support import SESSION, assistant, requires_rust, user


@requires_rust
@pytest.mark.parametrize(
    ("name", "input"),
    [
        pytest.param("Bash", {"command": "ls -la"}, id="typed"),
        pytest.param("TotallyUnknownTool", {"foo": "bar", "n": 3}, id="other_kind"),
    ],
)
def test_render_tool_call_view_matches_json_arm(name: str, input: dict[str, object]) -> None:
    call = parse_tool_call(name, input)
    turn_chars, tool_chars = 700, 1500
    assert _native.render_tool_call_view(call, turn_chars, tool_chars) == _native.render_tool_call(
        name, json.dumps(input), turn_chars, tool_chars
    )


@requires_rust
def test_render_tool_call_view_rejects_fallback_call() -> None:
    fallback = parse_tool_call("Bash", {"command": b"ls"}, on_error="other")
    assert isinstance(fallback, FallbackCall)
    with pytest.raises(TypeError):
        _native.render_tool_call_view(fallback, 700, 1500)


@requires_rust
@pytest.mark.parametrize(
    ("name", "input", "expected"),
    [
        pytest.param("Bash", {"command": b"ls"}, "b'ls'", id="bytes-command"),
        pytest.param(
            "Frobnicate",
            {"widget": datetime(2024, 1, 1)},
            'Frobnicate({"widget":"2024-01-01 00:00:00"})',
            id="datetime-arg",
        ),
    ],
)
def test_render_tool_call_renders_non_json_fallback_raw(name: str, input: dict[str, object], expected: str) -> None:
    call = parse_tool_call(name, input, on_error="other")
    assert isinstance(call, FallbackCall)
    assert render_tool_call(call, budget=Budget()) == expected


@requires_rust
@pytest.mark.parametrize(
    "budget",
    [
        pytest.param(Budget(turn_chars=5000, tool_chars=5000), id="generous"),
        pytest.param(Budget(turn_chars=10, tool_chars=8), id="clipped"),
    ],
)
def test_render_turn_from_events_matches_render_turn(budget: Budget) -> None:
    events = [
        user("u0", "please refactor the parser"),
        assistant("a0", "working on the refactor now", secs=1),
        assistant("a1", blocks=(testkit.tool_use("t1", "Bash", {"command": "pytest --maxfail=1 -q"}),), secs=2),
        assistant("a2", "   ", secs=3),
        user("u1", "", blocks=(testkit.tool_result("t1", "ok"),), secs=4),
    ]
    turn = SessionActivity.from_events(SESSION, events).turns[0]
    assert _native.render_turn_from_events(
        turn.prompt, list(turn.events), budget.turn_chars, budget.tool_chars
    ) == render_turn(turn, budget=budget)
