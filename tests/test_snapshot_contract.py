from __future__ import annotations

import pytest
from pydantic import ValidationError

from scripts.snapshot_contract import MAX_OBSERVERS, REQUEST, RESPONSE, Usage

PRIMARY_ONLY_PREDICATES = [
    pytest.param({"kind": "has_command_regex", "pattern": "git", "subagents": True}, id="regex"),
    pytest.param({"kind": "has_edit_to", "values": ["*.rs"], "subagents": True}, id="edit-glob"),
    pytest.param({"kind": "has_read_glob", "values": ["*.rs"], "subagents": True}, id="read-glob"),
    pytest.param({"kind": "deep_predicate_inputs", "order": "forward"}, id="inputs"),
]


def query_with_attachments(count: int) -> dict[str, object]:
    return {
        "schema": "cc-transcript.snapshot/1",
        "id": "attachments",
        "operation": "query",
        "view": {
            "handle": {
                "owner_epoch": "owner",
                "snapshot_id": "snapshot",
                "generation": "generation",
                "lease_id": "lease",
            },
            "classifier": {"id": "native", "version": "1"},
            "selectors": [],
            "attachments": [f"/sessions/{index}.jsonl" for index in range(count)],
        },
        "query": {"kind": "has_tool", "pattern": "Read", "subagents": False},
        "deadline_unix_ms": 1,
        "limits": {
            "max_read_bytes": 1,
            "max_source_read_bytes": 1,
            "max_events": 1,
            "max_items": 1,
            "max_output_bytes": 1,
            "max_discovery_entries": 1,
            "max_sources": 1,
        },
    }


def prepare_with_threads(count: int) -> dict[str, object]:
    payload = query_with_attachments(0)
    payload["operation"] = "prepare_graph"
    payload.pop("query")
    payload["thread_ids"] = [f"thread-{index}" for index in range(count)]
    payload["roots"] = ["/sessions"]
    payload["direct_paths"] = []
    return payload


def deep_query(**fields: object) -> dict[str, object]:
    return (
        {key: value for key, value in query_with_attachments(0).items() if key != "view"}
        | {
            "operation": "query_graph",
            "handle": {"graph_id": "graph", "owner_epoch": "epoch", "revision": "revision", "complete": True},
            "selectors": [],
            "query": {"kind": "has_tool", "pattern": "Read", "subagents": True},
        }
        | fields
    )


def scalar_response(data: dict[str, object]) -> dict[str, object]:
    return {
        "schema": "cc-transcript.snapshot/1",
        "id": "observed",
        "status": "ok",
        "complete": True,
        "data": data,
        "cursor": None,
        "reason": None,
        "usage": dict.fromkeys(Usage.model_fields, 0),
    }


def test_prepared_registry_accepts_a_registry_past_1024_threads_and_a_window_cutoff() -> None:
    prepared = REQUEST.validate_python(prepare_with_threads(1281) | {"active_since_unix_ms": 1})
    assert (len(prepared.thread_ids), prepared.active_since_unix_ms) == (1281, 1)
    with pytest.raises(ValidationError, match="active_since_unix_ms"):
        REQUEST.validate_python(prepare_with_threads(1) | {"active_since_unix_ms": 0})


def test_limits_require_a_source_read_budget() -> None:
    limits = {
        "max_read_bytes": 1,
        "max_events": 1,
        "max_items": 1,
        "max_output_bytes": 1,
        "max_discovery_entries": 1,
        "max_sources": 1,
    }
    with pytest.raises(ValidationError, match="max_source_read_bytes"):
        REQUEST.validate_python(query_with_attachments(0) | {"limits": limits})


def test_ordinary_query_cannot_carry_attachment_paths() -> None:
    with pytest.raises(ValidationError, match="attachments"):
        REQUEST.validate_python(query_with_attachments(1))


def test_query_cannot_select_background_work_class() -> None:
    payload = query_with_attachments(0)
    payload["work_class"] = "background"
    with pytest.raises(ValidationError, match="work_class"):
        REQUEST.validate_python(payload)


def test_deep_query_requires_complete_prepared_handle() -> None:
    payload = query_with_attachments(0)
    payload.pop("view")
    payload["operation"] = "query_graph"
    payload["selectors"] = []
    payload["query"] = {"kind": "has_tool", "pattern": "Read", "subagents": True}
    with pytest.raises(ValidationError, match="handle"):
        REQUEST.validate_python(payload)
    payload["handle"] = {"graph_id": "graph", "owner_epoch": "epoch", "revision": "revision", "complete": True}
    assert REQUEST.validate_python(payload).handle.graph_id == "graph"


def test_deep_query_carries_observers_of_every_scoped_scalar_kind() -> None:
    observers = [
        {"kind": "has_tool", "pattern": "Edit", "subagents": True},
        {"kind": "has_read", "pattern": "a.rs", "subagents": True},
        {"kind": "has_command", "values": ["git", "push"], "subagents": True},
        {"kind": "has_skill", "values": ["review"], "subagents": True},
        {"kind": "has_skill_suffix", "values": ["review"], "subagents": True},
        {"kind": "has_override", "token": "override", "invalidated_by": ["Edit"], "subagents": True},
        {"kind": "has_error", "subagents": True},
        {"kind": "has_edit", "subagents": True},
    ]
    query = REQUEST.validate_python(deep_query(observers=observers))
    assert [observer.kind for observer in query.observers] == [observer["kind"] for observer in observers]


@pytest.mark.parametrize("payload", [deep_query(), deep_query(observers=None)], ids=["absent", "null"])
def test_deep_query_observers_are_optional(payload: dict[str, object]) -> None:
    assert REQUEST.validate_python(payload).observers is None


@pytest.mark.parametrize("observer", PRIMARY_ONLY_PREDICATES)
def test_deep_query_rejects_an_observer_that_cannot_ride_a_traversal(observer: dict[str, object]) -> None:
    with pytest.raises(ValidationError, match="observers"):
        REQUEST.validate_python(deep_query(observers=[observer]))


@pytest.mark.parametrize("count", [0, 33], ids=["empty", "over-limit"])
def test_deep_query_bounds_its_observer_list(count: int) -> None:
    with pytest.raises(ValidationError, match="observers"):
        REQUEST.validate_python(deep_query(observers=[{"kind": "has_error", "subagents": True}] * count))


def test_deep_query_accepts_the_observer_limit() -> None:
    observers = [{"kind": "has_error", "subagents": True}] * MAX_OBSERVERS
    assert len(REQUEST.validate_python(deep_query(observers=observers)).observers) == 32


@pytest.mark.parametrize("query", PRIMARY_ONLY_PREDICATES)
def test_observer_ineligible_predicate_remains_a_primary_deep_query(query: dict[str, object]) -> None:
    assert REQUEST.validate_python(deep_query(query=query)).query.kind == query["kind"]


def test_observed_scalar_reports_one_outcome_per_observer() -> None:
    observed = {"kind": "observed_scalar", "value": False, "observers": [True, False, None]}
    assert RESPONSE.validate_python(scalar_response(observed)).data.observers == [True, False, None]
    assert RESPONSE.validate_python(scalar_response({"kind": "scalar", "value": False})).data.value is False


def test_observed_scalar_requires_an_outcome() -> None:
    with pytest.raises(ValidationError, match="observers"):
        RESPONSE.validate_python(scalar_response({"kind": "observed_scalar", "value": True, "observers": []}))
